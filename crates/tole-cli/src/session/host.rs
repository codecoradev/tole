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

use crate::webhook_out::turn::{TurnCode, TurnEnd};

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
    /// The model this session is pinned to (a `tole serve` per-session
    /// profile, #740); `None` = the server default.
    pub model: Option<String>,
    /// The per-session profile's name (#740), fixed for this live session;
    /// `None` = opened without one.
    pub profile: Option<String>,
    /// The last-known status snapshot (#573), refreshed whenever the session
    /// is read idle, a turn settles or a register changes. A separate lock,
    /// held only to copy or store, so the status route can read it while a
    /// turn holds `storage`.
    pub status: StdArc<Mutex<crate::session::status::StatusCell>>,
}

impl SessionState {
    /// The configured model this session falls back to: its profile's, else
    /// the provider configuration's (`None` when none resolves).
    fn default_model(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| crate::turn_runner::provider_config().ok().map(|c| c.model))
            .filter(|m| !m.trim().is_empty())
    }

    /// Re-read the status from `storage` (the caller holds its lock) and keep
    /// it as the last-known snapshot. Pure: no network, no other lock than
    /// the small status cell.
    pub fn refresh_status(&self, storage: &impl Storage) -> crate::session::status::StatusSnapshot {
        let default = self.default_model();
        let mut cell = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let snap = crate::session::status::take_snapshot(storage, default.as_deref(), &cell);
        cell.store(snap.clone());
        snap
    }

    /// The last-known snapshot, never touching `storage`.
    pub fn last_status(&self) -> Option<crate::session::status::StatusSnapshot> {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot()
            .cloned()
    }

    /// Ask the provider for the context window once the session has a prompt
    /// to measure and none is known (a resumed session, or the first turn
    /// just settled), then refresh the snapshot. Blocks on the network, so
    /// callers run it off the request path. Never waits for a running turn.
    pub fn warm_status(&self) {
        let Ok(mut cfg) = crate::turn_runner::provider_config() else {
            return;
        };
        if let Some(m) = self.model.as_deref().filter(|m| !m.trim().is_empty()) {
            cfg.model = m.to_string();
        }
        let model = {
            let Ok(g) = self.storage.try_lock() else {
                return;
            };
            if tole_core::compaction::last_prompt_tokens(&*g).is_none() {
                return;
            }
            crate::turn_runner::session_model(&*g, &cfg.model)
        };
        if crate::session::status::detect_window(&self.status, &cfg, &model) {
            if let Ok(g) = self.storage.try_lock() {
                self.refresh_status(&*g);
            }
        }
    }
}

/// What a host may fix for ONE session instead of taking the process-wide
/// values (#740: a `tole serve` per-session profile). The default changes
/// nothing, so ACP, MCP and a profile-less serve session behave as before.
#[derive(Debug, Clone, Default)]
pub struct SessionOverrides {
    /// Replaces the persona chain (profile > env > config) when `Some`.
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
}

static SERVER_SKILLS: std::sync::OnceLock<crate::session::assemble::ServerSkills> =
    std::sync::OnceLock::new();

/// Fix the process's skills choice for server sessions (#945), once at
/// startup: the owner's `--skill` files and `--no-skills`. Unset means no
/// explicit skills (approved workspace skills still load). The first call
/// wins.
pub fn set_server_skills(skills: crate::session::assemble::ServerSkills) {
    let _ = SERVER_SKILLS.set(skills);
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
    open_session_with(
        session_id,
        cwd,
        loading,
        plan_mode,
        approver,
        memory,
        sessions_dir_override,
        turnend,
        parent_allows,
        cancel,
        plan_emitter,
        SessionOverrides::default(),
    )
}

/// [`open_session`] with per-session `overrides` (#740).
#[allow(clippy::too_many_arguments)]
pub fn open_session_with(
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
    overrides: SessionOverrides,
) -> Result<SessionState, String> {
    // An unset cell is an error, never a silent default: a default would lose
    // the owner's `--no-skills` for an entrypoint that forgot to set it.
    let skills = match SERVER_SKILLS.get() {
        Some(s) => s.clone(),
        #[cfg(test)]
        None => Default::default(),
        #[cfg(not(test))]
        None => {
            return Err(
                "server skills are not configured: the entrypoint must call \
                        session_host::set_server_skills before opening a session"
                    .into(),
            )
        }
    };
    // Same rule for the owner's MCP servers (#946): an unset cell is an error,
    // never a silent "none".
    let mcp: &[String] = match crate::session::owner_mcp::get() {
        Some(m) => m,
        #[cfg(test)]
        None => &[],
        #[cfg(not(test))]
        None => {
            return Err(
                "the owner's MCP servers are not configured: the entrypoint must call \
                        session::owner_mcp::set before opening a session"
                    .into(),
            )
        }
    };
    open_session_inner(
        session_id,
        cwd,
        loading,
        plan_mode,
        approver,
        memory,
        sessions_dir_override,
        turnend,
        parent_allows,
        cancel,
        plan_emitter,
        overrides,
        &skills,
        mcp,
    )
}

