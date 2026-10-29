//! #137: multi-session MCP tools — one MCP connection, N durable tole
//! sessions, routing by `session_id` in the tool arguments.
//!
//! These tools are registered alongside the registry's regular tools
//! when tole serves MCP over HTTP. The MCP client opens a session
//! (`tole_session_new`), runs turns (`tole_session_prompt`), inspects
//! (`tole_session_status`), and lists (`tole_session_list`) — all by
//! explicit session id. Sessions are durable JSONL files in the
//! session's workspace; a server restart preserves them (the JSONL is
//! the source of truth, `session/load` semantics).
//!
//! `tole_session_prompt` runs the turn synchronously in the MCP
//! request (v1) — the final answer returns as the tool result text.

use serde_json::{json, Value};
use std::sync::Arc;

use crate::session_host::{
    lock_sessions, new_session_id, open_session, run_session_turn, SessionState, Sessions,
    SharedSessions,
};
use tole_core::memory::MemoryConfig;
use tole_core::tool::{Risk, Tool};

/// Jail-of-jails, shared by both server transports (MCP
/// `tole_session_new` and REST `POST /sessions`): a client-supplied
/// session cwd must resolve INSIDE the server's workspace root —
/// otherwise a remote client could point a session at `/` (the whole
/// filesystem) or `/etc` (CodeCora #137). Returns the CANONICAL path the
/// session must be rooted at: a symlink swapped in after the check (or a
/// `..`/symlink spelling in the raw string) must not redirect it.
pub fn jail_session_cwd(workspace_root: &std::path::Path, cwd: &str) -> Result<String, String> {
    let root = workspace_root
        .canonicalize()
        .map_err(|e| format!("workspace root: {e}"))?;
    let target = std::path::Path::new(cwd)
        .canonicalize()
        .map_err(|e| format!("cwd {cwd:?}: {e}"))?;
    if target != root && !target.starts_with(&root) {
        return Err(format!(
            "cwd {cwd:?} escapes the server workspace root {}",
            root.display()
        ));
    }
    target
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("cwd {cwd:?} is not valid UTF-8 once resolved"))
}

/// Shared factory state for the session tools.
#[derive(Clone)]
pub struct SessionToolState {
    pub sessions: SharedSessions,
    pub allow_patterns: Vec<String>,
    pub plan_mode: bool,
    pub memory: Option<MemoryConfig>,
    /// Server-side root that constrains client-supplied cwd values
    /// (CodeCora on #137: without it a client could jail a session to
    /// `/` — the whole filesystem). Defaults to the server cwd.
    pub workspace_root: std::path::PathBuf,
    /// Explicit `--sessions-dir` override (None = per-session-cwd
    /// default, the pre-existing behavior).
    pub sessions_dir: Option<std::path::PathBuf>,
    /// `--on-turnend` stop gates wired onto every session registry.
    pub turnend: Vec<String>,
}

impl SessionToolState {
    pub fn new(
        allow_patterns: Vec<String>,
        plan_mode: bool,
        memory: Option<MemoryConfig>,
        workspace_root: std::path::PathBuf,
        sessions_dir: Option<std::path::PathBuf>,
        turnend: Vec<String>,
    ) -> Self {
        Self {
            sessions: Arc::new(std::sync::Mutex::new(Sessions::default())),
            allow_patterns,
            plan_mode,
            memory,
            workspace_root,
            sessions_dir,
            turnend,
        }
    }

    /// Resolve the target session: explicit `session_id` wins; with
    /// exactly one open session it is the implicit default (single-
    /// session ergonomics); otherwise refuse with a clear error.
    fn resolve(&self, args: &Value) -> Result<SessionState, String> {
        let sessions = lock_sessions(&self.sessions);
        let id = sessions.resolve_id(args.get("session_id").and_then(Value::as_str))?;
        sessions
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown session: {id}"))
    }

    /// The session-level tools as registry entries.
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(SessionNewTool(self.clone())),
            Box::new(SessionPromptTool(self.clone())),
            Box::new(SessionCancelTool(self.clone())),
            Box::new(SessionInterruptTool(self.clone())),
            Box::new(SessionStatusTool(self.clone())),
            Box::new(SessionListTool(self.clone())),
        ]
    }
}

// ---------------------------------------------------------------------------
// tole_session_new
// ---------------------------------------------------------------------------

struct SessionNewTool(SessionToolState);

