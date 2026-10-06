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

use crate::tools::WriteFileTool;
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

/// Marks a session busy for its whole lifetime; Drop un-marks even on
/// panic, so one failed turn cannot brick the session.
pub struct BusyGuard(StdArc<Mutex<bool>>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        *self.0.lock().expect("busy lock") = false;
    }
}

#[derive(Default)]
pub struct Sessions {
    pub map: HashMap<String, SessionState>,
}

pub type SharedSessions = StdArc<Mutex<Sessions>>;

/// Callback publishing a plan payload to the session's client (issue
/// #196 phase 4). ACP wires a closure sending the standard `plan`
/// session/update (Termul's PlanPanel renders it, full-replace); the
/// serve/MCP faces pass None — no UI to render plans there.
pub type PlanEmitter = StdArc<dyn Fn(&serde_json::Value) + Send + Sync>;

/// The model-facing `update_plan` tool (issue #196 phase 4): publishes
/// the execution plan to the host's plan panel. Structurally
/// ReadOnly — it mutates nothing durable and has no side effects
/// beyond the notification. Validation is strict-but-lenient: required
/// `content`, enum-checked `priority`/`status` with spec defaults, so
/// the model self-corrects from the error text on malformed input.
pub struct UpdatePlanTool {
    emitter: PlanEmitter,
}

impl UpdatePlanTool {
    pub fn new(emitter: PlanEmitter) -> Self {
        Self { emitter }
    }
}

impl tole_core::tool::Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn risk(&self) -> tole_core::tool::Risk {
        tole_core::tool::Risk::ReadOnly
    }
    fn describe(&self, _input: &serde_json::Value) -> String {
        "publish the execution plan to the user's plan panel".into()
    }
    fn spec(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "entries": {
                    "type": "array",
                    "description": "The FULL plan, replacing any previous one",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "priority": {"enum": ["high", "medium", "low"]},
                            "status": {"enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["entries"]
        }))
    }
    fn execute(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
        let entries = normalize_plan(&input)?;
        (self.emitter)(&entries);
        Ok(serde_json::json!({
            "ok": true,
            "entries": entries.as_array().map(|a| a.len()).unwrap_or(0)
        }))
    }
}