/// [`open_session_with`] under an explicit skills choice (tests inject one).
#[allow(clippy::too_many_arguments)]
fn open_session_inner(
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
    overrides: SessionOverrides,
    skills: &crate::session::assemble::ServerSkills,
    owner_mcp: &[String],
) -> Result<SessionState, String> {
    let workspace = PathBuf::from(cwd);
    let workspace_canon = workspace
        .canonicalize()
        .map_err(|e| format!("session workspace {}: {e}", workspace.display()))?;
    // An explicit prompt: the selected profile's persona (#584) >
    // TOLE_SYSTEM_PROMPT env > project config (#208; absent unless a trusted
    // config installed it). With none, a NEW session gets the shared default
    // prompt below (#945), as the CLI does.
    let explicit_prompt = overrides
        .system_prompt
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(crate::config_apply::profile_system_prompt)
        .or_else(|| {
            std::env::var("TOLE_SYSTEM_PROMPT")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(crate::config_apply::config_system_prompt);

    // The shared tool roster (#420): server faces jail files AND shell
    // tools to the session workspace, may start child agents (not in
    // plan mode, never from a child), and publish plans when the
    // transport renders them. delete_file follows the approver: only an
    // interactive one (ACP) gets it.
    let mut roster = crate::session::assemble::roster_for(
        &crate::session::assemble::FacePolicy::SERVER_SESSION,
        crate::session::assemble::RosterInputs {
            workspace: workspace_canon.clone(),
            plan_mode,
            allow_patterns: &parent_allows,
            agents_worktree: false,
            delegation: None,
            // The owner's servers, read at startup (#946): never from the
            // request or its overrides.
            mcp_specs: owner_mcp,
        },
    )
    .map_err(|e| e.to_string())?;
    #[cfg(feature = "mcp")]
    let mcp_notices = {
        let sink = crate::tool_roster::McpNoticeSink::default();
        roster.mcp_notices = Some(sink.clone());
        // Every MCP tool on a server face is Write behind the approval gate:
        // no ReadOnly `mcp_read` (#946).
        roster.mcp_resources = false;
        sink
    };
    roster.plan_emitter = plan_emitter;
    roster.session = Some(session_id.to_string());
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
    // A new session pins its prompt in the header. A loaded one keeps the
    // header's unless an explicit prompt applies (a profile persona chosen on
    // resume, #584, still takes over as before). Append-only: a session
    // created before #945 has no pinned prompt and does not gain the default.
    let (storage, system_prompt) = if loading {
        let st = tole_core::storage::JsonlStorage::open(dir.join(format!("{session_id}.jsonl")))
            .map_err(|e| format!("loading session: {e}"))?;
        let prompt = explicit_prompt.or_else(|| st.system_prompt().map(str::to_string));
        // Append-only (#945): a resumed session keeps its pinned prompt and
        // gains no skills section. When that prompt already lists skills,
        // `load_skill` is registered again so the listing stays usable.
        if prompt
            .as_deref()
            .is_some_and(crate::session::assemble::prompt_has_skills_index)
        {
            crate::session::assemble::server_session_skills(skills, &workspace_canon, &mut reg)
                .map_err(|e| e.to_string())?;
        }
        (st, prompt)
    } else {
        // Skills (#945): approved ones only, from the session workspace; the
        // sections append to the pinned prompt, `load_skill` joins the
        // registry. Before the session file exists, so a broken explicit
        // skill leaves nothing behind.
        let sections =
            crate::session::assemble::server_session_skills(skills, &workspace_canon, &mut reg)
                .map_err(|e| e.to_string())?;
        let prompt = format!(
            "{}{sections}",
            crate::session::assemble::server_session_prompt(
                explicit_prompt,
                plan_mode,
                &workspace_canon,
                &reg,
            )
        );
        let st = create_session(&dir, session_id, Some(&prompt), None, Some("server"))
            .map_err(|e| format!("creating session: {e}"))?;
        (st, Some(prompt))
    };
    // A server the lock refused (or that did not start) is reported once in
    // the session log, where a client reads it with the entries (#946).
    #[cfg(feature = "mcp")]
    let storage = {
        let mut storage = storage;
        let notices = mcp_notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        record_mcp_notices(&mut storage, &notices)?;
        storage
    };
    // Task-list tools (issue #198): registered after the storage open —
    // the state is seeded from the replayed transcript, so crash-resume
    // restores the last list; todo_write is absent under --plan-mode.
    crate::tool_roster::register_todo(&mut reg, storage.entries(), plan_mode)
        .map_err(|e| e.to_string())?;

    let state = SessionState {
        status: StdArc::default(),
        storage: StdArc::new(Mutex::new(storage)),
        path: dir.join(format!("{session_id}.jsonl")),
        registry: StdArc::new(reg),
        system_prompt,
        memory,
        first_prompt_done: StdArc::new(Mutex::new(loading)),
        busy: StdArc::new(Mutex::new(false)),
        cancel,
        tool_ids: StdArc::new(Mutex::new(0)),
        model: overrides.model,
        profile: overrides.profile,
    };
    state.refresh_status(&*state.storage.lock().unwrap_or_else(|p| p.into_inner()));
    Ok(state)
}

/// Routes a server session at its first prompt, before any provider call: the
/// CLI's semantics through the one shared step. Off unless the OWNER's config
/// enables `[router]`; a session that already has a model (client-set through
/// the model route, a profile, resume) is left alone; a broken router fails
/// this prompt with its redacted remedy. A request cannot touch the router.
fn route_first_prompt(
    storage: &mut dyn Storage,
    profile: Option<&str>,
    prompt: &str,
    registry: &tole_core::tool::ToolRegistry,
) -> Result<(), String> {
    // A config that no longer loads fails the prompt too, redacted like a
    // router error (its text can echo a value the owner wrote).
    let secrets = crate::provider_chain::process_secrets(&crate::config_apply::real_env);
    let cfg = match crate::config::user_config_path() {
        Some(p) => crate::config::user_config_present(&p)
            .and_then(|present| {
                if present {
                    crate::config::load(&p).map(Some).map_err(|e| e.to_string())
                } else {
                    Ok(None)
                }
            })
            .map_err(|e| secrets.redact(&e).into_owned())?,
        None => None,
    }
    .unwrap_or_default();
    let setup = crate::delegate::Setup {
        cfg,
        ..Default::default()
    };
    crate::session::assemble::route_session(
        storage,
        &setup,
        profile,
        tole_core::router::TaskKind::Chat,
        prompt,
        registry,
        false,
        None,
    )
    // `route_session` redacts its own error with the process secrets
    // (assemble.rs), so this text is already safe to return.
    .map_err(|e| e.to_string())
}

/// Appends one `error` entry per MCP server that did not start (#946): the
/// server's name and the reason, no secret (the reason is the lock's own
/// text). A root entry the model wire drops, so it never reaches the model.
#[cfg(feature = "mcp")]
fn record_mcp_notices(
    storage: &mut tole_core::storage::JsonlStorage,
    notices: &[crate::tool_roster::McpNotice],
) -> Result<(), String> {
    use tole_core::entry::{EntryType, NewEntry};
    for n in notices {
        let text = format!(
            "MCP server `{}` was not started for this session: {}",
            n.server, n.reason
        );
        // Redacted with the process secrets, as every egress of the log.
        let text = crate::provider_chain::process_secrets(&crate::config_apply::real_env)
            .redact(&text)
            .into_owned();
        // A resume re-opens the session: the same server and reason is not
        // appended again (a changed reason is).
        let seen = storage.entries().iter().any(|e| {
            e.payload["notice"] == "mcp_server_not_started"
                && e.payload["server"] == n.server.as_str()
                && e.payload["error"] == text.as_str()
        });
        if seen {
            continue;
        }
        storage
            .commit(tole_core::storage::Commit::new().entry(NewEntry::root(
                EntryType::new(EntryType::ERROR),
                serde_json::json!({
                    "error": text,
                    "notice": "mcp_server_not_started",
                    "server": n.server,
                }),
            )))
            .map_err(|e| format!("recording the MCP notice: {e}"))?;
    }
    Ok(())
}

/// The provider config of one turn: the process-wide one, with the session's
/// pinned model (#740) when it has one.
fn session_provider_config(model: Option<&str>) -> Result<tole_core::openai::OpenAiConfig, String> {
    let mut cfg = crate::turn_runner::provider_config()?;
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        cfg.model = m.to_string();
    }
    Ok(cfg)
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
    run_session_turn_with_images(sessions, session_id, prompt, &[], observer)
}

/// [`run_session_turn`] with images on this turn's user message (#559):
/// ACP `image` content blocks and `serve`'s prompt route (#558), already
/// resolved to durable references.
pub fn run_session_turn_with_images(
    sessions: SharedSessions,
    session_id: &str,
    prompt: &str,
    images: &[tole_core::image::ImageRef],
    observer: Option<StdArc<dyn tole_core::turn::TurnObserver>>,
) -> Result<(String, Option<String>), String> {
    run_session_turn_detailed(sessions, session_id, prompt, images, observer, None)
        .map(|r| (r.stop, r.text))
}

/// A settled turn with what only `serve` needs on top of the ACP/MCP pair
/// (#571): which tool the turn stopped on when it waits for an approval.
/// `stop` keeps the ACP vocabulary (`refusal` for an approval stop).
#[non_exhaustive]
pub struct TurnResult {
    pub stop: String,
    pub text: Option<String>,
    /// `Some(tool)` when the outcome was `ApprovalRequired`.
    pub approval_tool: Option<String>,
    /// [`crate::approvals::ApprovalQueue::mark`] taken right after the turn
    /// claimed the session (`Some` when the caller passed a queue): an entry
    /// of this session newer than it was queued by this turn.
    pub queue_mark: Option<u64>,
    /// The typed end of the turn for the `turn_finished` webhook (#962): every
    /// payload string dropped. `None` when the end is not announced.
    pub turn_end: Option<TurnEnd>,
}

/// A turn that returned an error. `message` is what the HTTP face reports.
/// `end` is `Some` only when the turn had started (the session was claimed):
/// a busy session or an unknown id never produce one, so nothing is announced.
/// It carries no text; `message` must never be copied into an event.
#[derive(Debug)]
pub struct TurnError {
    pub message: String,
    pub end: Option<TurnEnd>,
}

impl TurnError {
    fn before_turn(message: String) -> TurnError {
        TurnError { message, end: None }
    }

    fn failed(message: String, code: TurnCode, storage: &dyn Storage) -> TurnError {
        TurnError {
            message,
            end: Some(TurnEnd::failed(code, last_seq(storage))),
        }
    }
}

/// The `seq` of the newest entry (0 when the log is empty): the position
/// `GET /sessions/{id}/entries?after=` pages by.
fn last_seq(storage: &dyn Storage) -> u64 {
    storage.entries().last().map_or(0, |e| e.seq)
}

/// [`run_session_turn_with_images`] plus the approval tool of an
/// approval stop. Only the serve prompt route reads the extra fields; it
/// passes its approval queue so the snapshot is taken AFTER the busy claim
/// (an approvals-resume thread claims the same session and could queue an
/// entry between an earlier snapshot and our claim).
pub fn run_session_turn_detailed(
    sessions: SharedSessions,
    session_id: &str,
    prompt: &str,
    images: &[tole_core::image::ImageRef],
    observer: Option<StdArc<dyn tole_core::turn::TurnObserver>>,
    approvals: Option<&crate::approvals::ApprovalQueue>,
) -> Result<TurnResult, String> {
    run_session_turn_with_end(sessions, session_id, prompt, images, observer, approvals)
        .map_err(|e| e.message)
}

/// [`run_session_turn_detailed`] with the typed end of a failed turn (#962),
/// for the one face that announces turn ends (`tole serve`).
pub fn run_session_turn_with_end(
    sessions: SharedSessions,
    session_id: &str,
    prompt: &str,
    images: &[tole_core::image::ImageRef],
    observer: Option<StdArc<dyn tole_core::turn::TurnObserver>>,
    approvals: Option<&crate::approvals::ApprovalQueue>,
) -> Result<TurnResult, TurnError> {
    // Brief map lock: claim the session for this turn. The MAP lock is
    // released here — a running turn holds only its OWN storage lock, so
    // the transport loop stays live (CodeCora deadlock finding).
    let (state, _busy_guard) = lock_sessions(&sessions)
        .claim_turn(session_id)
        .map_err(TurnError::before_turn)?;
    let queue_mark = approvals.map(|q| q.mark());
    let refresher = state.clone();
    let SessionState {
        storage,
        registry,
        memory,
        system_prompt,
        first_prompt_done,
        cancel,
        model,
        profile,
        ..
    } = state;
    let mut final_text: Option<String> = None;

    // Memory loop, pre-turn (first prompt of a fresh session only). An
    // image-only prompt (#559) has no text to recall on.
    let effective = if *first_prompt_done.lock().expect("fpd lock") || prompt.trim().is_empty() {
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
    // Model routing (#586, #945): once, at the first prompt of a NEW session.
    if !*first_prompt_done.lock().expect("fpd lock") && model.is_none() {
        route_first_prompt(&mut *storage, profile.as_deref(), prompt, &registry)
            .map_err(|e| TurnError::failed(e, TurnCode::Internal, &*storage))?;
    }
    // Live-token sinks (issue #196 phase 3): when an observer is wired
    // (ACP, serve with an open event stream), streamed reasoning/content
    // deltas flow to it DURING the provider call; the decisive output
    // semantics are unchanged. The same configuration applies to a reserve
    // provider (#209). The turn itself is the shared runner (#525).
    let plan = crate::turn_runner::TurnPlan {
        cfg: session_provider_config(model.as_deref())
            .map_err(|e| TurnError::failed(e, TurnCode::Provider, &*storage))?,
        registry: &registry,
        system_prompt: system_prompt.as_deref(),
        cancel: &cancel,
        observer: observer.as_ref(),
        resolve_parked: false,
        compaction: None,
    };
    let outcome =
        crate::turn_runner::run_turn_on_with_images(&mut *storage, &plan, &effective, images)
            .map_err(|e| run_error(e, &*storage))?;

    #[cfg(feature = "shell-tools")]
    if let tole_core::turn::TurnOutcome::Final { text, wrote } = &outcome {
        crate::turn_runner::remember(memory.as_ref(), session_id, prompt, text, *wrote);
    }
    *first_prompt_done.lock().expect("fpd lock") = true;
    // The turn is over: the footer's last-known values are these (#573).
    refresher.refresh_status(&*storage);

    if let tole_core::turn::TurnOutcome::Final { text, .. } = &outcome {
        final_text = Some(text.clone());
    }
    log_outcome(&outcome);
    let turn_end = TurnEnd::from_outcome(&outcome, last_seq(&*storage));
    let stop = stop_reason(&outcome).map_err(|message| TurnError {
        message,
        end: turn_end,
    })?;
    let approval_tool = match &outcome {
        tole_core::turn::TurnOutcome::ApprovalRequired { name } => Some(name.clone()),
        _ => None,
    };
    Ok(TurnResult {
        stop,
        text: final_text,
        approval_tool,
        queue_mark,
        turn_end,
    })
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
    resume_session_turn_with_end(sessions, session_id, None)
        .map(|r| (r.stop, r.text))
        .map_err(|e| e.message)
}

/// [`resume_session_turn`] with the typed end of the resumed turn (#962), and
/// the approval queue snapshot (`TurnResult::queue_mark`, taken after the busy
/// claim as in [`run_session_turn_detailed`]) so a second approval stop can name
/// the entry it queued. Only `tole serve` calls it.
pub fn resume_session_turn_with_end(
    sessions: SharedSessions,
    session_id: &str,
    approvals: Option<&crate::approvals::ApprovalQueue>,
) -> Result<TurnResult, TurnError> {
    let (state, _busy_guard) = lock_sessions(&sessions)
        .claim_turn(session_id)
        .map_err(TurnError::before_turn)?;
    let queue_mark = approvals.map(|q| q.mark());
    let refresher = state.clone();
    let SessionState {
        storage,
        registry,
        system_prompt,
        cancel,
        model,
        ..
    } = state;
    let mut storage = storage.lock().unwrap_or_else(|p| p.into_inner());
    // #318: the session token is wired into the replan loop, so
    // `/cancel` during an approval-resume settles the turn as cancelled.
    // The `reset()` above stays: like `run_session_turn`, it runs inside
    // the busy claim, so a cancel landing after the claim targets this
    // resume, while one that landed earlier had no in-flight turn to
    // target (cancel is unconditional) and must not kill it.
    let plan = crate::turn_runner::TurnPlan {
        cfg: session_provider_config(model.as_deref())
            .map_err(|e| TurnError::failed(e, TurnCode::Provider, &*storage))?,
        registry: &registry,
        system_prompt: system_prompt.as_deref(),
        cancel: &cancel,
        observer: None,
        resolve_parked: false,
        compaction: None,
    };
    let outcome = crate::turn_runner::resume_turn_on(&mut *storage, &plan)
        .map_err(|e| run_error(e, &*storage))?;
    refresher.refresh_status(&*storage);
    let final_text = match &outcome {
        tole_core::turn::TurnOutcome::Final { text, .. } => Some(text.clone()),
        _ => None,
    };
    log_outcome(&outcome);
    let turn_end = TurnEnd::from_outcome(&outcome, last_seq(&*storage));
    let stop = stop_reason(&outcome).map_err(|message| TurnError {
        message,
        end: turn_end,
    })?;
    let approval_tool = match &outcome {
        tole_core::turn::TurnOutcome::ApprovalRequired { name } => Some(name.clone()),
        _ => None,
    };
    Ok(TurnResult {
        stop,
        text: final_text,
        approval_tool,
        queue_mark,
        turn_end,
    })
}

/// A runner error as a failed turn: the code says which part failed, the text
/// stays in `message` only.
fn run_error(e: crate::turn_runner::TurnRunError, storage: &dyn Storage) -> TurnError {
    use crate::turn_runner::TurnRunError as E;
    let code = match &e {
        E::Provider(_) => TurnCode::Provider,
        E::Storage(_) => TurnCode::Storage,
    };
    TurnError::failed(e.into_message(), code, storage)
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

    struct TmpDir(crate::test_dir::TestDir);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            Self(crate::test_dir::TestDir::new(&format!("tole-evict-{tag}")))
        }
        fn path(&self) -> &std::path::Path {
            self.0.path()
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
            model: None,
            profile: None,
            status: StdArc::default(),
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

    fn open(tag: &str, plan_mode: bool) -> (SessionState, crate::test_dir::TestDir) {
        let ws = crate::test_dir::TestDir::new(&format!("tole-sh-roster-{tag}"));
        let state = open_session(
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
        .unwrap();
        (state, ws)
    }

    /// #420: server sessions build their tools through the shared roster —
    /// the same rules as the CLI (verify_package present; delete_file absent
    /// behind a non-interactive approver; plan mode leaves ReadOnly only).
    #[test]
    fn server_sessions_use_the_shared_roster() {
        let (s, _ws) = open("full", false);
        assert!(s.registry.get("verify_package").is_some());
        assert!(s.registry.get("write_file").is_some());
        assert!(s.registry.get("todo_write").is_some());
        assert!(s.registry.get("delete_file").is_none());

        let (p, _ws2) = open("plan", true);
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

    /// The prompt in the session file's header (the session holds the file lock).
    fn pinned(s: &SessionState) -> Option<String> {
        s.storage
            .lock()
            .unwrap()
            .system_prompt()
            .map(str::to_string)
    }

    fn without_context(p: &str) -> String {
        p.lines()
            .filter(|l| !l.starts_with("Context: working directory "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// #945: a server session with no explicit prompt pins the CLI's default
    /// prompt (modulo the context line, which names the SESSION workspace).
    #[test]
    fn server_session_pins_the_cli_default_prompt_with_its_own_workspace() {
        let (s, ws) = open("prompt", false);
        let p = s.system_prompt.clone().expect("a default prompt");
        assert_eq!(
            without_context(&p),
            without_context(&crate::session::assemble::build_default_prompt(false))
        );
        let canon = ws.canonicalize().unwrap();
        assert!(
            p.contains(&format!("Context: working directory {};", canon.display())),
            "{p}"
        );
        assert_ne!(
            Some(canon),
            std::env::current_dir().ok(),
            "the test needs a workspace that is not the process cwd"
        );
        // Pinned in the header, byte for byte.
        assert_eq!(pinned(&s), Some(p));
    }

    #[test]
    fn server_session_plan_mode_prompt_carries_the_plan_sentence() {
        let (s, _ws) = open("prompt-plan", true);
        let p = s.system_prompt.unwrap();
        assert!(p.contains("PLAN MODE"));
        assert_eq!(
            without_context(&p),
            without_context(&crate::session::assemble::build_default_prompt(true))
        );
    }

    #[test]
    fn an_explicit_prompt_wins_over_the_default() {
        let ws = crate::test_dir::TestDir::new("tole-sh-prompt-explicit");
        let s = open_session_with(
            "s-explicit",
            ws.to_str().unwrap(),
            false,
            false,
            AllowlistApprover::allow_only(vec![]),
            None,
            Some(&ws.join("sessions")),
            Vec::new(),
            Vec::new(),
            tole_core::cancel::CancelToken::default(),
            None,
            SessionOverrides {
                system_prompt: Some("You are a pirate.".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("You are a pirate."));
        assert_eq!(pinned(&s).as_deref(), Some("You are a pirate."));
    }

    /// Append-only (#945): a session created before the default prompt
    /// existed has none pinned, and resuming it must not add one.
    #[test]
    fn resume_keeps_the_stored_prompt_and_adds_none_to_an_old_session() {
        let ws = crate::test_dir::TestDir::new("tole-sh-prompt-resume");
        let dir = ws.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        // Old style: no pinned prompt in the header.
        tole_core::storage::JsonlStorage::create(&dir, "s-old", None).unwrap();
        // And one with a prompt pinned by an earlier version.
        tole_core::storage::JsonlStorage::create_named(
            &dir,
            "s-pinned",
            None,
            Some("old pinned"),
            None,
        )
        .unwrap();
        for (id, want) in [("s-old", None), ("s-pinned", Some("old pinned"))] {
            let s = open_session(
                id,
                ws.to_str().unwrap(),
                true,
                false,
                AllowlistApprover::allow_only(vec![]),
                None,
                Some(&dir),
                Vec::new(),
                Vec::new(),
                tole_core::cancel::CancelToken::default(),
                None,
            )
            .unwrap();
            assert_eq!(s.system_prompt.as_deref(), want, "{id}");
            assert_eq!(pinned(&s).as_deref(), want, "{id} header");
        }
    }
}

/// #945 capability 3: server sessions load approved skills, discovered in the
/// session workspace only.
#[cfg(test)]
mod server_skills_tests {
    use super::*;
    use crate::session::assemble::ServerSkills;
    use std::sync::Arc;
    use tole_core::approval::AllowlistApprover;
    use tole_core::skill_approval::{read_package, Approvals, Gate};

    fn write_skill(root: &std::path::Path, name: &str, body: &str) {
        let d = root.join("skills").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: \"d {name}\"\n---\n{body}"),
        )
        .unwrap();
    }

    fn approved(ws: &std::path::Path, names: &[&str]) -> Gate {
        let mut a = Approvals::default();
        for n in names {
            a.approve(&read_package(Some(ws), n).unwrap(), 1);
        }
        Gate::Approved(Arc::new(a))
    }

    fn cfg(gate: Gate) -> ServerSkills {
        ServerSkills {
            gate_override: Some(gate),
            ..Default::default()
        }
    }

    fn open_with(
        ws: &std::path::Path,
        id: &str,
        loading: bool,
        skills: &ServerSkills,
    ) -> SessionState {
        open_session_inner(
            id,
            ws.to_str().unwrap(),
            loading,
            false,
            AllowlistApprover::allow_only(vec![]),
            None,
            Some(&ws.join("sessions")),
            Vec::new(),
            Vec::new(),
            tole_core::cancel::CancelToken::default(),
            None,
            SessionOverrides::default(),
            skills,
            &[],
        )
        .unwrap()
    }

    fn pinned(s: &SessionState) -> Option<String> {
        s.storage
            .lock()
            .unwrap()
            .system_prompt()
            .map(str::to_string)
    }

    #[test]
    fn approved_skill_loads_into_the_pinned_prompt_and_registers_load_skill() {
        let ws = crate::test_dir::TestDir::new("tole-sk-approved");
        write_skill(&ws, "okskill", "do the ok thing");
        let s = open_with(&ws, "s1", false, &cfg(approved(&ws, &["okskill"])));
        let p = s.system_prompt.clone().unwrap();
        assert!(p.contains("# Skills available"), "{p}");
        assert!(p.contains("okskill"), "{p}");
        assert!(!p.contains("unavailable"), "{p}");
        assert_eq!(pinned(&s), Some(p));
        let t = s.registry.get("load_skill").expect("load_skill registered");
        assert_eq!(t.risk(), tole_core::tool::Risk::ReadOnly);
        let out = t
            .execute(serde_json::json!({"name": "okskill"}))
            .expect("approved skill loads");
        assert!(out.to_string().contains("do the ok thing"), "{out}");
    }

    #[test]
    fn unapproved_and_revoked_skills_are_never_loaded() {
        let ws = crate::test_dir::TestDir::new("tole-sk-unapproved");
        write_skill(&ws, "okskill", "ok body");
        write_skill(&ws, "newskill", "SECRET-NEW-BODY");
        write_skill(&ws, "oldskill", "SECRET-OLD-BODY");
        // Approved then revoked: the ledger no longer names it.
        let mut a = Approvals::default();
        a.approve(&read_package(Some(&ws), "okskill").unwrap(), 1);
        a.approve(&read_package(Some(&ws), "oldskill").unwrap(), 1);
        assert!(a.revoke("oldskill"));
        let s = open_with(&ws, "s2", false, &cfg(Gate::Approved(Arc::new(a))));
        let p = s.system_prompt.clone().unwrap();
        for name in ["newskill", "oldskill"] {
            assert!(
                p.contains(&format!("{name} {}", tole_core::skills::UNAPPROVED_MARK)),
                "{name} is listed as unavailable only: {p}"
            );
            let err = s
                .registry
                .get("load_skill")
                .unwrap()
                .execute(serde_json::json!({"name": name}))
                .unwrap_err();
            assert!(err.contains("approv"), "{err}");
        }
        assert!(!p.contains("SECRET"), "{p}");
    }

    #[test]
    fn a_changed_skill_needs_a_new_approval() {
        let ws = crate::test_dir::TestDir::new("tole-sk-changed");
        write_skill(&ws, "okskill", "v1");
        let gate = approved(&ws, &["okskill"]);
        write_skill(&ws, "okskill", "v2 SECRET-CHANGED");
        let s = open_with(&ws, "s3", false, &cfg(gate));
        let p = s.system_prompt.unwrap();
        assert!(!p.contains("SECRET-CHANGED"), "{p}");
        assert!(p.contains("not approved"), "{p}");
    }

    #[test]
    fn a_skill_outside_the_session_workspace_is_not_discovered() {
        let ws = crate::test_dir::TestDir::new("tole-sk-ws");
        let other = crate::test_dir::TestDir::new("tole-sk-other");
        write_skill(&other, "elsewhere", "ELSEWHERE-BODY");
        let gate = approved(&other, &["elsewhere"]);
        let s = open_with(&ws, "s4", false, &cfg(gate));
        let p = s.system_prompt.unwrap();
        assert!(!p.contains("elsewhere") && !p.contains("ELSEWHERE"), "{p}");
    }

    #[test]
    fn no_skills_loads_nothing_and_registers_nothing() {
        let ws = crate::test_dir::TestDir::new("tole-sk-off");
        write_skill(&ws, "okskill", "ok body");
        let mut c = cfg(approved(&ws, &["okskill"]));
        c.no_skills = true;
        let s = open_with(&ws, "s5", false, &c);
        let p = s.system_prompt.unwrap();
        assert!(!p.contains("okskill"), "{p}");
        assert!(s.registry.get("load_skill").is_none());
    }

    #[test]
    fn explicit_process_skills_append_as_on_the_cli() {
        let ws = crate::test_dir::TestDir::new("tole-sk-explicit");
        write_skill(&ws, "mine", "EXPLICIT-BODY");
        let mut c = cfg(Gate::Approved(Arc::new(Approvals::default())));
        c.explicit = vec![ws.join("skills").join("mine").join("SKILL.md")];
        let s = open_with(&ws, "s6", false, &c);
        assert!(
            s.system_prompt
                .unwrap()
                .contains("# Skill: mine\nEXPLICIT-BODY"),
            "an owner-named skill loads without an approval record"
        );
    }

    /// Append-only: an old session, or one pinned without skills, gains no
    /// skills section on resume; one pinned WITH the index gets `load_skill`
    /// back.
    #[test]
    fn resume_adds_no_skills_section_to_an_old_session() {
        let ws = crate::test_dir::TestDir::new("tole-sk-resume");
        write_skill(&ws, "okskill", "ok body");
        let dir = ws.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        tole_core::storage::JsonlStorage::create(&dir, "old", None).unwrap();
        tole_core::storage::JsonlStorage::create_named(
            &dir,
            "pinned",
            None,
            Some("old pinned"),
            None,
        )
        .unwrap();
        let c = cfg(approved(&ws, &["okskill"]));
        for (id, want) in [("old", None), ("pinned", Some("old pinned"))] {
            let s = open_with(&ws, id, true, &c);
            assert_eq!(s.system_prompt.as_deref(), want, "{id}");
            assert_eq!(pinned(&s).as_deref(), want, "{id} header");
            assert!(s.registry.get("load_skill").is_none(), "{id}");
        }
        // A session created with skills resumes with its prompt and the tool.
        let made = open_with(&ws, "withskills", false, &c);
        let prompt = made.system_prompt.clone().unwrap();
        drop(made);
        let again = open_with(&ws, "withskills", true, &c);
        assert_eq!(again.system_prompt, Some(prompt));
        assert!(again.registry.get("load_skill").is_some());
    }

    /// A request reaches a session only through `SessionOverrides` (and the
    /// arguments of `open_session`): none of them names a skill, a skills
    /// directory or an approval. Adding a field here must be a decision.
    #[test]
    fn session_overrides_carry_no_skill_input() {
        let SessionOverrides {
            system_prompt: _,
            model: _,
            profile: _,
        } = SessionOverrides::default();
        // The cwd a request names is the discovery root and the tool jail
        // at once, so skills come from where the file tools already may go.
        let ws = crate::test_dir::TestDir::new("tole-sk-body");
        write_skill(&ws, "okskill", "ok body");
        let sessions = ws.join("sessions");
        let s = open_session_inner(
            "s7",
            ws.to_str().unwrap(),
            false,
            false,
            AllowlistApprover::allow_only(vec![]),
            None,
            Some(&sessions),
            Vec::new(),
            Vec::new(),
            tole_core::cancel::CancelToken::default(),
            None,
            SessionOverrides {
                system_prompt: Some("# Skills available\n- okskill".into()),
                ..Default::default()
            },
            &cfg(Gate::Approved(Arc::new(Approvals::default()))),
            &[],
        )
        .unwrap();
        // The prompt text is the caller's; the registry still obeys the gate.
        let err = s
            .registry
            .get("load_skill")
            .unwrap()
            .execute(serde_json::json!({"name": "okskill"}))
            .unwrap_err();
        assert!(err.contains("approv"), "{err}");
    }
}
