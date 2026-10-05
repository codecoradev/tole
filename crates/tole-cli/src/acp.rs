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

/// The full `configOptions` response array for a session: the model
/// picker when configured. `current` is the effective model — the
/// durable register override when set, else the env default, else the
/// first advertised entry.
fn config_options_for(current: &str, models: &[String]) -> Option<Vec<Value>> {
    let mut current = current.to_string();
    if current.is_empty() {
        current = models.first()?.clone();
    }
    model_config_option(&current, models).map(|o| vec![o])
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

    /// Agent-initiated request (permission): returns the client's result
    /// object. Client cancellations/errors arrive as the error variant of
    /// the routed reply and fail closed (deny).
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
    // Advertised model list (issue #176): static for the process
    // lifetime — env cannot change under a running agent.
    let models = parse_model_list(std::env::var("TOLE_MODELS").ok().as_deref());
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
                let approver = InteractiveApprover::new(AcpPrompt {
                    conn: conn.clone(),
                    session_id: session_id.clone(),
                    counter: Arc::new(Mutex::new(0)),
                })
                .with_allow_patterns(allow_patterns.to_vec())
                .with_auto_write(auto_write);
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
                        let mut result = json!({ "sessionId": session_id });
                        if let Some(opts) = config_options_for(&current, &models) {
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
                    match run_session_turn(sessions, &session_id_clone, &prompt_text) {
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
                set_config_option(&conn, id, &sessions, &params, &models, env_model.as_deref());
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
/// value for a config option. Only the `model` select exists today.
///
/// The spec allows setting while a turn is generating; tole refuses it
/// instead. The reader loop would otherwise block on the storage mutex
/// the running turn holds — exactly the permission-routing deadlock
/// class the CodeCora scan removed — for a change that could only apply
/// to the NEXT turn anyway (the provider is built once per turn).
/// Clients retry after the turn settles.
fn set_config_option(
    conn: &Conn,
    id: Option<Value>,
    sessions: &tole_cli::session_host::SharedSessions,
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
    if config_id != "model" {
        reply_error(
            conn,
            id,
            &format!("session/set_config_option: unknown configId: {config_id}"),
        );
        return;
    }
    let Some(value) = params.get("value").and_then(Value::as_str) else {
        reply_error(conn, id, "session/set_config_option: missing value");
        return;
    };

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
    let current = value.to_string();
    let mut result = json!({});
    if let Some(opts) = config_options_for(&current, models) {
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
    fn config_option_none_without_models() {
        assert!(model_config_option("m", &[]).is_none());
        assert!(config_options_for("m", &[]).is_none());
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
    fn config_options_empty_current_falls_back_to_first_advertised() {
        let opts = config_options_for("", &["model-a".into(), "model-b".into()]).expect("opts");
        assert_eq!(opts[0]["currentValue"], "model-a");
    }
}
