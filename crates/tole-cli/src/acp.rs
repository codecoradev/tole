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
                        sessions.map.insert(session_id.clone(), state);
                        reply(&conn, id, json!({ "sessionId": session_id }));
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
            other => {
                if id.is_some() {
                    reply_error(&conn, id, &format!("method not supported: {other}"));
                }
            }
        }
    }
    Ok(())
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