impl Tool for SessionNewTool {
    fn name(&self) -> &str {
        "tole_session_new"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly // the session itself is a handle; tool risk is
                       // governed by the session's allowlist approver
    }
    fn summary(&self) -> String {
        "Open a new tole session rooted at a workspace directory; returns a session id.".into()
    }

    fn describe(&self, input: &Value) -> String {
        format!(
            "open tole session (cwd={:?})",
            input.get("cwd").and_then(Value::as_str).unwrap_or(".")
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "Workspace root for the session (jail for file/git tools)" }
            },
            "required": ["cwd"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let cwd = input
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or("tole_session_new: missing 'cwd'")?;
        let jailed = jail_session_cwd(&self.0.workspace_root, cwd)?;
        let cwd = jailed.as_str();
        // Capacity first: open_session creates the session file on disk, so
        // refusing AFTER it would leave an orphaned session behind. The
        // re-check at insert time below still guards a concurrent creator.
        if lock_sessions(&self.0.sessions).ensure_room().is_err() {
            return Err("session map at capacity and all sessions busy — close one first".into());
        }
        let session_id = new_session_id("mcp");
        let approver =
            tole_core::approval::AllowlistApprover::allow_only(self.0.allow_patterns.clone());
        let state = open_session(
            &session_id,
            cwd,
            false,
            self.0.plan_mode,
            approver,
            self.0.memory.clone(),
            self.0.sessions_dir.as_deref(),
            self.0.turnend.clone(),
            self.0.allow_patterns.clone(),
            tole_core::cancel::CancelToken::default(),
            None,
        )?;
        // Cap + eviction (the registry's admission rule): sessions are
        // durable on disk, eviction drops only the in-memory handle, and
        // busy sessions are never evicted.
        lock_sessions(&self.0.sessions)
            .admit(session_id.clone(), state)
            .map_err(|_| "session map at capacity and all sessions busy — close one first")?;
        Ok(json!({ "session_id": session_id, "workspace": cwd }))
    }
}

// ---------------------------------------------------------------------------
// tole_session_prompt
// ---------------------------------------------------------------------------

struct SessionPromptTool(SessionToolState);

impl Tool for SessionPromptTool {
    fn name(&self) -> &str {
        "tole_session_prompt"
    }
    fn risk(&self) -> Risk {
        // The TOOL itself is a session-management call; the agentic turn
        // it runs inside goes through the SESSION's approver (per-call
        // risk gates there). Server-level classification as Write made
        // the tool uncallable without blanket --allow (found live).
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Send a prompt to a tole session and run one turn to completion.".into()
    }

