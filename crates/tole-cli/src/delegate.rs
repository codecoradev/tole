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
//! - **Machine guard:** past `max_live_processes`, or while the machine
//!   resource guard ([`crate::machine_guard`]) says wait, a spawn is recorded
//!   `queued` and started by a later delegate or poll, from the recorded
//!   node, strictly in log order. Children start at `child_nice`.
//! - **Location:** the log and its queue live in the tole data dir
//!   ([`log_path`]), outside every workspace; children get its absolute path.
//!
//! `agent_start` / `agent_poll` stay for one release as aliases of
//! `delegate` / `delegate_poll`.
//!
//! A child that ended is settled by the PARENT ([`settle_finished`]): its
//! measured usage (its whole carve when unreadable) and the completion report
//! it handed in with `delegate_report`, read from its plain agent dir with
//! the caps of `tole_core::delegation_report`. `done` needs a complete
//! report; otherwise `incomplete` with the reasons.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tole_core::agents::{AgentPollTool, AgentStartTool, ChildOpts};
use tole_core::delegation::{
    Budget, BudgetRequest, DelegateRequest, Delegation, Goal, Mode, Node, Outcome, Spend,
    DEFAULT_MAX_DEPTH, DEFAULT_MAX_LIVE_PROCESSES,
};
use tole_core::tool::{Risk, Tool};

use crate::config::Config;
use crate::ledger_file::{lease_alive, lease_of, FileStore};
use crate::machine_guard::{Gate, Guard, SharedProbe};
use tole_core::resource_guard::Thresholds;

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
    /// The tole data dir (`<codecora home>/tole`). The delegation log and
    /// its queue live there, OUTSIDE every workspace, so no child's file
    /// tools can reach them. `None`: no delegation.
    pub data_dir: Option<PathBuf>,
    /// The machine probe; `None`: this machine's (`LinuxProbe`).
    pub probe: Option<SharedProbe>,
    /// The guard thresholds; `None`: the `[delegation]` keys of `cfg`.
    pub thresholds: Option<Thresholds>,
}

impl Setup {
    /// The machine guard with its registry in `dir`.
    pub fn guard(&self, dir: PathBuf) -> Guard {
        let t = self.thresholds.clone().unwrap_or_else(|| {
            let d = self.cfg.delegation.clone().unwrap_or_default();
            // Validated when the config was parsed.
            d.resource_thresholds().unwrap_or_default()
        });
        Guard::new(t, self.probe.clone(), dir)
    }
}

/// The delegation log of the session rooted at `root`:
/// `<data dir>/delegation/<hash of the canonical root>/delegation.jsonl`.
pub fn log_path(data_dir: &Path, root: &Path) -> Option<PathBuf> {
    let canon = root.canonicalize().ok()?;
    // FNV-1a 64: a stable, short directory name per workspace.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canon.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(
        data_dir
            .join("delegation")
            .join(format!("{h:016x}"))
            .join(LOG_FILE),
    )
}

/// What a queued spawn keeps next to the log: only what carries no
/// authority. Allow list, budget, depth, mode, task, workspace and profile
/// are read back from the node the tree recorded, and the secrets are
/// recomputed, so a child that can write the queue file cannot widen a
/// queued sibling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Extras {
    node: String,
    role: Option<String>,
    consumes: Option<Value>,
    keep_mailbox: bool,
}

/// Everything needed to start one child, built from the recorded node.
#[derive(Debug, Clone)]
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
    guard: Guard,
    /// The guard's startup notes are shown once, on the first delegate.
    noted: std::sync::atomic::AtomicBool,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Largest total of session files the parent reads to measure a child.
pub const MAX_MEASURED_BYTES: u64 = 256 * 1024 * 1024;

/// A node's measured own usage: its agent dir's session usage records.
/// `None` when they cannot be read safely (the caller charges the carve):
/// a session entry that is not a regular file (a symlink, a device) or
/// session files larger than [`MAX_MEASURED_BYTES`] together; the child
/// can write its own agent dir, so sizes are checked before any read.
pub fn measured(agent_dir: &Path) -> Option<Spend> {
    let mut found = false;
    let mut s = Spend::default();
    let mut total: u64 = 0;
    let sessions = agent_dir.join("sessions");
    if !std::fs::symlink_metadata(&sessions).ok()?.is_dir() {
        return None;
    }
    for e in std::fs::read_dir(&sessions).ok()? {
        let p = e.ok()?.path();
        if p.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&p).ok()?;
        total = total.saturating_add(meta.len());
        if !meta.is_file() || total > MAX_MEASURED_BYTES {
            return None;
        }
        // Opened without blocking or following a link, and checked again
        // on the open file: the child may swap the entry after the stat.
        let bytes = tole_core::delegation_report::read_plain_file(&p, MAX_MEASURED_BYTES)
            .ok()
            .flatten()?;
        let usages = usage_records(&bytes);
        s.tokens = s
            .tokens
            .saturating_add(tole_core::transcript::tokens_in_out(&usages));
        let steps = tole_core::transcript::provider_steps(&usages);
        s.steps = s
            .steps
            .saturating_add(u32::try_from(steps).unwrap_or(u32::MAX));
        found = true;
    }
    found.then_some(s)
}

