//! Missions over serve (#947): `POST /missions`, `GET /missions/{id}`,
//! `POST /missions/{id}/cancel`.
//!
//! A mission is its own session (its own pinned prompt and writer lock), the
//! way `tole mission` makes one, so it is not a sub-route of a live session.
//! It runs on a background thread through the SAME loop as the CLI
//! (`mission::run_mission_outcome`); this module only decides what a request
//! may ask for and keeps the table of missions this server started.
//!
//! Owner configuration, never a request field: the verify command (and its
//! timeout), the budget ceilings, the workspace (the serve workspace root),
//! the allow list, the sandbox and the profile. A body with any key but the
//! four it names is a `400`, `verify` by name.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::State;
use crate::mission::{self, BudgetTier, MissionConfig, MissionOutcome};
use tole_core::storage::Storage;

/// What the owner configured for missions at start-up. Read once from the
/// user config (`[mission] verify`, `verify_timeout`, the budget ceilings),
/// never from a request.
#[derive(Clone)]
pub struct MissionSettings {
    /// The verify command, when the owner set one.
    pub verify: Option<String>,
    pub verify_timeout_secs: u64,
    /// Both the default and the ceiling of every budget: a request may ask
    /// for less, never more.
    pub ceiling: BudgetTier,
    pub compaction: tole_cli::compaction_host::CompactionSettings,
}

impl Default for MissionSettings {
    fn default() -> Self {
        Self {
            verify: None,
            verify_timeout_secs: 300,
            ceiling: mission::DEFAULT_TIER,
            compaction: Default::default(),
        }
    }
}

/// One mission this server started.
struct Entry {
    cancel: tole_core::cancel::CancelToken,
    /// `None` while it runs.
    done: Option<Result<MissionOutcome, String>>,
}

/// The missions of this server process. One may run at a time.
#[derive(Default)]
pub struct Missions {
    table: Mutex<HashMap<String, Entry>>,
}

impl Missions {
    /// A mission that is running, for tests of the routes around it.
    #[cfg(test)]
    pub fn plant_running(&self, id: &str, cancel: tole_core::cancel::CancelToken) {
        self.lock()
            .insert(id.to_string(), Entry { cancel, done: None });
    }

    /// Shutdown (#1014): ask every running mission to stop, through the same
    /// cooperative cancel as `POST /missions/{id}/cancel`, and return how many are
    /// still running. A mission is its own session and has no detach path of its
    /// own; it ends at its next checkpoint with a durable summary.
    pub fn cancel_running(&self) -> usize {
        let table = self.lock();
        table
            .values()
            .filter(|e| e.done.is_none())
            .inspect(|e| e.cancel.cancel())
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The only keys a mission request may carry.
const KEYS: [&str; 4] = ["goal", "max_steps", "max_minutes", "max_tokens"];
const MAX_GOAL_CHARS: usize = 8000;

/// Parse a budget field: absent or `null` = the ceiling, otherwise a positive
/// integer not above it.
fn budget(req: &Value, key: &str, ceiling: u64) -> Result<u64, String> {
    match req.get(key) {
        None | Some(Value::Null) => Ok(ceiling),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 && n <= ceiling => Ok(n),
            Some(n) if n > ceiling => Err(format!(
                "{key} {n} is above this server's ceiling of {ceiling}"
            )),
            _ => Err(format!("{key} must be a positive integer")),
        },
    }
}

/// `POST /missions {goal, max_steps?, max_minutes?, max_tokens?}`.
pub(super) fn start(state: &State, body: &str) -> (u16, Value) {
    let Ok(req) = serde_json::from_str::<Value>(body) else {
        return (400, json!({"error": "invalid JSON body"}));
    };
    let Some(obj) = req.as_object() else {
        return (400, json!({"error": "the body must be a JSON object"}));
    };
    if obj.contains_key("verify") {
        return (
            400,
            json!({"error": "verify cannot be set by a request: the verify command is the \
                    server owner's configuration ([mission] verify)"}),
        );
    }
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return (
            400,
            json!({"error": format!(
                "unknown field {k:?}: a mission request takes only {}; everything else is \
                 the server owner's configuration", KEYS.join(", "))}),
        );
    }
    let goal = match req.get("goal").and_then(Value::as_str).map(str::trim) {
        Some(g) if !g.is_empty() && g.chars().count() <= MAX_GOAL_CHARS => g.to_string(),
        Some(g) if !g.is_empty() => {
            return (
                400,
                json!({"error": format!("goal is over {MAX_GOAL_CHARS} characters")}),
            )
        }
        _ => return (400, json!({"error": "goal must be a non-empty string"})),
    };
    let ceil = state.mission.ceiling;
    let (max_steps, max_minutes, max_tokens) = match (
        budget(&req, "max_steps", ceil.max_steps),
        budget(&req, "max_minutes", ceil.max_minutes),
        budget(&req, "max_tokens", ceil.max_tokens),
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return (400, json!({"error": e})),
    };
    // A mission writes (todo_write at least), so it cannot run where the
    // owner started the server read-only.
    if state.plan_mode {
        return (
            409,
            json!({"error": "this server runs in plan mode; a mission needs a writable session"}),
        );
    }

