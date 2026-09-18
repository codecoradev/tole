//! D2 (issue #95): `tole acp` — an **Agent Client Protocol** host.
//!
//! Editors and ACP-capable clients (Zed et al.) launch `tole acp` and
//! drive a durable tole session over line-delimited JSON-RPC on
//! stdio/stderr:
//!
//! - `initialize` → protocol + capability handshake
//! - `session/new` / `session/load` → a durable JSONL session (workspace
//!   jail rooted at the client-provided cwd)
//! - `session/prompt` → one full tole turn; the final answer is delivered
//!   as an `agent_message_chunk` update before the response (v1 has no
//!   intra-turn streaming — the turn loop is synchronous; the response is
//!   correct, just not incremental)
//! - Write/Destructive tool calls surface as
//!   `session/request_permission` **requests to the editor** — the human
//!   in the editor is the approver, which is exactly tole's gate with a
//!   different face. Destructive tools therefore CAN be exposed here
//!   (unlike MCP server mode): consent is structurally a human decision.
//!
//! Wire hygiene: protocol messages go to stdout; all diagnostics go to
//! stderr. No new dependencies — the JSON-RPC framing is hand-rolled
//! line-delimited JSON (serde_json only).

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tole_cli::approver::{InteractiveApprover, PromptFn};
use tole_cli::tools::WriteFileTool;

const ACP_PROTOCOL_VERSION: u32 = 1;
/// Permission requests can sit in an editor until a human clicks; do not
/// turn that into a timeout race.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Connection: outgoing lines + response routing
// ---------------------------------------------------------------------------

/// Shared connection state: outgoing JSON-RPC lines are pushed to the
/// writer thread; responses to agent-initiated requests (permission) are
/// routed back to whoever is waiting on them.
#[derive(Clone)]
struct Conn {
    tx: mpsc::Sender<String>,
    next_id: Arc<Mutex<u64>>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
}

impl Conn {
    fn new(tx: mpsc::Sender<String>) -> Self {
        Self {
            tx,
            next_id: Arc::new(Mutex::new(1)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn send_line(&self, line: &str) {
        if self.tx.send(format!("{line}\n")).is_err() {
            // Client is gone; the pending waits below will time out and
            // the session loop winds down on EOF anyway.
        }
    }

    fn send_notification(&self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        self.send_line(&line);
    }

    /// Agent-initiated request (permission): returns the client's result
    /// object, or an error-shaped object on timeout/cancel.
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = {
            let mut n = self.next_id.lock().expect("id lock");
            let id = *n;
            *n += 1;
            id
        };
        let (tx, rx) = mpsc::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.send_line(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        match rx.recv_timeout(timeout) {
            Ok(v) => Ok(v),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err("client did not answer the permission request in time".into())
            }
        }
    }

    /// Reader-side routing: a response line completes a pending agent
    /// request.
    fn route_response(&self, id: u64, result: Value) {
        if let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) {
            let _ = tx.send(result);
        }
    }
}

// ---------------------------------------------------------------------------
// Approval: the editor is the human
// ---------------------------------------------------------------------------

/// PromptFn bridge: renders a `session/request_permission` request to the
/// ACP client and maps the chosen option back to a tole verdict.
struct AcpPrompt {
    conn: Conn,
    session_id: String,
    counter: Arc<Mutex<u64>>,
}

impl PromptFn for AcpPrompt {
    fn prompt(&self, req: &tole_core::approval::ToolRequest<'_>) -> tole_core::approval::Verdict {
        use tole_core::approval::Verdict;
        let option_allow = "allow-once";
        let option_reject = "reject-once";
        let call_id = {
            let mut n = self.counter.lock().expect("call id lock");
            *n += 1;
            format!("call-{n}")
        };
        // Announce the tool call first so editors can render it.
        self.conn.send_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "title": req.description,
                    "kind": "other",
                    "rawInput": req.input,
                }
            }),
        );
        let params = json!({
            "sessionId": self.session_id,
            "toolCall": {
                "toolCallId": call_id,
                "title": req.description,
                "kind": "other",
                "rawInput": req.input,
            },
            "options": [
                {"kind": "allow_once", "name": "Allow", "optionId": option_allow},
                {"kind": "reject_once", "name": "Reject", "optionId": option_reject},
            ],
        });
        let verdict =
            match self
                .conn
                .request("session/request_permission", params, PERMISSION_TIMEOUT)
            {
                Ok(result) => {
                    let chosen = result["outcome"]["optionId"]
                        .as_str()
                        .unwrap_or(option_reject);
                    if chosen == option_allow {
                        Verdict::Allow
                    } else {
                        Verdict::Deny
                    }
                }
                Err(_) => {
                    // Cancelled or timed out: fail closed.
                    Verdict::Deny
                }
            };
        // Close the tool-call record.
        self.conn.send_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": if verdict == Verdict::Allow { "completed" } else { "rejected" },
                }
            }),
        );
        verdict
    }
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