/// Validate + normalize plan entries into the ACP `plan` update shape.
fn normalize_plan(input: &serde_json::Value) -> Result<serde_json::Value, String> {
    let arr = input["entries"]
        .as_array()
        .ok_or("update_plan: 'entries' (array) is required")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for e in arr {
        let content = e["content"]
            .as_str()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or("update_plan: each entry needs a non-empty 'content' string")?;
        let priority = e["priority"].as_str().unwrap_or("medium");
        if !matches!(priority, "high" | "medium" | "low") {
            return Err(format!(
                "update_plan: invalid priority '{priority}' (high | medium | low)"
            ));
        }
        let status = e["status"].as_str().unwrap_or("pending");
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return Err(format!(
                "update_plan: invalid status '{status}' (pending | in_progress | completed)"
            ));
        }
        out.push(serde_json::json!({
            "content": content, "priority": priority, "status": status
        }));
    }
    Ok(serde_json::Value::Array(out))
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
    let system_prompt = std::env::var("TOLE_SYSTEM_PROMPT")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let interactive = approver.interactive();
    let mut reg = tole_core::tool::ToolRegistry::with_approver(approver);
    use tole_core::cora_search::CoraSearchTool;
    use tole_core::file_tools::{DeleteFileTool, EditFileTool};
    use tole_core::gh::GhTool;
    use tole_core::git::GitTool;
    use tole_core::jobs::{JobPollTool, JobStartTool};
    use tole_core::read_file::ReadFileTool;
    use tole_core::run_command::RunCommandTool;
    use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};
    if tole_cli_binary_available("cora") {
        reg.register(Box::new(CoraSearchTool::new()))?;
    }
    if tole_cli_binary_available("uteke") {
        reg.register(Box::new(UtekeRecallTool::new()))?;
        if !plan_mode {
            reg.register(Box::new(UtekeDocumentTool::new(None)))?;
        }
    }
    // Write-tier tools: skipped entirely under --plan-mode (read-only
    // sessions — the model cannot even attempt a write). The earlier
    // draft registered everything and "filtered" afterwards, which
    // CodeCora rightly called out: the full registry under --yes broke
    // the read-only contract.
    let agent_depth: u32 = std::env::var("TOLE_AGENT_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if !plan_mode {
        let run_cmd = if agent_depth >= 1 {
            RunCommandTool::new(workspace_canon.clone()).in_child_agent_mode()
        } else {
            RunCommandTool::new(workspace_canon.clone())
        };
        reg.register(Box::new(run_cmd))?;
        let job_start = if agent_depth >= 1 {
            JobStartTool::new(workspace_canon.clone()).in_child_agent_mode()
        } else {
            JobStartTool::new(workspace_canon.clone())
        };
        reg.register(Box::new(job_start))?;
        reg.register(Box::new(WriteFileTool::new(workspace_canon.clone())))?;
        reg.register(Box::new(EditFileTool::new(workspace_canon.clone())))?;
        {
            let repo =
                detect_github_repo(&workspace_canon).unwrap_or_else(|| "codecoradev/tole".into());
            reg.register(Box::new(GhTool::new(repo)))?;
            register_gitea(&mut reg, &workspace_canon);
        }
        reg.register(Box::new(GitTool::new().in_dir(workspace_canon.clone())))?;
        // delete_file (Destructive) registers ONLY behind an interactive
        // approver — in serve mode (allowlist) it is skipped: Destructive
        // is never allowlistable, and a server has no human to ask.
        if interactive {
            reg.register(Box::new(DeleteFileTool::new(workspace_canon.clone())))?;
        }
    }
    reg.register(Box::new(JobPollTool::new(workspace_canon.clone())))?;
    reg.register(Box::new(ReadFileTool::new(workspace_canon.clone())))?;
    // Plan publishing (issue #196 phase 4): model-facing tool, present
    // only when the transport has a client that renders plans (ACP).
    if let Some(emitter) = plan_emitter {
        reg.register(Box::new(UpdatePlanTool::new(emitter)))?;
    }
    // systemone_decide (#172): probe-gated on SYSTEMONE_API_KEY.
    if let Some(t) = tole_core::systemone::SystemOneTool::from_env() {
        reg.register(Box::new(t))?;
    }
    // Child agents (#171): parent sessions only — a child (depth >= 1)
    // gets no agent tools at all (the structural depth cap).
    if agent_depth == 0 && !plan_mode {
        if let Ok(bin) = std::env::current_exe() {
            let _ = reg.register(Box::new(
                tole_core::agents::AgentStartTool::new(bin, workspace_canon.clone())
                    .with_parent_allows(parent_allows.clone()),
            ));
            let _ = reg.register(Box::new(tole_core::agents::AgentPollTool::new(
                workspace_canon.clone(),
            )));
        }
    }
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
        tole_core::storage::JsonlStorage::create_with(
            &dir,
            session_id,
            None,
            system_prompt.as_deref(),
        )
        .map_err(|e| format!("creating session: {e}"))?
    };
    // Task-list tools (issue #198): registered after the storage open —
    // the state is seeded from the replayed transcript (settled
    // todo_write outputs), so crash-resume restores the last list.
    // todo_read is ReadOnly (plan-mode safe); todo_write joins the other
    // Write tools in being absent under --plan-mode.
    let todo_state = tole_core::todo::TodoState::shared();
    todo_state.hydrate(storage.entries());
    reg.register(Box::new(tole_core::todo::TodoReadTool::new(StdArc::clone(
        &todo_state,
    ))))?;
    if !plan_mode {
        reg.register(Box::new(tole_core::todo::TodoWriteTool::new(
            StdArc::clone(&todo_state),
        )))?;
    }

    Ok(SessionState {
        storage: StdArc::new(Mutex::new(storage)),
        registry: StdArc::new(reg),
        system_prompt,
        memory,
        first_prompt_done: StdArc::new(Mutex::new(loading)),
        busy: StdArc::new(Mutex::new(false)),
        cancel,
        tool_ids: StdArc::new(Mutex::new(0)),
    })
}

pub fn sessions_dir_for(cwd: &str) -> Result<PathBuf, String> {
    let dir = PathBuf::from(cwd).join(".tole/sessions");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

pub fn tole_cli_binary_available(name: &str) -> bool {
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

pub fn detect_github_repo(cwd: &PathBuf) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd);
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    crate::session_host::github_repo_from_remote_url(&String::from_utf8_lossy(&out.stdout))
}

