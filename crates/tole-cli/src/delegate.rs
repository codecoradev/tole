//! The delegate tool (#585 part a, design `profiles-collaboration-secrets.md`
//! A6, A8): the host side of `tole_core::delegation`.
//!
//! `delegate` carves a child out of this session's node and starts it as a
//! budgeted `tole mission` (its steps, minutes and tokens are the carve),
//! optionally as a profile. Every cap is checked by the delegation tree
//! before anything spawns:
//! - **Depth:** the tool is registered only where `may_delegate` holds, and
//!   a child carries its log and node id in its environment
//!   (`child_env::DELEGATION_*`), so it can delegate again only while above
//!   the goal's `max_depth`.
//! - **Authority:** the child's `--allow` is parent ∩ profile (or the
//!   requested patterns a parent pattern covers). A delegated child takes
//!   allow and trust from its flags only (`config_apply`), never from its
//!   profile, config or environment.
//! - **Secrets:** the child keeps its provider keys plus the parent grant ∩
//!   the profile's grants (`Profile::granted`); every other secret-named
//!   variable is scrubbed. The parent grant is the set of secret-named
//!   variables this process holds.
//! - **Writers:** an `execute` child takes a write lease on the
//!   CANONICALIZED directory it works in (the worktree, else the workspace),
//!   checked before it spawns.
//! - **Usage:** a settled child is charged what its session usage records
//!   say; when they cannot be read, its whole carve.
//! - **Machine guard:** past `max_live_processes` a spawn is recorded
//!   `queued` with its launch spec and started by a later delegate or poll.
//!
//! `agent_start` / `agent_poll` stay for one release as aliases of
//! `delegate` / `delegate_poll`. Part b of #585 adds the completion report;
//! until then an `ok` child settles `incomplete` ("no completion report").

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tole_core::agents::{AgentPollTool, AgentStartTool, ChildOpts};
use tole_core::delegation::{
    Budget, BudgetRequest, DelegateRequest, Delegation, Goal, Mode, Outcome, Spend,
    DEFAULT_MAX_DEPTH, DEFAULT_MAX_LIVE_PROCESSES,
};
use tole_core::storage::Storage as _;
use tole_core::tool::{Risk, Tool};

use crate::config::Config;
use crate::ledger_file::{lease_alive, lease_of, FileStore};

/// The delegation log, next to the agent dirs of the root session.
pub const LOG_FILE: &str = "delegation.jsonl";
/// Defaults of a goal's budget when `[delegation]` sets none.
pub const DEFAULT_GOAL_BUDGET: u64 = 1_000_000;
pub const DEFAULT_GOAL_STEPS: u32 = 400;
pub const DEFAULT_GOAL_MINUTES: u64 = 120;

/// What delegation needs from the host: the layered config (profiles and
/// `[delegation]`) and where profile paths resolve.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub cfg: Config,
    pub base: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

/// Everything needed to start one child, kept for a queued spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Spec {
    node: String,
    bin: PathBuf,
    root: PathBuf,
    worktrees: bool,
    task: String,
    mode: Mode,
    profile: Option<String>,
    allow: Vec<String>,
    keep_secrets: Vec<String>,
    depth: u32,
    budget: Budget,
    role: Option<String>,
    consumes: Option<Value>,
    keep_mailbox: bool,
}

/// One session's delegation host.
pub struct Delegator {
    bin: PathBuf,
    root: PathBuf,
    worktrees: bool,
    parent_allows: Vec<String>,
    setup: Setup,
    log: PathBuf,
    /// This session's node: set in a delegated child, begun lazily at root.
    node: Mutex<Option<String>>,
    child: bool,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A node's measured own usage: its agent dir's session usage records.
/// `None` when no session file can be read (the caller charges the carve).
pub fn measured(agent_dir: &Path) -> Option<Spend> {
    let mut found = false;
    let mut s = Spend::default();
    for e in std::fs::read_dir(agent_dir.join("sessions")).ok()? {
        let p = e.ok()?.path();
        if p.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let st = tole_core::storage::JsonlStorage::open_read_only(&p).ok()?;
        s.tokens = s
            .tokens
            .saturating_add(tole_core::transcript::tokens_in_out(st.usages()));
        let steps = tole_core::transcript::provider_steps(st.usages());
        s.steps = s
            .steps
            .saturating_add(u32::try_from(steps).unwrap_or(u32::MAX));
        found = true;
    }
    found.then_some(s)
}

/// Secret-named variables this process holds: its grant to pass down.
fn held_secrets() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| tole_core::secrets::is_secret_name(k))
        .collect()
}

