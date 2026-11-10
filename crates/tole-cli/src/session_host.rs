//! Shared session host: the durable-session machinery used by BOTH
//! ACP (`tole acp`) and the HTTP server (`tole serve`). One
//! implementation, two transports — the drift the scan kept flagging.
//!
//! `open_session` takes the session's APPROVER as a parameter: ACP wires
//! the interactive editor prompt (Destructive consent is a genuine
//! per-call human decision), the HTTP server wires a non-interactive
//! allowlist (pre-authorized via --allow; Destructive structurally
//! absent). Everything else — the jail, the registry, the memory loop —
//! is identical.

use crate::tool_roster::PlanEmitter;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc as StdArc, Mutex};
use tole_core::memory::MemoryConfig;
use tole_core::storage::Storage;

#[derive(Clone)]
pub struct SessionState {
    /// Per-session storage lock: a turn holds THIS (not the session-map
    /// lock), so the reader loop stays live for permission routing while
    /// a prompt runs (CodeCora scan deadlock finding).
    pub storage: StdArc<Mutex<tole_core::storage::JsonlStorage>>,
    /// The session's JSONL file. Read-only consumers (the transcript
    /// route, #524) replay THIS instead of taking the storage lock a turn
    /// holds for its whole duration.
    pub path: PathBuf,
    pub registry: StdArc<tole_core::tool::ToolRegistry>,
    pub system_prompt: Option<String>,
    pub memory: Option<MemoryConfig>,
    pub first_prompt_done: StdArc<Mutex<bool>>,
    pub busy: StdArc<Mutex<bool>>,
    /// Cancellation checkpoint (issue #178): the transport sets it on
    /// Per-session tool-card id counter (issue #196; cora CI on PR
    /// #203): observer ids must keep incrementing ACROSS turns — a
    /// per-turn counter would re-emit `tool-1` on the second prompt and
    /// hosts would correlate the completion onto the previous turn's
    /// stale card.
    pub tool_ids: StdArc<Mutex<u64>>,
    /// client cancel (ACP `session/cancel`); the turn loop checks it
    /// between steps. `serve` never sets it — REST behavior unchanged.
    pub cancel: tole_core::cancel::CancelToken,
}

/// Why a soft interrupt was not queued (#377).
#[derive(Debug, PartialEq, Eq)]
pub enum InterruptRefusal {
    /// No turn is running: there is nothing to steer, and a stale message
    /// must not wait around to hijack the next, unrelated turn.
    NotBusy,
    /// Blank, oversized, or the queue is full (see `tole_core::interrupt`).
    Invalid(String),
}

/// Queue a soft interrupt for the session's in-flight turn (#377). Shared
/// by the REST and MCP faces. The turn loop delivers it at its next safe
/// point; a turn that ends first leaves it queued for the next turn's
/// first safe point (the check below is a best-effort guard, not a lock).
pub fn interrupt_session(
    st: &SessionState,
    text: &str,
    urgent: bool,
) -> Result<(), InterruptRefusal> {
    if !*st.busy.lock().expect("busy lock") {
        return Err(InterruptRefusal::NotBusy);
    }
    st.cancel
        .interrupts()
        .push(text, urgent)
        .map_err(|e| InterruptRefusal::Invalid(e.to_string()))
}

/// Detach the session's in-flight turn (#217): stop waiting, keep the work.
/// Shared by the REST and MCP faces. `false` when no turn is running (a
/// stale flag must not detach the next, unrelated turn; `reset` also clears
/// it when a turn is claimed).
pub fn detach_session(st: &SessionState) -> bool {
    if !*st.busy.lock().expect("busy lock") {
        return false;
    }
    st.cancel.detach();
    true
}

/// Marks a session busy for its whole lifetime; Drop un-marks even on
/// panic, so one failed turn cannot brick the session.
pub struct BusyGuard(StdArc<Mutex<bool>>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        *self.0.lock().expect("busy lock") = false;
    }
}

/// The live session registry shared by every server face (REST, MCP,
/// ACP). It owns the map's invariants — the capacity cap and eviction,
/// the admission rule, and the per-turn busy claim — so the faces never
/// touch the map itself.
#[derive(Default)]
pub struct Sessions {
    map: HashMap<String, SessionState>,
}