/// The usage rows of a session file's bytes (`{"kind":"usage",...}` lines);
/// any other or malformed line is skipped.
fn usage_records(bytes: &[u8]) -> Vec<tole_core::storage::UsageRecord> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("kind").and_then(Value::as_str) == Some("usage"))
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
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
        // ONE canonical root for every key: the log path, the agents dir,
        // the agent dirs recorded as sessions and the poll lookups. A
        // workspace that cannot be canonicalized gets no delegate tool.
        let root = root.canonicalize().ok()?;
        let (log, node, child) = match tole_core::child_env::delegation(env) {
            Some((log, node)) => {
                let d = Delegation::open(FileStore::new(log.clone())).ok()?;
                if !d.may_delegate(&node) {
                    return None;
                }
                (log, Some(node), true)
            }
            None if tole_core::child_env::depth(env) >= 1 => return None,
            None => (log_path(setup.data_dir.as_deref()?, &root)?, None, false),
        };
        // One registry per machine: `<data dir>/delegation`, the log's
        // grandparent for the root and every delegated child alike.
        let guard = setup.guard(log.parent()?.parent()?.to_path_buf());
        Some(Self {
            bin,
            root,
            worktrees,
            parent_allows,
            setup,
            log,
            node: Mutex::new(node),
            child,
            guard,
            noted: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The canonical workspace root every path of this host is keyed on.
    pub fn root(&self) -> &Path {
        &self.root
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
        // Unique per process too: two hosts in one process may share a log.
        static GOALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = GOALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("g-{:x}-{:x}-{n:x}", now(), std::process::id());
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

    /// Settles every running child whose process is gone as a finished run
    /// ([`settle_finished`]), then lets the tree cancel queued children of
    /// settled parents.
    fn recover(d: &mut Delegation<FileStore>) -> Result<(), String> {
        let gone: Vec<Node> = d
            .nodes()
            .filter(|n| n.status == tole_core::delegation::NodeStatus::Running)
            .filter(|n| n.lease.is_some_and(|l| !lease_alive(&l)))
            .cloned()
            .collect();
        for n in &gone {
            settle_finished(d, n)?;
        }
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
            if mode == Mode::Execute {
                if let Some((p, h)) = lease_blocked(d, &workspace, self.worktrees) {
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
            let h = d
                .delegate(&node, &req, own, now())
                .map_err(|e| e.to_string())?;
            let extras = Extras {
                node: h.node_id.clone(),
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
            // Strict FIFO: the new node joins the queue and the queue drains in
            // log order, so it never starts ahead of an older queued node.
            self.queue(&extras)?;
            let mut others = Vec::new();
            let mut mine = None;
            let (started, held) = self.drain(d, &goal)?;
            let queue = queue_view(d, &goal, held.as_deref());
            if !self.noted.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let paths = self.guard_paths();
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                let notes = tole_core::resource_guard::startup_notes(&*self.guard.probe.0, &refs);
                if !notes.is_empty() {
                    out["guard_notes"] = json!(notes);
                }
            }
            for (id, v) in started {
                if id == h.node_id {
                    mine = Some(v);
                } else {
                    others.push(json!({"node": id, "result": v}));
                }
            }
            out["started_from_queue"] = json!(others);
            match mine {
                Some(v) if v.get("error").is_some() => Err(format!(
                    "delegate: starting `{}` failed: {}",
                    h.node_id, v["error"]
                )),
                Some(v) => {
                    for (k, v) in v.as_object().into_iter().flatten() {
                        out[k] = v.clone();
                    }
                    Ok(out)
                }
                None => {
                    out["queued"] = json!(true);
                    out["note"] = json!(
                        "queued: it starts, in order, when the live-process limit, the machine \
                         guard and the workspace lease allow (on a later delegate or poll)"
                    );
                    if let Some(me) = queue.iter().find(|q| q["node"] == json!(h.node_id)) {
                        out["queued_reason"] = me["queued_reason"].clone();
                        out["waited_secs"] = me["waited_secs"].clone();
                    }
                    out["queue"] = json!(queue);
                    Ok(out)
                }
            }
        })
    }

    /// The launch spec of a recorded node: authority from the log, secrets
    /// recomputed (parent grant ∩ the profile's grants), extras from `x`.
    fn spec_for(&self, d: &Delegation<FileStore>, x: &Extras) -> Result<Spec, String> {
        let n = d.poll(&x.node).ok_or("delegate: node missing")?;
        let profile = n.profile.clone().filter(|p| p != "-");
        let keep_secrets = match &profile {
            None => Vec::new(),
            Some(name) => crate::profile::Profile::resolve(
                &self.setup.cfg,
                name,
                self.setup.base.as_deref(),
                self.setup.home.as_deref(),
            )?
            .granted(&held_secrets()),
        };
        Ok(Spec {
            node: n.id.clone(),
            bin: self.bin.clone(),
            root: PathBuf::from(
                n.workspace
                    .as_deref()
                    .ok_or("delegate: node has no workspace")?,
            ),
            worktrees: self.worktrees,
            task: n.task.clone(),
            mode: n.mode,
            profile,
            allow: n.allow.clone(),
            keep_secrets,
            depth: n.depth,
            budget: n.budget,
            role: x.role.clone(),
            consumes: x.consumes.clone(),
            keep_mailbox: x.keep_mailbox,
        })
    }

    fn queue_dir(&self) -> PathBuf {
        self.log.with_extension("queued")
    }

    fn queue(&self, extras: &Extras) -> Result<(), String> {
        let dir = self.queue_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("delegate: queue: {e}"))?;
        let body = serde_json::to_vec(extras).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(format!("{}.json", extras.node)), body)
            .map_err(|e| format!("delegate: queue: {e}"))
    }

    /// Disk paths the guard checks: the data dir, the workspace and, with
    /// worktrees, their directory.
    fn guard_paths(&self) -> Vec<PathBuf> {
        let mut p = vec![self.guard.dir.clone(), self.root.clone()];
        if self.worktrees {
            p.push(self.root.join(".tole-worktrees"));
        }
        crate::machine_guard::disk_paths(&p)
    }

    /// Starts queued children of `goal` in log order while the goal's
    /// live-process limit and the machine guard allow. It stops at the
    /// first one that must wait (FIFO: nothing jumps ahead of it) and
    /// returns why as the second value. Returns each started node with its
    /// launch output, or `{"error": ...}` when its launch failed (that node
    /// is settled `failed` and the drain goes on).
    fn drain(&self, d: &mut Delegation<FileStore>, goal: &str) -> Result<Drained, String> {
        let (_, max_live) = self.limits();
        let paths = self.guard_paths();
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let mut started = Vec::new();
        let held = loop {
            let live = d.live(goal);
            if live >= max_live.max(1) {
                break Some(format!(
                    "queued: {live} children of this goal are running (max_live_processes {})",
                    max_live.max(1)
                ));
            }
            let Some(next) = d.next_queued(goal).map(|n| n.id.clone()) else {
                break None;
            };
            let file = self.queue_dir().join(format!("{next}.json"));
            let extras = std::fs::read(&file)
                .ok()
                .and_then(|b| serde_json::from_slice::<Extras>(&b).ok())
                .filter(|x| x.node == next);
            let Some(extras) = extras else {
                let _ = std::fs::remove_file(&file);
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
                continue;
            };
            let spec = match self.spec_for(d, &extras) {
                Ok(spec) => spec,
                Err(e) => {
                    // e.g. its profile is gone from the config: it never
                    // started, so it spent nothing.
                    let _ = std::fs::remove_file(&file);
                    d.settle(
                        &next,
                        Outcome::Cancelled,
                        Spend::default(),
                        None,
                        Some(&e),
                        now(),
                    )
                    .map_err(|e| e.to_string())?;
                    continue;
                }
            };
            if spec.mode == Mode::Execute {
                let ws = spec.root.to_string_lossy().into_owned();
                if let Some((p, h)) = lease_blocked(d, &ws, spec.worktrees) {
                    // Waits for the writer ahead of it.
                    break Some(format!("queued: `{p}` is write-leased by `{h}`"));
                }
            }
            let nice = self.guard.thresholds.child_nice;
            let gate = self
                .guard
                .gate(&refs, now().max(0) as u64, &lease_alive, || {
                    let _ = std::fs::remove_file(&file);
                    match launch(d, &spec, &self.log, nice) {
                        Ok(out) => {
                            let pid = out["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
                            (out, pid.map(lease_of))
                        }
                        Err(e) => (json!({ "error": e }), None),
                    }
                })?;
            match gate {
                Gate::Started(out) => started.push((next, out)),
                Gate::Wait(reason) => break Some(reason),
            }
        };
        Ok((started, held))
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
            let Some(n) = d.by_session(&session).cloned() else {
                Self::recover(d)?;
                return Ok(None);
            };
            // The polled child is settled with its report and usage, then
            // every other child whose process is gone the same way.
            if !n.status.is_terminal() {
                settle_finished(d, &n)?;
            }
            Self::recover(d)?;
            let (started, held) = self.drain(d, &n.goal_id)?;
            let started: Vec<String> = started.into_iter().map(|(id, _)| id).collect();
            let queue = queue_view(d, &n.goal_id, held.as_deref());
            let n = d.poll(&n.id).ok_or("delegate: node vanished")?;
            Ok(Some(json!({
                "node": n.id, "status": n.status, "reason": n.reason,
                "used": n.used.map(|u| json!({"tokens": u.tokens, "steps": u.steps})),
                "report": n.report.as_ref().map(tole_core::delegation_report::fenced),
                "started_from_queue": started,
                "queue": queue,
            })))
        })?;
        if let Some(v) = view {
            out["delegation"] = v;
        }
        Ok(out)
    }
}