    let id = tole_cli::session_host::new_session_id("mission");
    let cancel = tole_core::cancel::CancelToken::new();
    // Claim the one slot and register the mission BEFORE the thread starts,
    // so a second request cannot slip in between.
    {
        let mut table = state.missions.lock();
        if table.values().any(|e| e.done.is_none()) {
            return (
                409,
                json!({"error": "a mission is already running on this server"}),
            );
        }
        table.insert(
            id.clone(),
            Entry {
                cancel: cancel.clone(),
                done: None,
            },
        );
    }
    let cwd = state.workspace_root.to_string_lossy().into_owned();
    let dir = super::serve_storage_dir(state, &cwd);
    let registry = match build_registry(state, &id, &dir, &cancel) {
        Ok(r) => r,
        Err(e) => {
            state.missions.lock().remove(&id);
            return (500, json!({"error": e}));
        }
    };
    let cfg = MissionConfig {
        cancel,
        goal,
        max_steps,
        max_minutes,
        max_tokens,
        verify: state.mission.verify.clone(),
        resume_id: None,
        fresh_id: Some(id.clone()),
        verify_timeout_secs: state.mission.verify_timeout_secs,
        compaction: state.mission.compaction,
        images: Vec::new(),
        persona: None,
        advisor: None,
    };
    let shared = Arc::clone(&state.missions);
    let run_id = id.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("mission-{id}"))
        .spawn(move || {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    return Err(format!("creating {}: {e}", dir.display()));
                }
                mission::run_mission_outcome(cfg, registry, &dir, &|_, _| Ok(()))
                    .map_err(|e| format!("{e:#}"))
            }))
            .unwrap_or_else(|_| Err("the mission thread panicked".to_string()));
            if let Err(e) = &ran {
                eprintln!("tole serve: mission {run_id} failed: {e}");
            }
            if let Some(entry) = shared.lock().get_mut(&run_id) {
                entry.done = Some(ran);
            }
        });
    if let Err(e) = spawned {
        state.missions.lock().remove(&id);
        return (
            500,
            json!({"error": format!("starting the mission thread: {e}")}),
        );
    }
    eprintln!("tole serve: mission {id} started");
    (202, json!({"id": id, "status": "running"}))
}

/// The registry of a mission: the server-session roster over the serve
/// workspace, with write approvals queued in the serve approvals queue.
fn build_registry(
    state: &State,
    id: &str,
    dir: &std::path::Path,
    cancel: &tole_core::cancel::CancelToken,
) -> Result<tole_core::tool::ToolRegistry, String> {
    let approver = tole_cli::approvals::QueueApprover::new(
        state.allow_patterns.clone(),
        Arc::clone(&state.approvals),
        id,
        dir.join(format!("{id}.jsonl")),
    );
    let mut roster = tole_cli::session::assemble::roster_for(
        &tole_cli::session::assemble::FacePolicy::SERVER_SESSION,
        tole_cli::session::assemble::RosterInputs {
            workspace: state.workspace_root.clone(),
            plan_mode: false,
            allow_patterns: &state.allow_patterns,
            agents_worktree: false,
            delegation: None,
            mcp_specs: &[],
        },
    )
    .map_err(|e| e.to_string())?;
    roster.session = Some(id.to_string());
    roster.job_notices = Some(cancel.interrupts().clone());
    let mut reg = tole_cli::tool_roster::build(approver, roster).map_err(|e| e.to_string())?;
    if !state.turnend.is_empty() {
        let mut hooks = tole_core::hooks::ToolHooks::from_cli(&[], &[]);
        hooks.turnend = state
            .turnend
            .iter()
            .map(|c| tole_core::hooks::turnend_hook(c))
            .collect();
        reg.set_hooks(hooks);
    }
    Ok(reg)
}