struct SessionState {
    storage: tole_core::storage::JsonlStorage,
    registry: tole_core::tool::ToolRegistry,
    system_prompt: Option<String>,
    memory: Option<tole_core::memory::MemoryConfig>,
    first_prompt_done: bool,
}

struct Sessions {
    map: HashMap<String, SessionState>,
}

impl Sessions {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// ACP server loop
// ---------------------------------------------------------------------------

/// Run the ACP agent over stdin/stdout. Blocks until the client closes
/// stdin.
pub fn run_acp(
    allow_patterns: &[String],
    auto_write: bool,
    _workspace_default: Option<&String>,
    memory: Option<tole_core::memory::MemoryConfig>,
) -> Result<()> {
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let conn = Conn::new(line_tx.clone());

    // Writer thread: stdout belongs to the protocol.
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line in line_rx {
            if out.write_all(line.as_bytes()).is_err() {
                break;
            }
            let _ = out.flush();
        }
    });

    // The session map lives for the WHOLE server lifetime and is shared
    // with prompt threads (Arc clone per prompt). Holding the map lock
    // for the duration of a turn also serializes access to one session's
    // storage — session/load of a busy id simply waits its turn instead
    // of opening a divergent second handle.
    let sessions: SharedSessions = StdArc::new(Mutex::new(Sessions::new()));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // non-JSON noise on the protocol channel: ignore
        };
        let id = msg.get("id").cloned();
        let Some(method) = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            // A response to one of OUR requests (permission).
            if let (Some(id), Some(result)) = (
                msg.get("id").and_then(Value::as_u64),
                msg.get("result").cloned(),
            ) {
                conn.route_response(id, result);
            }
            continue;
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        match method.as_str() {
            "initialize" => {
                reply(
                    &conn,
                    id,
                    json!({
                        "protocolVersion": ACP_PROTOCOL_VERSION,
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {},
                        },
                        "authMethods": [],
                    }),
                );
            }
            "session/new" | "session/load" => {
                let loading = method == "session/load";
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or(".")
                    .to_string();
                let session_id = match params.get("sessionId").and_then(Value::as_str) {
                    Some(id) => validate_session_id(id),
                    None => Some(new_session_id()),
                };
                let Some(session_id) = session_id else {
                    reply_error(&conn, id, "session/load: invalid sessionId");
                    continue;
                };
                match open_session(
                    &session_id,
                    &cwd,
                    loading,
                    allow_patterns,
                    auto_write,
                    memory.clone(),
                    conn.clone(),
                ) {
                    Ok(state) => {
                        lock_sessions(&sessions)
                            .map
                            .insert(session_id.clone(), state);
                        reply(&conn, id, json!({ "sessionId": session_id }));
                    }
                    Err(e) => reply_error(&conn, id, &e.to_string()),
                }
            }
            "session/prompt" => {
                let Some(session_id) = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .map(|id| validate_session_id(&id))
                else {
                    reply_error(&conn, id, "session/prompt: missing sessionId");
                    continue;
                };
                let Some(session_id) = session_id else {
                    reply_error(&conn, id, "session/prompt: invalid sessionId");
                    continue;
                };
                // ACP sends `prompt` as an array of content blocks; a
                // plain string is accepted for client convenience.
                let prompt_text = match params.get("prompt") {
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(""),
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        reply_error(&conn, id, "session/prompt: missing prompt");
                        continue;
                    }
                };
                // Response is delivered from the turn thread (result or
                // error) so this reader loop stays live for permission
                // requests while the turn runs.
                let conn = conn.clone();
                let sessions = sessions.clone();
                let prompt_clone = prompt_text;
                let session_id_clone = session_id.clone();
                std::thread::spawn(move || {
                    let result =
                        run_prompt(sessions, &session_id_clone, &prompt_clone, conn.clone());
                    match result {
                        Ok(stop) => reply(&conn, id, json!({ "stopReason": stop })),
                        Err(e) => reply_error(&conn, id, &e.to_string()),
                    }
                });
            }
            other => {
                if id.is_some() {
                    reply_error(&conn, id, &format!("method not supported: {other}"));
                }
            }
        }
    }
    Ok(())
}