    fn describe(&self, input: &Value) -> String {
        format!(
            "run one tole turn (session {:?})",
            input
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("<implicit>")
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Target session (optional when exactly one is open)" },
                "text": { "type": "string", "description": "The user prompt for this turn" }
            },
            "required": ["text"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .ok_or("tole_session_prompt: missing 'text'")?
            .to_string();
        // Resolve the session id (explicit or implicit-singleton) BEFORE
        // the turn: run_session_turn addresses the map by id.
        let session_id = lock_sessions(&self.0.sessions)
            .resolve_id(input.get("session_id").and_then(Value::as_str))?;
        let (stop, text) =
            run_session_turn(Arc::clone(&self.0.sessions), &session_id, &text, None)?;
        let mut out = json!({ "stop_reason": stop });
        if let Some(t) = text {
            out["text"] = json!(t);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// tole_session_interrupt — soft interrupt (#377)
// ---------------------------------------------------------------------------

/// Steers the target session's in-flight turn WITHOUT cancelling it: the
/// message lands at the turn's next safe point as an attributed user-role
/// entry. Same classification as `tole_session_cancel` (callable while a
/// `tole_session_prompt` runs; the session's own approver still gates every
/// tool the steered turn runs).
struct SessionInterruptTool(SessionToolState);

impl Tool for SessionInterruptTool {
    fn name(&self) -> &str {
        "tole_session_interrupt"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Steer the in-flight turn of a tole session with a message, without cancelling it.".into()
    }
    fn describe(&self, input: &Value) -> String {
        format!(
            "interrupt session {:?}",
            input
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("<implicit>")
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Target session (optional when exactly one is open)" },
                "text": { "type": "string", "description": "The steering message" },
                "urgent": { "type": "boolean", "description": "Also withdraw a tool call that has not started yet (default false)" }
            },
            "required": ["text"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .ok_or("missing 'text'")?;
        let urgent = input
            .get("urgent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let st = self.0.resolve(&input)?;
        match crate::session_host::interrupt_session(&st, text, urgent) {
            Ok(()) => Ok(json!({
                "queued": true,
                "urgent": urgent,
                "note": "delivered at the turn's next safe point (after the running tool, before the next model call)"
            })),
            Err(crate::session_host::InterruptRefusal::NotBusy) => {
                Err("no turn is running in this session".into())
            }
            Err(crate::session_host::InterruptRefusal::Invalid(e)) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// tole_session_cancel — the MCP face of #178
// ---------------------------------------------------------------------------

/// Cancels the target session's in-flight turn (`run_session_turn`
/// observes the token at its next checkpoint and settles `cancelled`).
/// MCP-idiomatic cancellation: the MCP wire layer has no turn-cancel
/// notification handling for server-side tool execution, so the
/// capability is exposed as a first-class tool — callable by any MCP
/// client, addressable by session, before/while a `tole_session_prompt`
/// runs (MCP tools may run concurrently from the client side).
struct SessionCancelTool(SessionToolState);

impl Tool for SessionCancelTool {
    fn name(&self) -> &str {
        "tole_session_cancel"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Cancel the in-flight turn of a tole session.".into()
    }

    fn describe(&self, input: &Value) -> String {
        format!(
            "cancel the in-flight turn of session {:?}",
            input
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("<implicit>")
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Target session (optional when exactly one is open)" },
                "reason": { "type": "string", "description": "Optional cancellation reason (logged)" }
            }
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Resolve the id exactly like tole_session_prompt (explicit or
        // implicit-singleton), then set the token under the map lock.
        let (session_id, found) = {
            let sessions = lock_sessions(&self.0.sessions);
            let id = sessions.resolve_id(input.get("session_id").and_then(Value::as_str))?;
            let found = sessions.cancel(&id);
            (id, found)
        };
        if !found {
            return Err(format!("unknown session: {session_id}"));
        }
        if !reason.is_empty() {
            eprintln!("tole: session/{session_id} cancel requested (mcp): {reason}");
        } else {
            eprintln!("tole: session/{session_id} cancel requested (mcp)");
        }
        Ok(json!({
            "cancelled": true,
            "session_id": session_id,
            "note": "the in-flight turn settles 'cancelled' at its next checkpoint; a prompt response already past its last checkpoint completes normally"
        }))
    }
}

// ---------------------------------------------------------------------------
// tole_session_status / tole_session_list
// ---------------------------------------------------------------------------

struct SessionStatusTool(SessionToolState);

impl Tool for SessionStatusTool {
    fn name(&self) -> &str {
        "tole_session_status"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Report the status of a tole session.".into()
    }

    fn describe(&self, input: &Value) -> String {
        format!(
            "session status ({:?})",
            input
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("<implicit>")
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Target session (optional when exactly one is open)" }
            }
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        use tole_core::storage::Storage;
        let st = self.0.resolve(&input)?;
        let busy = *st.busy.lock().expect("busy lock");
        let (entries, report) = match st.storage.try_lock() {
            Ok(guard) => (
                json!(guard.entries().len()),
                // Issue #211: additive usage report (same object as the
                // serve status JSON).
                crate::usage_report::usage_json(guard.entries(), guard.usages()),
            ),
            Err(_) => (json!(null), json!(null)), // turn in flight
        };
        Ok(json!({ "busy": busy, "entries": entries, "usage_report": report }))
    }
}

struct SessionListTool(SessionToolState);

impl Tool for SessionListTool {
    fn name(&self) -> &str {
        "tole_session_list"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "List the open tole sessions.".into()
    }

    fn describe(&self, _input: &Value) -> String {
        "list open tole sessions".into()
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({ "type": "object", "properties": {} }))
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        let sessions = lock_sessions(&self.0.sessions);
        let list: Vec<Value> = sessions
            .iter()
            .map(|(id, st)| {
                // Issue #256 (rescan #38): NEVER take a blocking lock on
                // a session's busy mutex while holding the sessions-map
                // lock — a busy session's turn thread serializes this
                // behind its whole step, and two list calls deadlock on
                // each other's map lock. try_lock like the eviction
                // path (line ~175): a busy session reports "unknown"
                // instead of stalling the listing.
                let busy = st.busy.try_lock().map(|b| *b).unwrap_or(true);
                json!({
                    "session_id": id,
                    "busy": busy,
                })
            })
            .collect();
        Ok(json!({ "sessions": list }))
    }
}