fn outcome_json(id: &str, o: &MissionOutcome) -> Value {
    json!({
        "id": id,
        "status": o.status,
        "running": false,
        "turns": o.turns,
        "steps": o.steps,
        "tokens": o.tokens,
        "wallSeconds": o.wall_secs,
        "summary": o.summary,
    })
}

/// `GET /missions/{id}`: from this process's table while it knows the
/// mission, else from the `fact/mission` register of an earlier one's file.
pub(super) fn status(state: &State, id: &str) -> (u16, Value) {
    {
        let table = state.missions.lock();
        if let Some(e) = table.get(id) {
            return match &e.done {
                None => (200, json!({"id": id, "status": "running", "running": true})),
                Some(Ok(o)) => (200, outcome_json(id, o)),
                Some(Err(msg)) => (
                    200,
                    json!({"id": id, "status": "error", "running": false, "error": msg}),
                ),
            };
        }
    }
    // Not started by this process: a mission file whose summary was written.
    if !id.starts_with("mission-") || tole_cli::session_host::validate_session_id(id).is_none() {
        return (404, json!({"error": "unknown mission"}));
    }
    let cwd = state.workspace_root.to_string_lossy().into_owned();
    let file = super::serve_storage_dir(state, &cwd).join(format!("{id}.jsonl"));
    if !file.is_file() {
        return (404, json!({"error": "unknown mission"}));
    }
    let summary = tole_core::storage::JsonlStorage::open_read_only(&file)
        .ok()
        .and_then(|s| s.get_register("fact", "mission").cloned());
    match summary {
        Some(s) => (
            200,
            json!({
                "id": id,
                "status": s.get("status").cloned().unwrap_or(Value::Null),
                "running": false,
                "turns": s.get("turns").cloned().unwrap_or(Value::Null),
                "steps": s.get("steps").cloned().unwrap_or(Value::Null),
                "tokens": s.get("tokens_in_out").cloned().unwrap_or(Value::Null),
                "wallSeconds": s.get("wall_seconds").cloned().unwrap_or(Value::Null),
                "summary": s,
            }),
        ),
        None => (404, json!({"error": "unknown mission"})),
    }
}

/// `POST /missions/{id}/cancel`: the same cooperative cancel as a session's.
/// The mission ends at its next checkpoint with a durable summary.
pub(super) fn cancel(state: &State, id: &str) -> (u16, Value) {
    let table = state.missions.lock();
    match table.get(id) {
        None => (404, json!({"error": "unknown mission"})),
        Some(e) if e.done.is_some() => (409, json!({"error": "the mission has already ended"})),
        Some(e) => {
            e.cancel.cancel();
            eprintln!("tole serve: mission {id} cancel requested");
            (202, json!({"id": id, "cancelled": true}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::cancel::CancelToken;

    #[test]
    fn shutdown_cancels_a_running_mission_and_leaves_a_finished_one_alone() {
        let m = Missions::default();
        assert_eq!(m.cancel_running(), 0, "nothing runs");
        let (running, finished) = (CancelToken::new(), CancelToken::new());
        m.plant_running("mission-a", running.clone());
        m.lock().insert(
            "mission-b".to_string(),
            Entry {
                cancel: finished.clone(),
                done: Some(Err("stopped".into())),
            },
        );
        assert_eq!(m.cancel_running(), 1);
        assert!(running.is_cancelled());
        assert!(!finished.is_cancelled());
    }
}
