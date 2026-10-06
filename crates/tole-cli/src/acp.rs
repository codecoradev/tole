//! D2 (issue #95): `tole acp` — an **Agent Client Protocol** host.
//!
//! Editors and ACP-capable clients (Zed et al.) launch `tole acp` and
//! drive a durable tole session over line-delimited JSON-RPC on
//! stdin/stderr:
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
//!
//! The session machinery lives in [`tole_cli::session_host`] — one
//! implementation shared with `tole serve`.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tole_cli::approver::{InteractiveApprover, PromptFn};
use tole_cli::session_host::{
    lock_sessions, new_session_id, open_session, run_session_turn, validate_session_id, Sessions,
};
use tole_core::storage::Storage;

const ACP_PROTOCOL_VERSION: u32 = 1;
/// Permission requests can sit in an editor until a human clicks; do not
/// turn that into a timeout race.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(600);

/// Durable per-session model override (issue #176): a `fact/model`
/// register write. Registers are the sanctioned append-only mutable
/// state, so the override survives `session/load` in a fresh process
/// and `run_session_turn` (shared with `tole serve`) applies it when
/// building the provider.
const MODEL_REGISTER: (&str, &str) = ("fact", "model");

// ---------------------------------------------------------------------------
// Model picker: ACP session config options (issue #176)
// ---------------------------------------------------------------------------