pub type SharedSessions = StdArc<Mutex<Sessions>>;

/// Cap on the live in-memory session map, shared by every face (REST,
/// MCP, ACP). Sessions are durable on disk (the JSONL file) — eviction
/// drops only the in-memory handle, never the file.
pub const MAX_SESSIONS: usize = 256;

/// Every session in the map is mid-turn, so none can be evicted.
#[derive(Debug, PartialEq, Eq)]
pub struct AllSessionsBusy;

/// Why [`Sessions::admit`] refused a session.
#[derive(Debug, PartialEq, Eq)]
pub enum AdmitError {
    /// At capacity and every live session is mid-turn.
    AtCapacity,
    /// The id is already live and running a turn; replacing it would
    /// orphan that turn.
    Busy,
}

/// The faces' one wording for an omitted `session_id` with zero or several
/// sessions open.
pub const AMBIGUOUS_SESSION: &str =
    "missing 'session_id': pass it explicitly (or keep exactly one session open)";

impl Sessions {
    /// The live session `id`, if any.
    pub fn get(&self, id: &str) -> Option<&SessionState> {
        self.map.get(id)
    }

    /// Every live session, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &SessionState)> {
        self.map.iter()
    }

    /// True when `id` is live and running a turn.
    pub fn is_busy(&self, id: &str) -> bool {
        self.map
            .get(id)
            .is_some_and(|st| *st.busy.lock().expect("busy lock"))
    }

    /// Number of live sessions.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when no session is live.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The session a request addresses: the explicit id, or the only live
    /// session when none was given. Refused with [`AMBIGUOUS_SESSION`]
    /// otherwise.
    pub fn resolve_id(&self, explicit: Option<&str>) -> Result<String, String> {
        match explicit {
            Some(id) => Ok(id.to_string()),
            None if self.map.len() == 1 => Ok(self.map.keys().next().expect("len == 1").clone()),
            None => Err(AMBIGUOUS_SESSION.to_string()),
        }
    }

    /// Request cancellation of `id`'s in-flight turn; false for an unknown
    /// session.
    pub fn cancel(&self, id: &str) -> bool {
        self.map.get(id).map(|st| st.cancel.cancel()).is_some()
    }

    /// Make room for a new session BEFORE it is opened (opening creates
    /// its file on disk, so refusing afterwards would orphan it). Returns
    /// the evicted ids; [`Sessions::admit`] re-checks under the same lock
    /// as the insert, for a concurrent creator.
    pub fn ensure_room(&mut self) -> Result<Vec<String>, AllSessionsBusy> {
        self.evict_for_insert(MAX_SESSIONS)
    }

    /// Admit an opened session under `id`. A live, idle session with the
    /// same id is replaced in place (a re-open); a busy one is refused. A
    /// new id evicts an idle session when the map is at capacity
    /// ([`MAX_SESSIONS`]). Returns the evicted ids so callers can prune
    /// their side maps.
    pub fn admit(&mut self, id: String, state: SessionState) -> Result<Vec<String>, AdmitError> {
        self.admit_capped(id, state, MAX_SESSIONS)
    }

    fn admit_capped(
        &mut self,
        id: String,
        state: SessionState,
        max: usize,
    ) -> Result<Vec<String>, AdmitError> {
        let evicted = match self.map.get(&id) {
            Some(live) if *live.busy.lock().expect("busy lock") => return Err(AdmitError::Busy),
            Some(_) => Vec::new(),
            None => self
                .evict_for_insert(max)
                .map_err(|AllSessionsBusy| AdmitError::AtCapacity)?,
        };
        self.map.insert(id, state);
        Ok(evicted)
    }

    /// Claim `id` for one turn: mark it busy and clear a stale cancel in
    /// the same critical section (#178) — a cancel that lands after the
    /// claim targets THIS turn; one that landed before had no turn to
    /// target. The returned guard un-marks the session when dropped, even
    /// on panic.
    pub fn claim_turn(&self, id: &str) -> Result<(SessionState, BusyGuard), String> {
        let state = self
            .map
            .get(id)
            .ok_or_else(|| format!("unknown session: {id}"))?;
        let mut busy = state.busy.lock().expect("busy lock");
        if *busy {
            return Err("session is busy running a turn".into());
        }
        *busy = true;
        state.cancel.reset();
        drop(busy);
        Ok((state.clone(), BusyGuard(StdArc::clone(&state.busy))))
    }

    /// Make room for one more session: while the map is at `max`, evict
    /// one NON-busy session. A busy flag that is itself locked means a
    /// turn is mid-flight on that session, so it is skipped via
    /// `try_lock` without blocking the whole map (CodeCora scan
    /// round-2). Returns the evicted ids so callers can prune any side
    /// maps keyed by session id; `Err(AllSessionsBusy)` when at capacity
    /// with nothing evictable.
    fn evict_for_insert(&mut self, max: usize) -> Result<Vec<String>, AllSessionsBusy> {
        let mut evicted = Vec::new();
        while self.map.len() >= max {
            let oldest = self
                .map
                .iter()
                .filter(|(_, st)| matches!(st.busy.try_lock().as_deref().copied(), Ok(false)))
                .map(|(id, _)| id.clone())
                .next();
            match oldest {
                Some(id) => {
                    self.map.remove(&id);
                    evicted.push(id);
                }
                None => return Err(AllSessionsBusy),
            }
        }
        Ok(evicted)
    }
}