use std::sync::Arc as StdArc;
type SharedSessions = StdArc<Mutex<Sessions>>;

/// Poisoning-tolerant lock: one panicking turn must not brick the whole
/// ACP server (CodeCora scan finding — mutex poisoning).
fn lock_sessions(sessions: &SharedSessions) -> std::sync::MutexGuard<'_, Sessions> {
    sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reply(conn: &Conn, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    conn.send_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
}

fn reply_error(conn: &Conn, id: Option<Value>, message: &str) {
    let Some(id) = id else { return };
    conn.send_line(
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}})
            .to_string(),
    );
}

/// ACP session ids are tole session ids: reject path separators,
/// parent refs, and anything outside the tole charset before the id ever
/// touches a path (CodeCora scan finding: `../` or absolute ids would
/// escape the sessions dir via Path::join).
fn validate_session_id(id: &str) -> Option<String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok {
        Some(id.to_string())
    } else {
        None
    }
}

fn new_session_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("acp-{ms:x}-{:x}", std::process::id())
}

/// Create/open a session: workspace jail = the client's cwd; approval =
/// the ACP editor (interactive — which is what unlocks Destructive tools
/// with genuine human consent).
#[allow(clippy::too_many_arguments)]
fn open_session(
    session_id: &str,
    cwd: &str,
    loading: bool,
    allow_patterns: &[String],
    auto_write: bool,
    memory: Option<tole_core::memory::MemoryConfig>,
    conn: Conn,
) -> Result<SessionState> {
    let workspace = PathBuf::from(cwd);
    let workspace_canon = workspace
        .canonicalize()
        .with_context(|| format!("session workspace {}: {cwd}", workspace.display()))?;
    let system_prompt = std::env::var("TOLE_SYSTEM_PROMPT")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let mut reg = tole_core::tool::ToolRegistry::with_approver(
        InteractiveApprover::new(AcpPrompt {
            conn,
            session_id: session_id.to_string(),
            counter: Arc::new(Mutex::new(0)),
        })
        .with_allow_patterns(allow_patterns.to_vec())
        .with_auto_write(auto_write),
    );
    use tole_core::cora_search::CoraSearchTool;
    use tole_core::file_tools::{DeleteFileTool, EditFileTool};
    use tole_core::gh::GhTool;
    use tole_core::git::GitTool;
    use tole_core::jobs::{JobPollTool, JobStartTool};
    use tole_core::read_file::ReadFileTool;
    use tole_core::run_command::RunCommandTool;
    use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};
    if tole_cli_binary_available("cora") {
        reg.register(Box::new(CoraSearchTool::new()))
            .map_err(anyhow::Error::msg)?;
    }
    if tole_cli_binary_available("uteke") {
        reg.register(Box::new(UtekeRecallTool::new()))
            .map_err(anyhow::Error::msg)?;
        reg.register(Box::new(UtekeDocumentTool::new(None)))
            .map_err(anyhow::Error::msg)?;
    }
    reg.register(Box::new(RunCommandTool::new(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;
    reg.register(Box::new(JobStartTool::new(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;
    reg.register(Box::new(JobPollTool::new(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;
    reg.register(Box::new(ReadFileTool::new(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;
    reg.register(Box::new(WriteFileTool::new(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;
    reg.register(Box::new(EditFileTool::new(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;
    {
        let repo =
            detect_github_repo(&workspace_canon).unwrap_or_else(|| "codecoradev/tole".into());
        reg.register(Box::new(GhTool::new(repo)))
            .map_err(anyhow::Error::msg)?;
    }
    reg.register(Box::new(GitTool::new().in_dir(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;
    // delete_file IS registered here: the ACP editor prompt is an
    // interactive approver, so a Destructive tool carries genuine human
    // consent — the same rule as the CLI, not an exception.
    reg.register(Box::new(DeleteFileTool::new(workspace_canon.clone())))
        .map_err(anyhow::Error::msg)?;

    let storage = if loading {
        let dir = sessions_dir_for(cwd)?;
        tole_core::storage::JsonlStorage::open(dir.join(format!("{session_id}.jsonl")))
            .context("loading session")?
    } else {
        let dir = sessions_dir_for(cwd)?;
        std::fs::create_dir_all(&dir)?;
        tole_core::storage::JsonlStorage::create_with(
            &dir,
            session_id,
            None,
            system_prompt.as_deref(),
        )
        .context("creating session")?
    };
    Ok(SessionState {
        storage,
        registry: reg,
        system_prompt,
        memory,
        first_prompt_done: loading,
    })
}

fn sessions_dir_for(cwd: &str) -> Result<PathBuf> {
    let dir = PathBuf::from(cwd).join(".tole/sessions");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn tole_cli_binary_available(name: &str) -> bool {
    if name.contains('/') {
        return std::path::Path::new(name).is_file();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join(name).is_file() {
                return true;
            }
        }
    }
    false
}

fn detect_github_repo(cwd: &PathBuf) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd);
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    crate::github_repo_from_remote_url(&String::from_utf8_lossy(&out.stdout))
}

/// One ACP prompt = one full tole turn. Returns the stop reason.
fn run_prompt(
    sessions: SharedSessions,
    session_id: &str,
    prompt: &str,
    conn: Conn,
) -> Result<String> {
    // Poisoning-tolerant: see lock_sessions.
    let mut sessions = lock_sessions(&sessions);
    let Some(state) = sessions.map.get_mut(session_id) else {
        anyhow::bail!("unknown session: {session_id}");
    };

    // Memory loop, pre-turn (first prompt of a fresh session only).
    let mut effective = prompt.to_string();
    #[cfg(feature = "shell-tools")]
    if let Some(mem) = state.memory.clone() {
        if !state.first_prompt_done {
            if let Ok(block) = tole_core::memory::recall_block(&mem, prompt) {
                if !block.is_empty() {
                    eprintln!("tole acp: memory: recalled context injected");
                    effective = format!("{prompt}{block}");
                }
            }
        }
    }

    // Provider: built per turn (cheap), so a session can be created
    // without provider env and only fail when a turn is actually run.
    let cfg = tole_core::openai::OpenAiConfig::from_env().context(
        "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY \
         (or the OPENAI_* equivalents)",
    )?;
    let mut provider =
        tole_core::openai::OpenAiProvider::new(cfg).with_tool_specs(state.registry.specs());
    if let Some(sys) = state.system_prompt.as_deref() {
        provider = provider.with_system_prompt(sys);
    }

    let outcome = tole_core::turn::run_turn(
        &mut state.storage,
        &mut provider,
        &state.registry,
        &effective,
    )?;

    #[cfg(feature = "shell-tools")]
    if let tole_core::turn::TurnOutcome::Final { text } = &outcome {
        if let Some(mem) = state.memory.clone() {
            let _ = tole_core::memory::remember_session(&mem, session_id, prompt, text);
        }
    }
    state.first_prompt_done = true;

    let stop = match &outcome {
        tole_core::turn::TurnOutcome::Final { text } => {
            conn.send_notification(
                "session/update",
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": text},
                    }
                }),
            );
            "end_turn"
        }
        tole_core::turn::TurnOutcome::ApprovalRequired { name } => {
            eprintln!("tole acp: approval denied for '{name}'");
            "refusal"
        }
        tole_core::turn::TurnOutcome::UnknownTool { name } => {
            eprintln!("tole acp: unknown tool '{name}'");
            "refusal"
        }
        tole_core::turn::TurnOutcome::ProviderFailed { message } => {
            eprintln!("tole acp: provider failed: {message}");
            "refusal"
        }
        tole_core::turn::TurnOutcome::BudgetExhausted => {
            eprintln!("tole acp: step budget exhausted");
            "max_tokens"
        }
        tole_core::turn::TurnOutcome::LoopDetected { tool, .. } => {
            eprintln!("tole acp: loop detected on '{tool}'");
            "refusal"
        }
        tole_core::turn::TurnOutcome::Storage(e) => {
            anyhow::bail!("storage error: {e}");
        }
    };
    Ok(stop.to_string())
}