/// Parse the `TOLE_MODELS` env into the advertised model list: comma
/// separated, trimmed, empties dropped, duplicates collapsed, order kept.
/// Empty result = no picker advertised (today's behavior).
pub(crate) fn parse_model_list(env_val: Option<&str>) -> Vec<String> {
    let Some(raw) = env_val else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for id in raw.split(',') {
        let id = id.trim();
        if !id.is_empty() && !out.iter().any(|m| m == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// Model-list resolution precedence (issue #195): a non-empty
/// `TOLE_MODELS` wins AS-IS — deterministic, no network, the operator's
/// explicit override/filter. Empty or unset falls to `probe` (the
/// provider's `GET /models`); its result is used whatever it is,
/// including empty (a failed probe is cached by the caller — never
/// retried per session).
fn resolve_model_list(env_val: Option<&str>, probe: impl FnOnce() -> Vec<String>) -> Vec<String> {
    let from_env = parse_model_list(env_val);
    if !from_env.is_empty() {
        return from_env;
    }
    probe()
}

/// The full advertised option list: the `TOLE_MODELS` entries plus the
/// session's current model (prepended when missing — the spec requires
/// `currentValue` to be one of the options so the picker can render it).
fn advertised_models(current: &str, models: &[String]) -> Vec<String> {
    if current.is_empty() || models.iter().any(|m| m == current) {
        models.to_vec()
    } else {
        let mut all = vec![current.to_string()];
        all.extend(models.iter().cloned());
        all
    }
}

/// Build the `configOptions` entry for the model picker, or `None` when
/// there is nothing to advertise (no `TOLE_MODELS` configured).
/// Wire shape per the ACP session-config-options spec: a `select`-kind
/// option with `category: "model"` — Termul/Zed render it as the model
/// dropdown. Select-only: boolean options additionally require the
/// client's `session.configOptions.boolean` capability, which tole does
/// not probe.
pub(crate) fn model_config_option(current: &str, models: &[String]) -> Option<Value> {
    if models.is_empty() {
        return None;
    }
    let all = advertised_models(current, models);
    Some(json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current,
        "options": all
            .iter()
            .map(|m| json!({"value": m, "name": m}))
            .collect::<Vec<_>>(),
    }))
}

/// Build the `configOptions` entry for the approval selector: the
/// per-session `--yes` equivalent, category `mode` so hosts render it
/// next to the model picker. Session-scoped by design — approval
/// consent resets with the process, so it is NOT persisted like the
/// model override.
fn approval_config_option(auto_write: bool) -> Value {
    json!({
        "id": "approval",
        "name": "Approval",
        "category": "mode",
        "type": "select",
        "currentValue": if auto_write { "auto" } else { "ask" },
        "options": [
            {"value": "ask", "name": "Ask",
             "description": "Request permission before Write tools"},
            {"value": "auto", "name": "Auto",
             "description": "Auto-allow Write tools (Destructive still prompts)"},
        ],
    })
}

/// The full `configOptions` response array for a session: the approval
/// selector always, the model picker when configured.
fn config_options_for(
    current_model: &str,
    models: &[String],
    auto_write: bool,
) -> Option<Vec<Value>> {
    // The approval selector is unconditional; an early `?` on the model
    // fallback must not swallow it (found by the no-env E2E).
    let mut opts = vec![approval_config_option(auto_write)];
    if !models.is_empty() {
        let current = if current_model.is_empty() {
            models.first().expect("non-empty").clone()
        } else {
            current_model.to_string()
        };
        if let Some(model) = model_config_option(&current, models) {
            opts.push(model);
        }
    }
    Some(opts)
}

/// Read the durable model override from a session's storage.
fn session_model_override(
    storage: &Arc<Mutex<tole_core::storage::JsonlStorage>>,
) -> Option<String> {
    let storage = storage.lock().unwrap_or_else(|p| p.into_inner());
    storage
        .get_register(MODEL_REGISTER.0, MODEL_REGISTER.1)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Per-session approval state, addressable by session id so
/// `session/set_config_option` can act without touching the storage
/// mutex (the reader thread must never block on a running turn). The
/// auto-write handle is the same `Arc` the session's
/// `InteractiveApprover` owns; the effective model mirrors what
/// `session/new` computed (durable override or env default) and is
/// updated by `set_model_option`, so every config-state reply renders
/// the CURRENT selection (cora MAJOR, PR 2: an approval flip must not
/// show a stale model). The pattern store stays bridge-owned
/// (`AcpPrompt`) — grants land there.
#[derive(Clone)]
struct SessionApprovalState {
    auto_write: Arc<Mutex<bool>>,
    current_model: Arc<Mutex<String>>,
}

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
            // Client is gone; the pending wait below will time out and
            // the session loop winds down on EOF anyway.
        }
    }

    fn send_notification(&self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        self.send_line(&line);
    }

    /// Send ONE agent-initiated request and hand back its reply
    /// receiver, so the caller can wait in slices and re-check the
    /// turn's cancellation token between slices (issue #178) — calling
    /// [`Conn::request`] repeatedly would re-send duplicate wire
    /// requests. Pair with [`Conn::abandon`] when giving up.
    fn open_request(&self, method: &str, params: Value) -> (u64, mpsc::Receiver<Value>) {
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
        (id, rx)
    }

    /// Drop a pending request entry (fail-closed path). A late client
    /// reply then finds no waiter and is ignored by `route`.
    fn abandon(&self, id: u64) {
        self.pending.lock().expect("pending lock").remove(&id);
    }

    /// Reader-side routing: a response line completes a pending agent
    /// request. Client ERROR replies route too — an errored permission
    /// must fail closed immediately, not hang for the full timeout
    /// (CodeCora scan 2026-09-28). cora scan 2026-10-01 (#155): a reply
    /// with the matching id but NEITHER field (malformed client) gets
    /// the same immediate fail-closed treatment — otherwise the waiting
    /// thread hangs for the full timeout on garbage input.
    fn route_reply(&self, id: u64, msg: &Value) {
        if let Some(result) = msg.get("result").cloned() {
            self.route(id, result);
        } else if let Some(err) = msg.get("error").cloned() {
            self.route(
                id,
                json!({"outcome": {"outcome": "cancelled"}, "error": err}),
            );
        } else {
            // Malformed reply: fail closed now (cancelled outcome), the
            // requester surfaces the error instead of blocking.
            self.route(
                id,
                json!({"outcome": {"outcome": "cancelled"}, "error": "malformed reply"}),
            );
        }
    }

    fn route(&self, id: u64, result: Value) {
        if let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) {
            let _ = tx.send(result);
        }
    }
}

/// Outcome of a sliced permission wait (issue #178).
enum WaitOutcome {
    /// The client answered.
    Reply(Value),
    /// The window expired, the transport died, or the turn was
    /// cancelled with the request unresolved. The caller fails closed.
    Timeout,
}