/// Register the `gitea` tool when BOTH probe legs hold: the checkout's
/// origin remote points at a Gitea instance (not GitHub — that's `gh`)
/// and a token env exists (`TOLE_GITEA_TOKEN` or `GITEA_TOKEN`).
/// Absent legs degrade to one warning line, never a phantom tool — the
/// same probe contract as uteke/cora.
pub fn register_gitea(reg: &mut tole_core::tool::ToolRegistry, cwd: &std::path::Path) {
    use tole_core::gitea::{gitea_from_remote, GiteaRemote, GiteaTool};
    let mut cmd = std::process::Command::new("git");
    cmd.args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd);
    let Ok(out) =
        tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let remote = gitea_from_remote(&String::from_utf8_lossy(&out.stdout));
    let (base, repo) = match remote {
        GiteaRemote::Ok { base, repo } => (base, repo),
        GiteaRemote::InsecureHttp { host } => {
            eprintln!(
                "tole: origin is a Gitea remote over plain http ({host}) — refusing to send the \
                 token unencrypted; use an https remote (loopback http is allowed)"
            );
            return;
        }
        GiteaRemote::NotGitea => return,
    };
    let token = std::env::var("TOLE_GITEA_TOKEN")
        .or_else(|_| std::env::var("GITEA_TOKEN"))
        .ok()
        .filter(|t| !t.trim().is_empty());
    let Some(token) = token else {
        eprintln!(
            "tole: origin is a Gitea remote ({repo}) but no TOLE_GITEA_TOKEN/GITEA_TOKEN is set — gitea tool disabled"
        );
        return;
    };
    if let Err(e) = reg.register(Box::new(GiteaTool::new(base, token, repo))) {
        eprintln!("tole: registering gitea: {e}");
    }
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
    // Brief map lock: take the session's handles and reject a busy
    // session. The MAP lock is released here — a running turn holds only
    // its OWN storage lock, so the transport loop stays live (CodeCora
    // deadlock finding).
    let (storage, registry, memory, system_prompt, first_prompt_done, busy_guard, cancel) = {
        let mut sessions = lock_sessions(&sessions);
        let Some(state) = sessions.map.get_mut(session_id) else {
            return Err(format!("unknown session: {session_id}"));
        };
        {
            let mut busy = state.busy.lock().expect("busy lock");
            if *busy {
                return Err("session is busy running a turn".into());
            }
            *busy = true;
            // Wipe a stale cancel flag in the SAME critical section as
            // the busy claim (#178): a cancel that lands after this
            // point targets THIS turn; one that landed before it had no
            // in-flight turn to target and must not kill the new one.
            state.cancel.reset();
        }
        (
            state.storage.clone(),
            state.registry.clone(),
            state.memory.clone(),
            state.system_prompt.clone(),
            state.first_prompt_done.clone(),
            std::sync::Arc::clone(&state.busy),
            state.cancel.clone(),
        )
    };
    // Panic-safe un-busy: Drop clears the flag even if the turn unwinds.
    let _busy_guard = BusyGuard(busy_guard);
    let mut final_text: Option<String> = None;

    // Memory loop, pre-turn (first prompt of a fresh session only).
    let mut effective = prompt.to_string();
    {
        let done = *first_prompt_done.lock().expect("fpd lock");
        if !done {
            if let Some(mem) = &memory {
                if let Ok(block) = tole_core::memory::recall_block(mem, prompt) {
                    if !block.is_empty() {
                        eprintln!("tole: memory: recalled context injected");
                        effective = format!("{prompt}{block}");
                    }
                }
            }
        }
    }

    // Lock the storage for the rest of the turn. The provider is built
    // fresh every turn (issue #176): a `fact/model` register override —
    // set durably by `tole acp`'s session/set_config_option — wins over
    // the env default. serve sessions have no setter today, so this is
    // a no-op there.
    let mut storage = storage.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut cfg) = tole_core::openai::OpenAiConfig::from_env() else {
        return Err(
            "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY              (or the OPENAI_* equivalents)"
                .into(),
        );
    };
    if let Some(model) = storage
        .get_register("fact", "model")
        .and_then(serde_json::Value::as_str)
        .filter(|m| !m.trim().is_empty())
    {
        cfg.model = model.to_string();
    }
    let mut provider =
        tole_core::openai::OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    if let Some(sys) = system_prompt.as_deref() {
        provider = provider.with_system_prompt(sys);
    }
    // Live-token sinks (issue #196 phase 3): when an observer is wired
    // (ACP), streamed reasoning/content deltas flow to it DURING the
    // provider call; the decisive output semantics are unchanged.
    if let Some(o) = observer.as_ref() {
        let ot = StdArc::clone(o);
        let or = StdArc::clone(o);
        provider = provider.with_delta_sinks(
            Some(StdArc::new(move |t: &str| ot.text_delta(t))),
            Some(StdArc::new(move |r: &str| or.reasoning(r))),
        );
    }

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
        if let Some(mem) = &memory {
            let _ = tole_core::memory::remember_session(mem, session_id, prompt, text, *wrote);
        }
    }
    *first_prompt_done.lock().expect("fpd lock") = true;

    let stop = match &outcome {
        tole_core::turn::TurnOutcome::Final { text, .. } => {
            final_text = Some(text.clone());
            "end_turn"
        }
        tole_core::turn::TurnOutcome::StopGateBlocked { reason } => {
            eprintln!("tole: stop gate blocked the turn: {reason}");
            "refusal"
        }
        tole_core::turn::TurnOutcome::ApprovalRequired { name } => {
            eprintln!("tole: approval denied for '{name}'");
            "refusal"
        }
        tole_core::turn::TurnOutcome::UnknownTool { name } => {
            eprintln!("tole: unknown tool '{name}'");
            "refusal"
        }
        tole_core::turn::TurnOutcome::ProviderFailed { message } => {
            eprintln!("tole: provider failed: {message}");
            "refusal"
        }
        tole_core::turn::TurnOutcome::BudgetExhausted => "max_tokens",
        tole_core::turn::TurnOutcome::Cancelled => "cancelled",
        tole_core::turn::TurnOutcome::LoopDetected { tool, .. } => {
            eprintln!("tole: loop detected on '{tool}'");
            "refusal"
        }
        tole_core::turn::TurnOutcome::Storage(e) => {
            return Err(format!("storage error: {e}"));
        }
    };
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
    let (storage, registry, system_prompt, busy_guard, cancel) = {
        let mut sessions = lock_sessions(&sessions);
        let Some(state) = sessions.map.get_mut(session_id) else {
            return Err(format!("unknown session: {session_id}"));
        };
        {
            let mut busy = state.busy.lock().expect("busy lock");
            if *busy {
                return Err("session is busy running a turn".into());
            }
            *busy = true;
            state.cancel.reset();
        }
        (
            state.storage.clone(),
            state.registry.clone(),
            state.system_prompt.clone(),
            StdArc::clone(&state.busy),
            state.cancel.clone(),
        )
    };
    let _busy_guard = BusyGuard(busy_guard);
    let mut storage = storage.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut cfg) = tole_core::openai::OpenAiConfig::from_env() else {
        return Err(
            "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY              (or the OPENAI_* equivalents)"
                .into(),
        );
    };
    if let Some(model) = storage
        .get_register("fact", "model")
        .and_then(serde_json::Value::as_str)
        .filter(|m| !m.trim().is_empty())
    {
        cfg.model = model.to_string();
    }
    let mut provider =
        tole_core::openai::OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    if let Some(sys) = system_prompt.as_deref() {
        provider = provider.with_system_prompt(sys);
    }
    let outcome = tole_core::turn::resume_turn(&mut *storage, &mut provider, &registry)
        .map_err(|e| e.to_string())?;
    let stop = match &outcome {
        tole_core::turn::TurnOutcome::Final { text, .. } => {
            Ok((("end_turn".to_string()), Some(text.clone())))
        }
        tole_core::turn::TurnOutcome::Cancelled => Ok(("cancelled".to_string(), None)),
        _ => Ok(("refusal".to_string(), None)),
    };
    let _ = cancel;
    stop
}

