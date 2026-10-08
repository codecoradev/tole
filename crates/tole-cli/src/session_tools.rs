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
    SharedSessions, MAX_SESSIONS,
};
use tole_core::memory::MemoryConfig;
use tole_core::tool::{Risk, Tool};

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
        match args.get("session_id").and_then(Value::as_str) {
            Some(id) => sessions
                .map
                .get(id)
                .cloned()
                .ok_or_else(|| format!("unknown session: {id}")),
            None if sessions.map.len() == 1 => {
                Ok(sessions.map.values().next().expect("len==1").clone())
            }
            None => Err(
                "missing 'session_id': pass it explicitly (or keep exactly one session open)"
                    .into(),
            ),
        }
    }

    /// The session-level tools as registry entries.
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(SessionNewTool(self.clone())),
            Box::new(SessionPromptTool(self.clone())),
            Box::new(SessionCancelTool(self.clone())),
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
        // Jail-of-jails: the client-supplied cwd must resolve INSIDE the
        // server's workspace root — otherwise a remote client could point
        // a session at / (the whole filesystem) or /etc (CodeCora #137).
        let root = self
            .0
            .workspace_root
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
        {
            // Cap + eviction, mirroring the REST transport (CodeCora:
            // the MCP session map grew without bound). Sessions are
            // durable on disk; eviction drops only the in-memory handle.
            // Busy sessions are never evicted; all-busy at capacity is
            // a clear error.
            let mut sessions = lock_sessions(&self.0.sessions);
            if sessions.evict_for_insert(MAX_SESSIONS).is_err() {
                return Err(
                    "session map at capacity and all sessions busy — close one first".into(),
                );
            }
            sessions.map.insert(session_id.clone(), state);
        }
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
        let session_id =
            {
                let sessions = lock_sessions(&self.0.sessions);
                match input.get("session_id").and_then(Value::as_str) {
                    Some(id) => id.to_string(),
                    None if sessions.map.len() == 1 => {
                        sessions.map.keys().next().expect("len==1").clone()
                    }
                    None => return Err(
                        "missing 'session_id': pass it explicitly (or keep exactly one session \
                         open)"
                            .into(),
                    ),
                }
            };
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
        let (session_id, found) =
            {
                let sessions = lock_sessions(&self.0.sessions);
                let id = match input.get("session_id").and_then(Value::as_str) {
                    Some(id) => Some(id.to_string()),
                    None if sessions.map.len() == 1 => {
                        Some(sessions.map.keys().next().expect("len==1").clone())
                    }
                    None => None,
                };
                match id {
                    Some(id) => {
                        let found = sessions.map.get(&id).map(|st| st.cancel.cancel()).is_some();
                        (id, found)
                    }
                    None => return Err(
                        "missing 'session_id': pass it explicitly (or keep exactly one session \
                         open)"
                            .into(),
                    ),
                }
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
        let entries = match st.storage.try_lock() {
            Ok(guard) => json!(guard.entries().len()),
            Err(_) => json!(null), // turn in flight
        };
        Ok(json!({ "busy": busy, "entries": entries }))
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
            .map
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