/// What a drain started (each node with its launch output) and, when it
/// stopped at a queued node, why.
type Drained = (Vec<(String, Value)>, Option<String>);

/// The goal's queued nodes, oldest first, with how long each has waited
/// and why: the head carries the reason the drain stopped (`held`), the
/// rest wait behind it.
fn queue_view(d: &Delegation<FileStore>, goal: &str, held: Option<&str>) -> Vec<Value> {
    let q: Vec<_> = d
        .queued()
        .into_iter()
        .filter(|n| n.goal_id == goal)
        .collect();
    let head = q.first().map(|n| n.id.clone()).unwrap_or_default();
    q.iter()
        .enumerate()
        .map(|(i, n)| {
            let reason = match (i, held) {
                (0, Some(r)) => r.to_string(),
                (0, None) => "queued: its parent has ended".to_string(),
                _ => format!("queued: behind `{head}` in the queue"),
            };
            json!({"node": n.id, "waited_secs": now().saturating_sub(n.at).max(0),
                   "queued_reason": reason})
        })
        .collect()
}

/// Is `dir` a plain directory at its canonical path (no symlink anywhere in
/// it)? The parent reads a child's report and usage only through such a
/// path, so a child cannot point them at another file.
fn plain_dir(dir: &Path) -> bool {
    dir.canonicalize().is_ok_and(|c| c == dir)
        && std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
}