/// Best-effort `owner/name` from a git remote URL, for gh tool targeting.
pub fn github_repo_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let idx = url.to_ascii_lowercase().find("github.com")?;
    let rest = &url[idx + "github.com".len()..];
    let rest = rest.trim_start_matches(['/', ':']);
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    let valid = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if valid(owner) && valid(name) {
        Some(format!("{owner}/{name}"))
    } else {
        None
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn normalize_plan_validates_enums_and_defaults() {
        let plan = normalize_plan(&serde_json::json!({
            "entries": [
                {"content": "step one", "status": "in_progress"},
                {"content": "step two", "priority": "high"},
            ]
        }))
        .unwrap();
        assert_eq!(
            plan,
            serde_json::json!([
                {"content": "step one", "priority": "medium", "status": "in_progress"},
                {"content": "step two", "priority": "high", "status": "pending"},
            ])
        );
    }

    #[test]
    fn normalize_plan_rejects_malformed_input_loudly() {
        // Missing array.
        assert!(normalize_plan(&serde_json::json!({})).is_err());
        // Empty content.
        assert!(normalize_plan(&serde_json::json!({"entries": [{"content": " "}]})).is_err());
        // Out-of-enum values are refused, not silently coerced — the
        // model sees the error text and self-corrects.
        assert!(normalize_plan(
            &serde_json::json!({"entries": [{"content": "x", "priority": "urgent"}]})
        )
        .is_err());
        assert!(normalize_plan(
            &serde_json::json!({"entries": [{"content": "x", "status": "done"}]})
        )
        .is_err());
    }
}
