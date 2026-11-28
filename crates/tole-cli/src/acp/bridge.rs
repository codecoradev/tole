//! The bridges from the core's traits to the wire: the progress observer (tool cards,
//! thought bubbles, streamed text; issue #196) and the approval prompt, where the editor
//! is the human (`session/request_permission`).

use serde_json::{json, Value};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::wire::Conn;
use tole_cli::approver::PromptFn;

/// Permission requests can sit in an editor until a human clicks; do not
/// turn that into a timeout race.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(600);

/// Outcome of a sliced permission wait (issue #178).
enum WaitOutcome {
    /// The client answered.
    Reply(Value),
    /// The window expired, the transport died, or the turn was
    /// cancelled with the request unresolved. The caller fails closed.
    Timeout,
}

/// Wait for ONE pending permission reply in slices, re-checking the turn's
/// cancellation token between slices (issue #178): the request was sent once
/// via [`Conn::open_request`], only the WAIT is sliced, so a mid-wait
/// `session/cancel` unwinds within ~1 s. Every unresolved exit abandons the
/// pending entry (late replies find no waiter and are ignored by `route`).
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

/// ACP `ToolKind` for a tole tool name (issue #196 phase 1): the spec's
/// taxonomy drives host icons and progress rendering. "other" is the fallback
/// for anything unmapped; this is a pure name lookup, no registration
/// assumption.
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
pub(super) struct AcpObserver {
    pub(super) conn: Conn,
    pub(super) session_id: String,
    pub(super) counter: Arc<Mutex<u64>>,
    /// Set once answer text has been streamed via text_delta (issue
    /// #196 phase 3): the end-of-turn full-text delivery is suppressed
    /// so the host receives the message exactly once. Shared with the
    /// prompt thread — the trait object cannot expose the field.
    pub(super) text_streamed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The open card's id (cora MAJOR, #196): `tool_call_update` must
    /// reference the `toolCallId` the started card announced, or hosts
    /// cannot correlate them and the card stays `in_progress` forever.
    /// Tools run sequentially on the turn thread, so one slot suffices.
    pub(super) current: Mutex<Option<String>>,
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

    fn text_delta(&self, text: &str) {
        self.text_streamed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.conn.send_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                }
            }),
        );
    }
}

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
pub(super) struct AcpPrompt {
    pub(super) conn: Conn,
    pub(super) session_id: String,
    pub(super) counter: Arc<Mutex<u64>>,
    pub(super) patterns: Arc<Mutex<Vec<String>>>,
    /// Cancellation checkpoint for the session's current turn
    /// (issue #178): a `session/cancel` while a permission request is
    /// pending stops the wait immediately — the spec requires pending
    /// permission requests to settle `cancelled` (fail closed).
    pub(super) cancel: tole_core::cancel::CancelToken,
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
        // The timeline card is the observer's job (issue #196): announce +
        // completion fire from the turn loop for EVERY execution. The request
        // stays self-contained (hosts render the dialog from the embedded
        // toolCall). allow_always only for Write-tier calls: Destructive is
        // never allowlistable, and ReadOnly never reaches the approver.
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
                "title": tole_cli::acp_title::permission_title(req),
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