/// Poisoning-tolerant lock: one panicking turn must not brick the whole
/// server (CodeCora scan finding — mutex poisoning).
pub fn lock_sessions(sessions: &SharedSessions) -> std::sync::MutexGuard<'_, Sessions> {
    sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// ACP/serve session ids are tole session ids: reject path separators,
/// parent refs, and anything outside the tole charset before the id ever
/// touches a path (CodeCora scan finding: `../` or absolute ids would
/// escape the sessions dir via Path::join).
pub fn validate_session_id(id: &str) -> Option<String> {
    let ok = tole_core::storage::is_valid_session_id(id);
    if ok {
        Some(id.to_string())
    } else {
        None
    }
}

pub fn new_session_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Monotonic counter suffix: ms+pid alone can collide when two
    // sessions are created within the same millisecond of the same
    // process (CodeCora scan-3: collision → silent session overwrite).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{prefix}-{ms:x}-{n:x}-{:x}", std::process::id())
}

/// Create/open a session: workspace jail = the client-provided cwd;
/// approval = the caller-provided approver (ACP: the interactive editor;
/// serve: the non-interactive allowlist).
///
/// `sessions_dir_override` honors an explicit `--sessions-dir` from the
/// host (None keeps the per-session-cwd default `<cwd>/.tole/sessions`).
/// `turnend` wires `--on-turnend` stop gates onto the session's registry
/// (empty = off).
#[allow(clippy::too_many_arguments)]
pub fn open_session(
    session_id: &str,
    cwd: &str,
    loading: bool,
    plan_mode: bool,
    approver: impl tole_core::approval::Approver + 'static,
    memory: Option<MemoryConfig>,
    sessions_dir_override: Option<&std::path::Path>,
    turnend: Vec<String>,
    parent_allows: Vec<String>,
    cancel: tole_core::cancel::CancelToken,
    plan_emitter: Option<PlanEmitter>,
) -> Result<SessionState, String> {
    let workspace = PathBuf::from(cwd);
    let workspace_canon = workspace
        .canonicalize()
        .map_err(|e| format!("session workspace {}: {e}", workspace.display()))?;
    // TOLE_SYSTEM_PROMPT env > project config (#208; absent unless a
    // trusted config installed it) > none.
    let system_prompt = std::env::var("TOLE_SYSTEM_PROMPT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(crate::config_apply::config_system_prompt);

    // The shared tool roster (#420): server faces jail files AND shell
    // tools to the session workspace, may start child agents (not in
    // plan mode, never from a child), and publish plans when the
    // transport renders them. delete_file follows the approver: only an
    // interactive one (ACP) gets it.
    let mut roster = crate::tool_roster::Roster::new(workspace_canon);
    roster.plan_mode = plan_mode;
    roster.agents = Some(crate::tool_roster::AgentTools {
        worktrees: false,
        parent_allows,
    });
    roster.plan_emitter = plan_emitter;
    // #217: a job that exits nudges this session at its next safe point.
    roster.job_notices = Some(cancel.interrupts().clone());
    let mut reg = crate::tool_roster::build(approver, roster).map_err(|e| e.to_string())?;
    // Turn-end stop gates (#145): the same registry-level wiring the
    // run/chat hosts use, applied to server-face sessions.
    if !turnend.is_empty() {
        let mut hooks = tole_core::hooks::ToolHooks::from_cli(&[], &[]);
        hooks.turnend = turnend
            .iter()
            .map(|c| tole_core::hooks::turnend_hook(c))
            .collect();
        reg.set_hooks(hooks);
    }

    let dir = match sessions_dir_override {
        Some(d) => {
            std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
            d.to_path_buf()
        }
        None => sessions_dir_for(cwd)?,
    };
    let storage = if loading {
        tole_core::storage::JsonlStorage::open(dir.join(format!("{session_id}.jsonl")))
            .map_err(|e| format!("loading session: {e}"))?
    } else {
        create_session(
            &dir,
            session_id,
            system_prompt.as_deref(),
            None,
            Some("server"),
        )
        .map_err(|e| format!("creating session: {e}"))?
    };
    // Task-list tools (issue #198): registered after the storage open —
    // the state is seeded from the replayed transcript, so crash-resume
    // restores the last list; todo_write is absent under --plan-mode.
    crate::tool_roster::register_todo(&mut reg, storage.entries(), plan_mode)
        .map_err(|e| e.to_string())?;

    Ok(SessionState {
        storage: StdArc::new(Mutex::new(storage)),
        path: dir.join(format!("{session_id}.jsonl")),
        registry: StdArc::new(reg),
        system_prompt,
        memory,
        first_prompt_done: StdArc::new(Mutex::new(loading)),
        busy: StdArc::new(Mutex::new(false)),
        cancel,
        tool_ids: StdArc::new(Mutex::new(0)),
    })
}

/// Create a session file in `dir` and register it in the global session
/// index under `face` (`None` = do not index, e.g. tests that must stay off
/// the user's index). The one place a new session is created AND registered,
/// so a face cannot create one and forget the other (#525). Errors are the
/// storage's own; each caller adds its own context.
pub fn create_session(
    dir: &std::path::Path,
    id: &str,
    system_prompt: Option<&str>,
    name: Option<&str>,
    face: Option<&str>,
) -> Result<tole_core::storage::JsonlStorage, tole_core::storage::StorageError> {
    let storage =
        tole_core::storage::JsonlStorage::create_named(dir, id, None, system_prompt, name)?;
    if let Some(face) = face {
        crate::session_index::record(dir, id, face);
    }
    Ok(storage)
}

pub fn sessions_dir_for(cwd: &str) -> Result<PathBuf, String> {
    let dir = PathBuf::from(cwd).join(".tole/sessions");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// One prompt = one full tole turn. Returns `(stop_reason, final_text)`;
/// the transport decides how to deliver them (ACP: notification chunk +
/// response; serve: the HTTP response body).
pub fn run_session_turn(
    sessions: SharedSessions,
    session_id: &str,
    prompt: &str,
    observer: Option<StdArc<dyn tole_core::turn::TurnObserver>>,
) -> Result<(String, Option<String>), String> {
    // Brief map lock: claim the session for this turn. The MAP lock is
    // released here — a running turn holds only its OWN storage lock, so
    // the transport loop stays live (CodeCora deadlock finding).
    let (state, _busy_guard) = lock_sessions(&sessions).claim_turn(session_id)?;
    let SessionState {
        storage,
        registry,
        memory,
        system_prompt,
        first_prompt_done,
        cancel,
        ..
    } = state;
    let mut final_text: Option<String> = None;

    // Memory loop, pre-turn (first prompt of a fresh session only).
    let effective = if *first_prompt_done.lock().expect("fpd lock") {
        prompt.to_string()
    } else {
        crate::turn_runner::with_recalled_memory(memory.as_ref(), prompt)
    };

    // Lock the storage for the rest of the turn. The provider is built
    // fresh every turn (issue #176): a `fact/model` register override —
    // set durably by `tole acp`'s session/set_config_option — wins over
    // the env default. serve sessions have no setter today, so this is
    // a no-op there.
    let mut storage = storage.lock().unwrap_or_else(|p| p.into_inner());
    let cfg = crate::turn_runner::provider_config()?;
    // Live-token sinks (issue #196 phase 3): when an observer is wired
    // (ACP), streamed reasoning/content deltas flow to it DURING the
    // provider call; the decisive output semantics are unchanged. The same
    // configuration applies to a reserve provider (#209).
    let mut provider = crate::turn_runner::session_provider(
        cfg,
        &*storage,
        registry.specs(),
        system_prompt.as_deref(),
        observer.as_ref(),
    )?;

    let outcome = tole_core::turn::run_turn_with_observer(
        &mut *storage,
        &mut provider,
        &registry,
        &effective,
        &cancel,
        observer.as_ref().map(StdArc::as_ref),
    )
    .map_err(|e| e.to_string())?;

    #[cfg(feature = "shell-tools")]
    if let tole_core::turn::TurnOutcome::Final { text, wrote } = &outcome {
        crate::turn_runner::remember(memory.as_ref(), session_id, prompt, text, *wrote);
    }
    *first_prompt_done.lock().expect("fpd lock") = true;

    if let tole_core::turn::TurnOutcome::Final { text, .. } = &outcome {
        final_text = Some(text.clone());
    }
    log_outcome(&outcome);
    let stop = stop_reason(&outcome)?;
    Ok((stop.to_string(), final_text))
}

/// Approvals-only recovery over the serve face (issue #200): no new
/// prompt — `resume_turn` settles the parked guarded effect (its
/// approver consult happens with the one-shot remote approval in
/// place) and replans to completion. Mirrors the provider/model
/// construction of [`run_session_turn`]; the memory loop does not
/// participate (no new user message).
pub fn resume_session_turn(
    sessions: SharedSessions,
    session_id: &str,
) -> Result<(String, Option<String>), String> {
    let (state, _busy_guard) = lock_sessions(&sessions).claim_turn(session_id)?;
    let SessionState {
        storage,
        registry,
        system_prompt,
        cancel,
        ..
    } = state;
    let mut storage = storage.lock().unwrap_or_else(|p| p.into_inner());
    let cfg = crate::turn_runner::provider_config()?;
    let mut provider = crate::turn_runner::session_provider(
        cfg,
        &*storage,
        registry.specs(),
        system_prompt.as_deref(),
        None,
    )?;
    // #318: the session token is wired into the replan loop, so
    // `/cancel` during an approval-resume settles the turn as cancelled.
    // The `reset()` above stays: like `run_session_turn`, it runs inside
    // the busy claim, so a cancel landing after the claim targets this
    // resume, while one that landed earlier had no in-flight turn to
    // target (cancel is unconditional) and must not kill it.
    let outcome =
        tole_core::turn::resume_turn_with_cancel(&mut *storage, &mut provider, &registry, &cancel)
            .map_err(|e| e.to_string())?;
    let final_text = match &outcome {
        tole_core::turn::TurnOutcome::Final { text, .. } => Some(text.clone()),
        _ => None,
    };
    log_outcome(&outcome);
    Ok((stop_reason(&outcome)?, final_text))
}

/// Single stop-reason mapping for `run_session_turn` and
/// `resume_session_turn` (#318) so the two cannot drift. A storage error
/// outcome is surfaced as `Err`.
fn stop_reason(outcome: &tole_core::turn::TurnOutcome) -> Result<String, String> {
    use tole_core::turn::TurnOutcome as O;
    Ok(match outcome {
        O::Final { .. } => "end_turn",
        O::BudgetExhausted => "max_tokens",
        O::Cancelled => "cancelled",
        O::Detached => "detached",
        O::StopGateBlocked { .. }
        | O::ApprovalRequired { .. }
        | O::UnknownTool { .. }
        | O::ProviderFailed { .. }
        | O::LoopDetected { .. } => "refusal",
        O::Storage(e) => return Err(format!("storage error: {e}")),
    }
    .to_string())
}

/// Operator-visible stderr line for non-final outcomes (unchanged text).
fn log_outcome(outcome: &tole_core::turn::TurnOutcome) {
    use tole_core::turn::TurnOutcome as O;
    match outcome {
        O::StopGateBlocked { reason } => eprintln!("tole: stop gate blocked the turn: {reason}"),
        O::ApprovalRequired { name } => eprintln!("tole: approval denied for '{name}'"),
        O::UnknownTool { name } => eprintln!("tole: unknown tool '{name}'"),
        O::ProviderFailed { message } => eprintln!("tole: provider failed: {message}"),
        O::LoopDetected { tool, .. } => eprintln!("tole: loop detected on '{tool}'"),
        _ => {}
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("tole-evict-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state(dir: &std::path::Path, name: &str, busy: bool) -> SessionState {
        let storage = tole_core::storage::JsonlStorage::create(dir, name, None).unwrap();
        SessionState {
            storage: StdArc::new(Mutex::new(storage)),
            path: dir.join(format!("{name}.jsonl")),
            registry: StdArc::new(tole_core::tool::ToolRegistry::new()),
            system_prompt: None,
            memory: None,
            first_prompt_done: StdArc::new(Mutex::new(false)),
            busy: StdArc::new(Mutex::new(busy)),
            cancel: tole_core::cancel::CancelToken::default(),
            tool_ids: StdArc::new(Mutex::new(0)),
        }
    }

    #[test]
    fn admit_replaces_an_idle_id_refuses_a_busy_one_and_evicts_for_a_new_one() {
        let dir = TmpDir::new("admit");
        let mut s = Sessions::default();
        assert_eq!(
            s.admit_capped("a".into(), state(dir.path(), "a", false), 2),
            Ok(vec![])
        );
        // Re-open of an idle id: replaced in place, nothing evicted.
        assert_eq!(
            s.admit_capped("a".into(), state(dir.path(), "a2", false), 2),
            Ok(vec![])
        );
        assert_eq!(s.len(), 1);
        assert_eq!(
            s.admit_capped("b".into(), state(dir.path(), "b", true), 2),
            Ok(vec![])
        );
        // A busy live id is never swapped out from under its turn.
        assert_eq!(
            s.admit_capped("b".into(), state(dir.path(), "b2", false), 2),
            Err(AdmitError::Busy)
        );
        // A new id at capacity evicts the idle one, never the busy one.
        assert_eq!(
            s.admit_capped("c".into(), state(dir.path(), "c", true), 2),
            Ok(vec!["a".to_string()])
        );
        assert!(s.get("b").is_some() && s.get("c").is_some());
        assert_eq!(
            s.admit_capped("d".into(), state(dir.path(), "d", false), 2),
            Err(AdmitError::AtCapacity)
        );
    }

    #[test]
    fn claim_turn_marks_busy_resets_a_stale_cancel_and_releases_on_drop() {
        let dir = TmpDir::new("claim");
        let mut s = Sessions::default();
        s.admit("a".into(), state(dir.path(), "a", false)).unwrap();
        assert!(s.claim_turn("nope").is_err());
        assert!(s.cancel("a"), "a stale cancel before the turn");
        let (st, guard) = s.claim_turn("a").unwrap();
        assert!(!st.cancel.is_cancelled(), "the claim clears it");
        assert!(s.is_busy("a"));
        assert_eq!(
            s.claim_turn("a").err().as_deref(),
            Some("session is busy running a turn")
        );
        drop(guard);
        assert!(!s.is_busy("a"));
        assert!(!s.cancel("nope"));
    }

    #[test]
    fn resolve_id_uses_the_explicit_id_or_the_only_session() {
        let dir = TmpDir::new("resolve");
        let mut s = Sessions::default();
        assert_eq!(s.resolve_id(None), Err(AMBIGUOUS_SESSION.to_string()));
        s.admit("a".into(), state(dir.path(), "a", false)).unwrap();
        assert_eq!(s.resolve_id(None), Ok("a".to_string()));
        assert_eq!(s.resolve_id(Some("x")), Ok("x".to_string()));
        s.admit("b".into(), state(dir.path(), "b", false)).unwrap();
        assert_eq!(s.resolve_id(None), Err(AMBIGUOUS_SESSION.to_string()));
    }

    #[test]
    fn below_cap_evicts_nothing() {
        let dir = TmpDir::new("below");
        let mut s = Sessions::default();
        s.map.insert("a".into(), state(dir.path(), "a", false));
        assert_eq!(s.evict_for_insert(2), Ok(vec![]));
        assert_eq!(s.map.len(), 1);
    }

    #[test]
    fn at_cap_evicts_an_idle_session_and_reports_its_id() {
        let dir = TmpDir::new("atcap");
        let mut s = Sessions::default();
        s.map.insert("busy".into(), state(dir.path(), "busy", true));
        s.map
            .insert("idle".into(), state(dir.path(), "idle", false));
        let evicted = s.evict_for_insert(2).unwrap();
        assert_eq!(evicted, vec!["idle".to_string()]);
        assert!(s.map.contains_key("busy"), "busy session must survive");
        assert_eq!(s.map.len(), 1);
    }

    #[test]
    fn all_busy_at_cap_errors_and_evicts_nothing() {
        let dir = TmpDir::new("allbusy");
        let mut s = Sessions::default();
        s.map.insert("a".into(), state(dir.path(), "a", true));
        s.map.insert("b".into(), state(dir.path(), "b", true));
        assert_eq!(s.evict_for_insert(2), Err(AllSessionsBusy));
        assert_eq!(s.map.len(), 2);
    }

    #[test]
    fn locked_busy_flag_counts_as_busy() {
        let dir = TmpDir::new("locked");
        let mut s = Sessions::default();
        s.map.insert("a".into(), state(dir.path(), "a", false));
        let flag = s.map["a"].busy.clone();
        let _held = flag.lock().unwrap();
        assert_eq!(s.evict_for_insert(1), Err(AllSessionsBusy));
    }
}

#[cfg(test)]
mod stop_reason_tests {
    use super::stop_reason;
    use tole_core::turn::TurnOutcome as O;

    #[test]
    fn maps_every_outcome_once_for_run_and_resume() {
        let s = |o: O| stop_reason(&o).unwrap();
        assert_eq!(
            s(O::Final {
                text: "t".into(),
                wrote: false
            }),
            "end_turn"
        );
        assert_eq!(s(O::BudgetExhausted), "max_tokens");
        assert_eq!(s(O::Cancelled), "cancelled");
        for o in [
            O::StopGateBlocked { reason: "r".into() },
            O::ApprovalRequired { name: "n".into() },
            O::UnknownTool { name: "n".into() },
            O::ProviderFailed {
                message: "m".into(),
            },
            O::LoopDetected {
                tool: "t".into(),
                count: 3,
            },
        ] {
            assert_eq!(s(o), "refusal");
        }
    }
}

#[cfg(test)]
mod roster_tests {
    use super::*;
    use tole_core::approval::AllowlistApprover;
    use tole_core::tool::Risk;

    fn open(tag: &str, plan_mode: bool) -> SessionState {
        let ws = std::env::temp_dir().join(format!("tole-sh-roster-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        open_session(
            "s-roster",
            ws.to_str().unwrap(),
            false,
            plan_mode,
            AllowlistApprover::allow_only(vec![]),
            None,
            Some(&ws.join("sessions")),
            Vec::new(),
            Vec::new(),
            tole_core::cancel::CancelToken::default(),
            None,
        )
        .unwrap()
    }

    /// #420: server sessions build their tools through the shared roster —
    /// the same rules as the CLI (verify_package present; delete_file absent
    /// behind a non-interactive approver; plan mode leaves ReadOnly only).
    #[test]
    fn server_sessions_use_the_shared_roster() {
        let s = open("full", false);
        assert!(s.registry.get("verify_package").is_some());
        assert!(s.registry.get("write_file").is_some());
        assert!(s.registry.get("todo_write").is_some());
        assert!(s.registry.get("delete_file").is_none());

        let p = open("plan", true);
        assert!(p.registry.get("read_file").is_some());
        assert!(p.registry.get("todo_read").is_some());
        for name in ["write_file", "run_command", "agent_start", "todo_write"] {
            assert!(p.registry.get(name).is_none(), "{name} in plan mode");
        }
        assert!(p
            .registry
            .specs()
            .iter()
            .filter_map(|s| s["function"]["name"].as_str().or(s["name"].as_str()))
            .all(|n| p.registry.get(n).map(|t| t.risk()) == Some(Risk::ReadOnly)));
    }
}