/// Wait for ONE pending permission reply in slices, re-checking the
/// turn's cancellation token between slices (issue #178) — the wire
/// request was sent once via [`Conn::open_request`]; only the WAIT is
/// sliced, so a mid-wait `session/cancel` unwinds the approval within
/// ~1 s instead of after the full window. Every unresolved exit
/// abandons the pending entry (late client replies find no waiter and
/// are ignored by `route`).
fn wait_permission(
    conn: &Conn,
    req_id: u64,
    rx: mpsc::Receiver<Value>,
    total: Duration,
    cancel: &tole_core::cancel::CancelToken,
) -> WaitOutcome {
    let deadline = std::time::Instant::now() + total;
    loop {
        if cancel.is_cancelled() {
            conn.abandon(req_id);
            return WaitOutcome::Timeout;
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(v) => return WaitOutcome::Reply(v),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Writer thread gone (client vanished) — the ACP loop
                // winds down on EOF anyway; fail closed now instead of
                // sitting out the window.
                conn.abandon(req_id);
                return WaitOutcome::Timeout;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if std::time::Instant::now() >= deadline {
                    conn.abandon(req_id);
                    return WaitOutcome::Timeout;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Progress observer: tool cards + thought bubbles (issue #196)
// ---------------------------------------------------------------------------

/// ACP `ToolKind` for a tole tool name (issue #196 phase 1): the spec's
/// taxonomy drives host icons and progress rendering. Mapping falls out
/// of the registry vocabulary; "other" stays the fallback for anything
/// unmapped. Registry-gated tools only ever map when actually present —
/// this is a pure name lookup, no registration assumption.
fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read_file" | "tole_session_list" | "tole_session_status" => "read",
        "write_file" | "edit_file" => "edit",
        "delete_file" => "delete",
        "run_command" | "git" | "gh" | "gitea" | "job_start" | "job_poll" => "execute",
        "cora_search" | "uteke_recall" => "search",
        "systemone_decide" | "agent_start" | "agent_poll" => "think",
        "uteke_document" | "tole_session_new" | "tole_session_prompt" => "edit",
        _ => "other",
    }
}

/// The turn-loop observer for the ACP face (issue #196): translates
/// core turn events into `session/update` notifications — a tool card
/// (proper `kind` + `name`) at execution start, its completion with a
/// bounded output preview, and the model's reasoning as an
/// `agent_thought_chunk`. All fire mid-turn, so the host shows live
/// progress instead of a silent "Working…".
struct AcpObserver {
    conn: Conn,
    session_id: String,
    counter: Arc<Mutex<u64>>,
    /// The open card's id (cora MAJOR, #196): `tool_call_update` must
    /// reference the `toolCallId` the started card announced, or hosts
    /// cannot correlate them and the card stays `in_progress` forever.
    /// Tools run sequentially on the turn thread, so one slot suffices.
    current: Mutex<Option<String>>,
}

impl AcpObserver {
    fn next_id(&self) -> String {
        let mut n = self.counter.lock().expect("observer id lock");
        *n += 1;
        format!("tool-{n}")
    }
}

impl tole_core::turn::TurnObserver for AcpObserver {
    fn tool_started(&self, tool: &str, input: &Value) {
        let id = self.next_id();
        *self.current.lock().expect("observer current lock") = Some(id.clone());
        self.conn.send_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": tool,
                    "name": tool,
                    "kind": tool_kind(tool),
                    "rawInput": input,
                    "status": "in_progress",
                }
            }),
        );
    }

    fn tool_finished(&self, _tool: &str, ok: bool, preview: &str) {
        // Correlate with the started card; fall back to a fresh id only
        // if a finished event ever arrives without a started (cannot
        // happen today — every execution passes tool_started first).
        let id = self
            .current
            .lock()
            .expect("observer current lock")
            .take()
            .unwrap_or_else(|| self.next_id());
        self.conn.send_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if ok { "completed" } else { "failed" },
                    "content": [{"type": "content",
                                 "content": {"type": "text", "text": preview}}],
                }
            }),
        );
    }

    fn reasoning(&self, text: &str) {
        self.conn.send_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": text},
                }
            }),
        );
    }
}

// ---------------------------------------------------------------------------
// Approval: the editor is the human
// ---------------------------------------------------------------------------

/// PromptFn bridge: renders a `session/request_permission` request to the
/// ACP client and maps the chosen option back to a tole verdict.
///
/// The shared approval-state handles (issue #176 PR 2) let the human's
/// `allow_always` choice grant the exact tool name for the rest of the
/// session, and the `approval` config option flip auto-write — the same
/// `--allow` / `--yes` mechanisms, granted interactively instead of at
/// startup. Destructive calls never get an `allow_always` option and an
/// echoed one is honored ONE-TIME only (never remembered): Destructive
/// is never allowlistable.
struct AcpPrompt {
    conn: Conn,
    session_id: String,
    counter: Arc<Mutex<u64>>,
    patterns: Arc<Mutex<Vec<String>>>,
    /// Cancellation checkpoint for the session's current turn
    /// (issue #178): a `session/cancel` while a permission request is
    /// pending stops the wait immediately — the spec requires pending
    /// permission requests to settle `cancelled` (fail closed).
    cancel: tole_core::cancel::CancelToken,
}