/// Settles a child that has ended as a run: charged its measured session
/// usage (its whole carve when that cannot be read), with the report the
/// PARENT reads from the child's agent dir (capped, cleaned, never through
/// a symlink). The tree makes it `done` only with a complete report, else
/// `incomplete` with the reasons.
fn settle_finished(d: &mut Delegation<FileStore>, n: &Node) -> Result<(), String> {
    let dir = n
        .session
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| plain_dir(p));
    let (report, why) = match &dir {
        None => (
            None,
            Some("its agent dir is missing or not a plain directory".to_string()),
        ),
        Some(dir) => match tole_core::delegation_report::read(
            &dir.join(tole_core::delegation_report::REPORT_FILE),
        ) {
            Ok(r) => (r, None),
            Err(e) => (None, Some(format!("report refused: {e}"))),
        },
    };
    let used = dir
        .as_deref()
        .and_then(measured)
        .unwrap_or_else(|| Spend::all_of(n.budget));
    d.settle(&n.id, Outcome::Ok, used, report, why.as_deref(), now())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The live lease that blocks a new writer in `workspace` (canonical), as
/// `(path, holder)`. In place, the writer leases the whole workspace, so any
/// lease overlapping it blocks. With worktrees it gets a fresh directory
/// under `.tole-worktrees`, so only a lease on that dir or above it (someone
/// holding the whole workspace) blocks; sibling worktrees do not.
fn lease_blocked(
    d: &Delegation<FileStore>,
    workspace: &str,
    worktrees: bool,
) -> Option<(String, String)> {
    if worktrees {
        d.lease_ancestor_conflict(&format!("{workspace}/.tole-worktrees"), "")
    } else {
        d.lease_conflict(workspace, "")
    }
}

/// Spawns the child of `spec` (under the log lock), records its start and,
/// for an `execute` child, its write lease on the canonical directory it
/// works in.
fn launch(
    d: &mut Delegation<FileStore>,
    spec: &Spec,
    log: &Path,
    nice: u32,
) -> Result<Value, String> {
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
        nice: Some(nice),
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