fn canonical(p: &Path) -> Result<String, String> {
    let c = p
        .canonicalize()
        .map_err(|e| format!("delegate: canonicalizing {}: {e}", p.display()))?;
    c.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("delegate: {} is not UTF-8", c.display()))
}

impl Delegator {
    /// The host for this session, or `None` where it may not delegate: a
    /// classic `agent_start` child, or a delegated child at the goal's depth.
    pub fn for_session(
        bin: PathBuf,
        root: PathBuf,
        worktrees: bool,
        parent_allows: Vec<String>,
        setup: Setup,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let (log, node, child) = match tole_core::child_env::delegation(env) {
            Some((log, node)) => {
                let d = Delegation::open(FileStore::new(log.clone())).ok()?;
                if !d.may_delegate(&node) {
                    return None;
                }
                (log, Some(node), true)
            }
            None if tole_core::child_env::depth(env) >= 1 => return None,
            None => (
                tole_core::agents::agents_dir(&root).join(LOG_FILE),
                None,
                false,
            ),
        };
        Some(Self {
            bin,
            root,
            worktrees,
            parent_allows,
            setup,
            log,
            node: Mutex::new(node),
            child,
        })
    }

    /// Runs `f` on the log under its exclusive lock (milliseconds plus a
    /// spawn; never a child's run).
    fn locked<R>(
        &self,
        f: impl FnOnce(&mut Delegation<FileStore>) -> Result<R, String>,
    ) -> Result<R, String> {
        let dir = self.log.parent().ok_or("delegate: log has no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("delegate: {e}"))?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.log.with_extension("lock"))
            .map_err(|e| format!("delegate: lock: {e}"))?;
        lock.lock().map_err(|e| format!("delegate: lock: {e}"))?;
        let mut d =
            Delegation::open(FileStore::new(self.log.clone())).map_err(|e| e.to_string())?;
        let out = f(&mut d);
        drop(lock);
        out
    }

    fn limits(&self) -> (u32, u32) {
        let d = self.setup.cfg.delegation.clone().unwrap_or_default();
        (
            d.max_depth.unwrap_or(DEFAULT_MAX_DEPTH),
            d.max_live_processes.unwrap_or(DEFAULT_MAX_LIVE_PROCESSES),
        )
    }

    /// This session's node; at root, the goal is begun on first use.
    fn node_id(&self, d: &mut Delegation<FileStore>) -> Result<String, String> {
        let mut node = self.node.lock().map_err(|_| "delegate: poisoned")?;
        if let Some(n) = node.as_ref() {
            return Ok(n.clone());
        }
        let c = self.setup.cfg.delegation.clone().unwrap_or_default();
        let (max_depth, max_live) = self.limits();
        let id = format!("g-{:x}-{:x}", now(), std::process::id());
        let minutes = c.goal_minutes.unwrap_or(DEFAULT_GOAL_MINUTES);
        d.begin(
            &Goal {
                id: &id,
                task: "orchestrator",
                budget: Budget {
                    tokens: c.goal_budget.unwrap_or(DEFAULT_GOAL_BUDGET),
                    steps: c.goal_steps.unwrap_or(DEFAULT_GOAL_STEPS),
                    deadline: now().saturating_add(i64::try_from(minutes * 60).unwrap_or(i64::MAX)),
                },
                allow: &self.parent_allows,
                max_depth,
                max_live,
            },
            now(),
        )
        .map_err(|e| e.to_string())?;
        *node = Some(id.clone());
        Ok(id)
    }

    /// Settles children whose process is gone, charging measured usage or
    /// the whole carve.
    fn recover(d: &mut Delegation<FileStore>) -> Result<(), String> {
        d.recover(now(), lease_alive, |n| {
            n.session.as_deref().map(Path::new).and_then(measured)
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    fn delegate(&self, input: &Value) -> Result<Value, String> {
        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
        let task = s("task")
            .or_else(|| s("prompt"))
            .filter(|t| !t.trim().is_empty())
            .ok_or("delegate: missing or empty 'task'")?;
        let mode = match s("mode").as_deref() {
            None | Some("execute") => Mode::Execute,
            Some("consult") => Mode::Consult,
            Some(m) => return Err(format!("delegate: mode {m:?} is not consult or execute")),
        };
        let requested: Vec<String> = input
            .get("allow")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let profile = match s("profile") {
            None => None,
            Some(name) => {
                if !requested.is_empty() {
                    return Err(
                        "delegate: a profile brings its own allow list; drop 'allow'".into(),
                    );
                }
                Some(crate::profile::Profile::resolve(
                    &self.setup.cfg,
                    &name,
                    self.setup.base.as_deref(),
                    self.setup.home.as_deref(),
                )?)
            }
        };
        let profile_allow = match &profile {
            Some(p) => p.allow.clone(),
            None if requested.is_empty() => self.parent_allows.clone(),
            None => requested,
        };
        let b = input.get("budget");
        let num = |k: &str| b.and_then(|b| b.get(k)).and_then(Value::as_u64);
        let budget = BudgetRequest {
            tokens: num("tokens"),
            steps: num("steps").map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            minutes: num("minutes"),
        };
        let keep_secrets = profile
            .as_ref()
            .map(|p| p.granted(&held_secrets()))
            .unwrap_or_default();
        let workspace = canonical(&self.root)?;
        self.locked(|d| {
            Self::recover(d)?;
            let node = self.node_id(d)?;
            if !d.may_delegate(&node) {
                return Err(format!("delegate: `{node}` is at the goal's depth limit"));
            }
            let me = d.poll(&node).ok_or("delegate: own node missing")?;
            let own = match (self.child, me.session.as_deref()) {
                (true, Some(dir)) => measured(Path::new(dir)).unwrap_or(me.own).max(me.own),
                _ => me.own,
            };
            let goal = me.goal_id.clone();
            // A writer in this workspace must be free to lease it.
            let lease_at = if self.worktrees {
                format!("{workspace}/.tole-worktrees")
            } else {
                workspace.clone()
            };
            if mode == Mode::Execute {
                if let Some((p, h)) = d.lease_conflict(&lease_at, "") {
                    return Err(format!(
                        "delegate: `{p}` is write-leased by `{h}`; wait for it, delegate \
                         `consult` work, or run with --agents-worktree"
                    ));
                }
            }
            let req = DelegateRequest {
                profile: profile.as_ref().map_or("-", |p| p.name.as_str()),
                task: &task,
                mode,
                budget,
                profile_allow: &profile_allow,
                workspace: Some(&workspace),
            };
            let h = d.delegate(&node, &req, own, now()).map_err(|e| e.to_string())?;
            let spec = Spec {
                node: h.node_id.clone(),
                bin: self.bin.clone(),
                root: self.root.clone(),
                worktrees: self.worktrees,
                task: task.clone(),
                mode,
                profile: profile.as_ref().map(|p| p.name.clone()),
                allow: h.allow.clone(),
                keep_secrets: keep_secrets.clone(),
                depth: h.depth,
                budget: h.budget,
                role: s("role"),
                consumes: input.get("consumes").cloned(),
                keep_mailbox: input.get("keep_mailbox").and_then(Value::as_bool) == Some(true),
            };
            let mut out = json!({
                "node": h.node_id, "depth": h.depth, "mode": mode,
                "budget": {"tokens": h.budget.tokens, "steps": h.budget.steps,
                           "deadline": h.budget.deadline},
                "allow": h.allow, "dropped": h.dropped,
            });
            let (_, max_live) = self.limits();
            if d.live(&goal) >= max_live.max(1) {
                self.queue(&spec)?;
                out["queued"] = json!(true);
                out["note"] = json!("queued: the machine guard's live-process limit is reached; it starts when a delegated run ends");
                return Ok(out);
            }
            let started = launch(d, &spec, &self.log)?;
            for (k, v) in started.as_object().into_iter().flatten() {
                out[k] = v.clone();
            }
            Ok(out)
        })
    }

    fn queue_dir(&self) -> PathBuf {
        self.log.with_extension("queued")
    }

    fn queue(&self, spec: &Spec) -> Result<(), String> {
        let dir = self.queue_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("delegate: queue: {e}"))?;
        let body = serde_json::to_vec(spec).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(format!("{}.json", spec.node)), body)
            .map_err(|e| format!("delegate: queue: {e}"))
    }

    /// Starts queued children of `goal` while the machine guard allows.
    fn drain(&self, d: &mut Delegation<FileStore>, goal: &str) -> Result<Vec<String>, String> {
        let (_, max_live) = self.limits();
        let mut started = Vec::new();
        while d.live(goal) < max_live.max(1) {
            let Some(next) = d.next_queued(goal).map(|n| n.id.clone()) else {
                break;
            };
            let file = self.queue_dir().join(format!("{next}.json"));
            let spec = std::fs::read(&file)
                .ok()
                .and_then(|b| serde_json::from_slice::<Spec>(&b).ok());
            let _ = std::fs::remove_file(&file);
            match spec {
                Some(spec) if spec.node == next => {
                    launch(d, &spec, &self.log)?;
                    started.push(next);
                }
                _ => {
                    // Never started: it spent nothing.
                    d.settle(
                        &next,
                        Outcome::Cancelled,
                        Spend::default(),
                        None,
                        Some("launch spec lost"),
                        now(),
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(started)
    }

    fn poll(&self, inner: &AgentPollTool, input: Value) -> Result<Value, String> {
        let mut out = inner.execute(input)?;
        if out.get("running").and_then(Value::as_bool) != Some(false) {
            return Ok(out);
        }
        let Some(agent) = out.get("agent").and_then(Value::as_str).map(str::to_string) else {
            return Ok(out);
        };
        let Some(dir) = tole_core::agents::agent_dir(&self.root, &agent) else {
            return Ok(out);
        };
        let session = dir.to_string_lossy().into_owned();
        let view = self.locked(|d| {
            Self::recover(d)?;
            let Some(n) = d.by_session(&session).cloned() else {
                return Ok(None);
            };
            if !n.status.is_terminal() {
                let used = measured(&dir).unwrap_or_else(|| Spend::all_of(n.budget));
                d.settle(&n.id, Outcome::Ok, used, None, None, now())
                    .map_err(|e| e.to_string())?;
            }
            let started = self.drain(d, &n.goal_id)?;
            let n = d.poll(&n.id).ok_or("delegate: node vanished")?;
            Ok(Some(json!({
                "node": n.id, "status": n.status, "reason": n.reason,
                "used": n.used.map(|u| json!({"tokens": u.tokens, "steps": u.steps})),
                "started_from_queue": started,
            })))
        })?;
        if let Some(v) = view {
            out["delegation"] = v;
        }
        Ok(out)
    }
}

/// Spawns the child of `spec` (under the log lock), records its start and,
/// for an `execute` child, its write lease on the canonical directory it
/// works in.
fn launch(d: &mut Delegation<FileStore>, spec: &Spec, log: &Path) -> Result<Value, String> {
    let minutes = ((spec.budget.deadline - now()).max(60) + 59) / 60;
    let mut command: Vec<std::ffi::OsString> = vec![
        "mission".into(),
        "--max-steps".into(),
        spec.budget.steps.to_string().into(),
        "--max-minutes".into(),
        minutes.to_string().into(),
        "--max-tokens".into(),
        spec.budget.tokens.to_string().into(),
    ];
    if let Some(p) = &spec.profile {
        command.extend(["--profile".into(), p.into()]);
    }
    let mut start = AgentStartTool::new(spec.bin.clone(), spec.root.clone())
        .with_worktrees(spec.worktrees)
        .with_parent_allows(spec.allow.clone());
    start.max_concurrent = usize::MAX; // the delegation tree's guard rules
    start.opts = ChildOpts {
        command,
        depth: spec.depth,
        delegated: true,
        delegation: Some((log.to_path_buf(), spec.node.clone())),
        keep_secrets: spec.keep_secrets.clone(),
    };
    let mut input = json!({"prompt": spec.task, "keep_mailbox": spec.keep_mailbox});
    if let Some(r) = &spec.role {
        input["role"] = json!(r);
    }
    if let Some(c) = &spec.consumes {
        input["consumes"] = c.clone();
    }
    let out = match start.execute(input) {
        Ok(o) => o,
        Err(e) => {
            let _ = d.settle(
                &spec.node,
                Outcome::Failed,
                Spend::default(),
                None,
                Some(&e),
                now(),
            );
            return Err(e);
        }
    };
    let agent = out["agent"].as_str().unwrap_or_default().to_string();
    let pid = out["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .unwrap_or(0);
    let dir = tole_core::agents::agent_dir(&spec.root, &agent)
        .ok_or_else(|| format!("delegate: invalid agent id {agent:?}"))?;
    d.start(
        &spec.node,
        lease_of(pid),
        Some(&dir.to_string_lossy()),
        now(),
    )
    .map_err(|e| e.to_string())?;
    if spec.mode == Mode::Execute {
        let cwd = out["worktree"]
            .as_str()
            .map_or(spec.root.clone(), PathBuf::from);
        let leased = canonical(&cwd).and_then(|p| {
            d.lease_write(&spec.node, &p, now())
                .map_err(|e| e.to_string())
        });
        if let Err(e) = leased {
            kill(pid);
            let used = measured(&dir).unwrap_or_else(|| Spend::all_of(spec.budget));
            let _ = d.settle(&spec.node, Outcome::Cancelled, used, None, Some(&e), now());
            return Err(e);
        }
    }
    Ok(out)
}

fn kill(pid: u32) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain signal to the child's own process group.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// `delegate` (or its alias `agent_start`).
pub struct DelegateTool {
    pub name: &'static str,
    pub host: std::sync::Arc<Delegator>,
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        self.name
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn summary(&self) -> String {
        let alias = if self.name == "agent_start" {
            " Deprecated alias of `delegate`."
        } else {
            ""
        };
        format!(
            "Delegate one task to a child session (optionally as a profile), with a budget carved \
             from yours; collect its result with delegate_poll.{alias}"
        )
    }
    fn describe(&self, input: &Value) -> String {
        format!(
            "delegate {:?} as {} ({}), allow {:?}",
            input
                .get("task")
                .or_else(|| input.get("prompt"))
                .and_then(Value::as_str)
                .unwrap_or("<missing task>")
                .chars()
                .take(160)
                .collect::<String>(),
            input
                .get("profile")
                .and_then(Value::as_str)
                .unwrap_or("no profile"),
            input
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("execute"),
            input.get("allow").cloned().unwrap_or(Value::Null),
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The child's task (alias: prompt)"},
                "prompt": {"type": "string", "description": "Alias of task"},
                "profile": {"type": "string", "description": "Run the child as this configured profile"},
                "mode": {"type": "string", "enum": ["execute", "consult"], "description": "consult never writes (default execute)"},
                "budget": {"type": "object", "description": "Carved from yours; omitted parts take half of what is left",
                    "properties": {"tokens": {"type": "integer"}, "steps": {"type": "integer"}, "minutes": {"type": "integer"}}},
                "allow": {"type": "array", "items": {"type": "string"}, "description": "Without a profile: allow patterns, each covered by one of yours"},
                "role": {"type": "string", "enum": ["explore", "implement", "verify", "fix"]},
                "consumes": {"type": "array", "items": {"type": "object"}},
                "keep_mailbox": {"type": "boolean"}
            }
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        self.host.delegate(&input)
    }
}

/// `delegate_poll` (or its alias `agent_poll`).
pub struct DelegatePollTool {
    pub name: &'static str,
    pub host: std::sync::Arc<Delegator>,
    pub inner: AgentPollTool,
}

impl Tool for DelegatePollTool {
    fn name(&self) -> &str {
        self.name
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn is_poll(&self) -> bool {
        true
    }
    fn summary(&self) -> String {
        "Poll a delegated child for its status and result; a settled child is charged its \
         measured usage."
            .into()
    }
    fn spec(&self) -> Option<Value> {
        self.inner.spec()
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        self.host.poll(&self.inner, input)
    }
}

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