/// The `allow_always` PermissionOption kind — a standard kind
/// (allow_once | allow_always | reject_once | reject_always) that hosts
/// render with "remember this choice" semantics.
const OPTION_ALLOW_ALWAYS: &str = "allow-always";

impl AcpPrompt {
    fn lock_patterns(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.patterns.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Map the client's chosen PermissionOption back to a tole verdict
    /// (extracted verbatim from the pre-#178 match body).
    fn map_option_choice(
        &self,
        result: &Value,
        req: &tole_core::approval::ToolRequest<'_>,
        option_reject: &str,
    ) -> tole_core::approval::Verdict {
        use tole_core::approval::Verdict;
        use tole_core::tool::Risk;
        let option_allow = "allow-once";
        let chosen = result["outcome"]["optionId"]
            .as_str()
            .unwrap_or(option_reject);
        // Comparison chain, NOT a match: a lowercase match arm would be
        // a fresh binding matching EVERYTHING (a reject would have read
        // as Allow).
        if chosen == option_allow {
            Verdict::Allow
        } else if chosen == OPTION_ALLOW_ALWAYS {
            if req.risk == Risk::Write {
                // Remember for the session: the exact tool name joins
                // the approver's pattern store (glob-matched; exact
                // names match trivially). Not persisted — consent
                // resets with the process.
                self.lock_patterns().push(req.tool.to_string());
            }
            // A Destructive call must never be remembered, even when a
            // hostile client echoes the option: honored one-time only.
            Verdict::Allow
        } else {
            Verdict::Deny
        }
    }
}

impl PromptFn for AcpPrompt {
    fn prompt(&self, req: &tole_core::approval::ToolRequest<'_>) -> tole_core::approval::Verdict {
        use tole_core::approval::Verdict;
        use tole_core::tool::Risk;
        let option_allow = "allow-once";
        let option_reject = "reject-once";
        let call_id = {
            let mut n = self.counter.lock().expect("call id lock");
            *n += 1;
            format!("perm-{n}")
        };
        // The timeline card is the observer's job (issue #196): announce
        // + completion fire from the turn loop for EVERY execution. The
        // permission request stays self-contained — hosts render the
        // dialog from the embedded toolCall, now with the real kind.
        // allow_always only for Write-tier calls: Destructive is never
        // allowlistable, and ReadOnly never reaches the approver.
        let mut options =
            vec![json!({"kind": "allow_once", "name": "Allow", "optionId": option_allow})];
        if req.risk == Risk::Write {
            options.push(json!({
                "kind": "allow_always", "name": "Always allow",
                "optionId": OPTION_ALLOW_ALWAYS,
            }));
        }
        options.push(json!({"kind": "reject_once", "name": "Reject", "optionId": option_reject}));
        let params = json!({
            "sessionId": self.session_id,
            "toolCall": {
                "toolCallId": call_id,
                "title": req.description,
                "name": req.tool,
                "kind": tool_kind(req.tool),
                "rawInput": req.input,
            },
            "options": options,
        });
        let (req_id, rx) = self.conn.open_request("session/request_permission", params);
        match wait_permission(&self.conn, req_id, rx, PERMISSION_TIMEOUT, &self.cancel) {
            WaitOutcome::Reply(result) => self.map_option_choice(&result, req, option_reject),
            // Timeout, dead transport, or a mid-wait cancel: fail
            // closed (Deny) — the routing contract for unresolved
            // permission requests. The already-set turn token makes a
            // cancelled wait settle `cancelled`, not `refusal`.
            WaitOutcome::Timeout => Verdict::Deny,
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
    plan_mode: bool,
    memory: Option<tole_core::memory::MemoryConfig>,
    sessions_dir: Option<std::path::PathBuf>,
    turnend: Vec<String>,
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
    // with prompt threads (Arc clone per prompt). A turn holds only its
    // own per-session storage lock, so this reader loop stays live for
    // permission routing while prompts run.
    let sessions: tole_cli::session_host::SharedSessions =
        Arc::new(Mutex::new(Sessions::default()));
    // Per-session shared approval state (issue #176 PR 2): addressable
    // by session id so set_config_option can flip the session's
    // auto-write while the approver owns the same handles.
    let approval_states: Arc<Mutex<HashMap<String, SessionApprovalState>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // Advertised model list (issues #176/#195): `TOLE_MODELS` wins
    // as-is; otherwise the provider's `GET /models` is probed ONCE on
    // first use (lazy — agent spawn stays instant) and cached for the
    // process lifetime, failures included (one stderr line, no per-
    // session retry). No provider config → nothing to probe.
    let env_models_raw = std::env::var("TOLE_MODELS").ok();
    let probe_cfg = tole_core::openai::OpenAiConfig::from_env();
    let models_cache: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let model_list = || {
        resolve_model_list(env_models_raw.as_deref(), || {
            // Probe at most once per process — failures are cached too,
            // so a broken gateway costs one stderr line, not one retry
            // per session.
            models_cache.get().cloned().unwrap_or_else(|| {
                let computed = match probe_cfg.as_ref() {
                    None => Vec::new(),
                    Some(cfg) => {
                        match tole_core::openai::fetch_model_ids(&cfg.base_url, &cfg.api_key) {
                            Ok(list) if !list.is_empty() => list,
                            Ok(_) => {
                                eprintln!(
                                    "tole acp: provider /models returned no ids — set \
                                         TOLE_MODELS to advertise a model picker"
                                );
                                Vec::new()
                            }
                            Err(e) => {
                                eprintln!(
                                    "tole acp: {e} — set TOLE_MODELS to advertise a \
                                         model picker"
                                );
                                Vec::new()
                            }
                        }
                    }
                };
                let _ = models_cache.set(computed.clone());
                computed
            })
        })
    };
    let env_model = std::env::var("TOLE_MODEL")
        .ok()
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .filter(|m| !m.trim().is_empty());
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
            // A response to one of OUR requests (permission). Client
            // ERROR replies route as well — an errored permission must
            // fail closed immediately, not hang for the full timeout
            // (CodeCora scan 2026-09-28).
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                conn.route_reply(id, &msg);
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
                        "agentInfo": {
                            "name": "tole",
                            "version": env!("CARGO_PKG_VERSION"),
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
                    None => Some(new_session_id("acp")),
                };
                let Some(session_id) = session_id else {
                    reply_error(&conn, id, "session: invalid sessionId");
                    continue;
                };
                // A turn holds its session's storage lock and marks it
                // busy; loading a busy id would open a SECOND handle on
                // the same JSONL mid-turn (CodeCora scan 2026-09-28).
                let busy_now = {
                    let sessions = lock_sessions(&sessions);
                    sessions
                        .map
                        .get(&session_id)
                        .map(|st| *st.busy.lock().expect("busy lock"))
                        .unwrap_or(false)
                };
                if busy_now {
                    reply_error(&conn, id, "session is busy running a turn");
                    continue;
                }
                // Shared approval handles: created here so the bridge
                // (allow_always grants), the approver, and the
                // set_config_option lookup all own the same stores.
                // Seeded with the startup flag values. The cancel token
                // (#178) is created HERE and shared three ways — the
                // prompt bridge (permission wait), the session state
                // (transport-side set), and the turn (checkpoint test).
                let patterns = Arc::new(Mutex::new(allow_patterns.to_vec()));
                let session_auto_write = Arc::new(Mutex::new(auto_write));
                let cancel = tole_core::cancel::CancelToken::new();
                let approver = InteractiveApprover::new(AcpPrompt {
                    conn: conn.clone(),
                    session_id: session_id.clone(),
                    counter: Arc::new(Mutex::new(0)),
                    patterns: Arc::clone(&patterns),
                    cancel: cancel.clone(),
                })
                .with_allow_patterns(allow_patterns.to_vec())
                .with_auto_write(auto_write)
                .with_shared_patterns(Arc::clone(&patterns))
                .with_shared_auto_write(Arc::clone(&session_auto_write));
                match open_session(
                    &session_id,
                    &cwd,
                    loading,
                    plan_mode,
                    approver,
                    memory.clone(),
                    sessions_dir.as_deref(),
                    turnend.clone(),
                    allow_patterns.to_vec(),
                    cancel,
                ) {
                    Ok(state) => {
                        // Insert + busy re-check in ONE critical section:
                        // open_session is slow (canonicalize + a git
                        // subprocess), and a prompt that set busy inside
                        // that window must not be orphaned by the insert
                        // swapping in a fresh, not-busy state (CodeCora
                        // scan 2026-09-28).
                        let mut sessions = lock_sessions(&sessions);
                        let busy_now = sessions
                            .map
                            .get(&session_id)
                            .map(|st| *st.busy.lock().expect("busy lock"))
                            .unwrap_or(false);
                        if busy_now {
                            drop(sessions);
                            reply_error(&conn, id, "session became busy while opening — retry");
                            continue;
                        }
                        // Model picker state (issue #176): the durable
                        // register override wins, else the env default,
                        // else the first advertised entry. Read BEFORE
                        // the state moves into the session map.
                        let model_override = session_model_override(&state.storage);
                        sessions.map.insert(session_id.clone(), state);
                        let current = model_override
                            .or_else(|| env_model.clone())
                            .unwrap_or_default();
                        let session_auto =
                            *session_auto_write.lock().unwrap_or_else(|p| p.into_inner());
                        approval_states.lock().expect("approval map").insert(
                            session_id.clone(),
                            SessionApprovalState {
                                auto_write: session_auto_write,
                                current_model: Arc::new(Mutex::new(current.clone())),
                            },
                        );
                        let mut result = json!({ "sessionId": session_id });
                        if let Some(opts) =
                            config_options_for(&current, &model_list(), session_auto)
                        {
                            result["configOptions"] = Value::Array(opts);
                        }
                        reply(&conn, id, result);
                    }
                    Err(e) => reply_error(&conn, id, &e),
                }
            }
            "session/prompt" => {
                let Some(session_id) = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    reply_error(&conn, id, "session/prompt: missing sessionId");
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
                // cora scan 2026-10-01 (#155 #4): an empty array or one
                // carrying only non-text blocks (e.g. images) joins to an
                // empty string — launching a full agent turn on empty
                // input must be rejected exactly like a missing prompt.
                if prompt_text.trim().is_empty() {
                    reply_error(&conn, id, "session/prompt: missing prompt");
                    continue;
                }
                // The turn runs on its own thread; this reader loop stays
                // live for permission routing while it runs.
                let conn = conn.clone();
                let sessions = sessions.clone();
                let session_id_clone = session_id.clone();
                std::thread::spawn(move || {
                    let observer = AcpObserver {
                        conn: conn.clone(),
                        session_id: session_id_clone.clone(),
                        counter: Arc::new(Mutex::new(0)),
                        current: Mutex::new(None),
                    };
                    match run_session_turn(
                        sessions,
                        &session_id_clone,
                        &prompt_text,
                        Some(&observer),
                    ) {
                        Ok((stop, Some(text))) => {
                            conn.send_notification(
                                "session/update",
                                json!({
                                    "sessionId": session_id_clone,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": {"type": "text", "text": text},
                                    }
                                }),
                            );
                            reply(&conn, id, json!({ "stopReason": stop }));
                        }
                        Ok((stop, None)) => reply(&conn, id, json!({ "stopReason": stop })),
                        Err(e) => reply_error(&conn, id, &e),
                    }
                });
            }
            "session/set_config_option" => {
                set_config_option(
                    &conn,
                    id,
                    &sessions,
                    &approval_states,
                    &params,
                    &model_list(),
                    env_model.as_deref(),
                );
            }
            // ACP baseline MUST (issue #178): stop the session's
            // generating turn. A cancel BEFORE any prompt (or against an
            // unknown session) is a no-op — the spec leaves that state
            // unconstrained. A cancel while a permission request is
            // pending also unwinds the wait below (fail closed to Deny,
            // per spec) and the turn then settles as `cancelled`, not
            // `refusal`.
            "session/cancel" => {
                let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                    continue; // notification: no id, nothing to answer
                };
                let cancelled = {
                    let sessions = lock_sessions(&sessions);
                    sessions
                        .map
                        .get(session_id)
                        .map(|st| st.cancel.cancel())
                        .is_some()
                };
                if cancelled {
                    eprintln!("tole: session/{session_id} cancel requested");
                }
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

/// `session/set_config_option` (issue #176): the client picked a new
/// value for a config option. Two selects exist: `model` and
/// `approval`.
///
/// Asymmetry is deliberate. `model` needs the session's storage mutex
/// (register write), so a running turn must refuse it — the reader loop
/// would otherwise block on the lock the turn holds, the exact
/// permission-routing deadlock class the CodeCora scan removed, for a
/// change that only applies to the NEXT turn anyway (the provider is
/// built once per turn). `approval` is an atomic flag flip on shared
/// handles — no storage lock — so the spec's "at any point, even while
/// generating" is honored: the very next approval check in the same
/// turn sees it. Destructive tools are unaffected by either setting.
fn set_config_option(
    conn: &Conn,
    id: Option<Value>,
    sessions: &tole_cli::session_host::SharedSessions,
    approval_states: &Arc<Mutex<HashMap<String, SessionApprovalState>>>,
    params: &Value,
    models: &[String],
    env_model: Option<&str>,
) {
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
        reply_error(conn, id, "session/set_config_option: missing sessionId");
        return;
    };
    let Some(config_id) = params.get("configId").and_then(Value::as_str) else {
        reply_error(conn, id, "session/set_config_option: missing configId");
        return;
    };
    let Some(value) = params.get("value").and_then(Value::as_str) else {
        reply_error(conn, id, "session/set_config_option: missing value");
        return;
    };

    match config_id {
        "model" => set_model_option(
            conn,
            id,
            sessions,
            approval_states,
            session_id,
            value,
            models,
            env_model,
        ),
        "approval" => set_approval_option(conn, id, approval_states, session_id, value, models),
        other => reply_error(
            conn,
            id,
            &format!("session/set_config_option: unknown configId: {other}"),
        ),
    }
}

/// The `approval` config option: `ask` (per-call permission) or `auto`
/// (the `--yes` semantics — every Write auto-allowed, Destructive still
/// prompts). An atomic flip on the shared handle, visible to approval
/// checks immediately, including mid-turn. NOT persisted: consent
/// resets with the process by design.
fn set_approval_option(
    conn: &Conn,
    id: Option<Value>,
    approval_states: &Arc<Mutex<HashMap<String, SessionApprovalState>>>,
    session_id: &str,
    value: &str,
    models: &[String],
) {
    let auto = match value {
        "ask" => false,
        "auto" => true,
        _ => {
            reply_error(
                conn,
                id,
                &format!("session/set_config_option: unknown approval value: {value}"),
            );
            return;
        }
    };
    let state = {
        let map = approval_states.lock().expect("approval map");
        match map.get(session_id) {
            Some(st) => st.clone(),
            None => {
                reply_error(conn, id, &format!("unknown session: {session_id}"));
                return;
            }
        }
    };
    *state.auto_write.lock().unwrap_or_else(|p| p.into_inner()) = auto;
    let current = state
        .current_model
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let mut result = json!({});
    if let Some(opts) = config_options_for(&current, models, auto) {
        result["configOptions"] = Value::Array(opts);
    }
    reply(conn, id, result);
}

/// The `model` config option: persist the choice as a durable register
/// write and reply with the complete config state.
#[allow(clippy::too_many_arguments)]
fn set_model_option(
    conn: &Conn,
    id: Option<Value>,
    sessions: &tole_cli::session_host::SharedSessions,
    approval_states: &Arc<Mutex<HashMap<String, SessionApprovalState>>>,
    session_id: &str,
    value: &str,
    models: &[String],
    env_model: Option<&str>,
) {
    let env_current = env_model.unwrap_or_default();
    // Validate against exactly what this session advertises (including
    // the prepended env default): a value outside the list would break
    // the picker's currentValue ∈ options invariant.
    if !advertised_models(env_current, models)
        .iter()
        .any(|m| m == value)
    {
        reply_error(
            conn,
            id,
            &format!("session/set_config_option: unknown model value: {value}"),
        );
        return;
    }

    // Busy-check BEFORE touching the storage mutex: a running turn holds
    // the storage lock for its whole duration, and this handler runs on
    // the reader thread that routes permission requests.
    {
        let sessions = lock_sessions(sessions);
        let Some(state) = sessions.map.get(session_id) else {
            reply_error(conn, id, &format!("unknown session: {session_id}"));
            return;
        };
        if *state.busy.lock().expect("busy lock") {
            reply_error(conn, id, "session is busy running a turn");
            return;
        }
        // Durable write (append-only register entry), replayed by
        // session/load and honored by run_session_turn on every turn.
        let mut storage = state.storage.lock().unwrap_or_else(|p| p.into_inner());
        let commit =
            tole_core::storage::Commit::new().register(tole_core::register::RegisterWrite::set(
                MODEL_REGISTER.0,
                MODEL_REGISTER.1,
                json!(value),
            ));
        if let Err(e) = storage.commit(commit) {
            reply_error(conn, id, &format!("storing model override: {e}"));
            return;
        }
    }
    // The spec reply carries the COMPLETE config state — approval entry
    // included, with its live values from the session cache.
    let state = {
        let map = approval_states.lock().expect("approval map");
        match map.get(session_id) {
            Some(st) => st.clone(),
            None => {
                reply_error(conn, id, &format!("unknown session: {session_id}"));
                return;
            }
        }
    };
    *state
        .current_model
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = value.to_string();
    let auto = *state.auto_write.lock().unwrap_or_else(|p| p.into_inner());
    let mut result = json!({});
    if let Some(opts) = config_options_for(value, models, auto) {
        result["configOptions"] = Value::Array(opts);
    }
    reply(conn, id, result);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_list_env_wins_without_probe() {
        let mut probed = false;
        let out = resolve_model_list(Some("a, b"), || {
            probed = true;
            vec!["z".to_string()]
        });
        assert_eq!(out, vec!["a", "b"]);
        assert!(!probed, "a non-empty TOLE_MODELS must bypass the probe");
    }

    #[test]
    fn resolve_model_list_falls_to_probe_when_env_unset_or_empty() {
        assert_eq!(resolve_model_list(None, || vec!["z".into()]), vec!["z"]);
        assert_eq!(
            resolve_model_list(Some("  , "), || vec!["z".into()]),
            vec!["z"]
        );
    }

    #[test]
    fn resolve_model_list_keeps_empty_probe_result() {
        assert!(resolve_model_list(None, Vec::new).is_empty());
    }

    #[test]
    fn model_list_unset_or_empty_advertises_nothing() {
        assert!(parse_model_list(None).is_empty());
        assert!(parse_model_list(Some("")).is_empty());
        assert!(parse_model_list(Some("  , , ")).is_empty());
    }

    #[test]
    fn model_list_trims_dedupes_keeps_order() {
        assert_eq!(
            parse_model_list(Some(
                " glm/glm-5.3-flash , glm/glm-5.3 , glm/glm-5.3-flash "
            )),
            vec!["glm/glm-5.3-flash", "glm/glm-5.3"]
        );
    }

    #[test]
    fn config_options_without_models_still_advertise_approval() {
        assert!(model_config_option("m", &[]).is_none());
        // The approval selector is always advertised; the model picker
        // is the optional one (issue #176).
        let opts = config_options_for("m", &[], false).expect("approval entry");
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0]["id"], "approval");
        assert_eq!(opts[0]["category"], "mode");
        assert_eq!(opts[0]["currentValue"], "ask");
    }

    #[test]
    fn config_option_wire_shape_matches_spec() {
        let o = model_config_option("model-a", &["model-a".into(), "model-b".into()])
            .expect("advertised");
        assert_eq!(o["id"], "model");
        assert_eq!(o["name"], "Model");
        assert_eq!(o["category"], "model");
        assert_eq!(o["type"], "select");
        assert_eq!(o["currentValue"], "model-a");
        assert_eq!(
            o["options"],
            json!([
                {"value": "model-a", "name": "model-a"},
                {"value": "model-b", "name": "model-b"},
            ])
        );
    }

    #[test]
    fn config_option_prepends_current_when_missing_from_list() {
        // The spec requires currentValue ∈ options; the env default (or a
        // stale override after an env change) must therefore be prepended.
        let o = model_config_option("model-z", &["model-a".into()]).expect("advertised");
        assert_eq!(o["currentValue"], "model-z");
        assert_eq!(
            o["options"][0],
            json!({"value": "model-z", "name": "model-z"})
        );
        assert_eq!(o["options"].as_array().expect("options").len(), 2);
    }

    #[test]
    fn config_options_order_and_empty_current_fallback() {
        // Approval first (priority order per the spec), model second;
        // an empty current model falls back to the first advertised id.
        let opts =
            config_options_for("", &["model-a".into(), "model-b".into()], true).expect("opts");
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["id"], "approval");
        assert_eq!(opts[0]["currentValue"], "auto");
        assert_eq!(opts[1]["id"], "model");
        assert_eq!(opts[1]["currentValue"], "model-a");
    }
}
