//! Delegation tree (issue #585, design `profiles-collaboration-secrets.md`
//! A6, A8 and A10): one goal, an orchestrator at its root, profiles below.
//!
//! The caps live here, in one place, so no caller can skip one:
//!
//! - **Budget carve, no count cap.** A child's tokens and steps come out of
//!   its parent's REMAINING budget, and its deadline is never later than the
//!   parent's. While a child runs, its whole carve is reserved; once it
//!   settles, the parent is charged what its subtree spent. N children share
//!   one pot, so delegating can never multiply the root budget. Unknown
//!   usage is charged in full: a crashed child is charged its whole carve
//!   unless the host measured less, a budget stop is charged its whole
//!   carve, and a node's reported own spend can only grow.
//! - **Depth.** The root is depth 0; a node may delegate only while
//!   `depth < max_depth` (default 2, hard ceiling 3). The host registers the
//!   delegate tool only where [`Delegation::may_delegate`] says so.
//! - **Authority narrows.** The child allowlist is the profile's patterns
//!   that a parent pattern covers ([`crate::approval::list_covers`]), plus
//!   every deny of both; a `consult` node can only spawn `consult` nodes.
//! - **One writer per path.** An `execute` node takes a write lease on a
//!   path before writing; a lease conflicts with any live lease on the same
//!   path, an ancestor or a descendant of it. Leases end when the node
//!   settles. A lease path must be absolute and lexically normal (`/a/b`);
//!   any other spelling is refused, never rewritten. Nothing here touches the
//!   filesystem, so the host canonicalizes paths (symlinks, `..`, case
//!   folding) before calling.
//! - **Machine guard.** `max_live_processes` only queues: a spawn over it is
//!   recorded `queued` and starts when a slot frees ([`Delegation::start`]).
//! - **Mandatory report.** An `ok` run settles `done` only with a complete
//!   [`Report`] and no unsettled children; otherwise `incomplete`, with the
//!   reasons recorded. A terminal record is never overwritten.
//!
//! Everything is an append-only JSONL log over a [`LedgerStore`], folded like
//! the cron ledger: nothing here reads a clock, spawns or locks (`now` and
//! process liveness are passed in). The host holds an exclusive lock around
//! every mutating call, as for the cron ledger. The log is the audit trail:
//! [`Delegation::render_tree`] prints who spawned whom, with what budget,
//! allowlist, workspace, session and report.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::{list_covers, DENY_PREFIX};
use crate::ledger::{Lease, LedgerError, LedgerStore};

/// Version of the delegation record format.
pub const DELEGATION_VERSION: u32 = 1;
/// Default depth below the orchestrator.
pub const DEFAULT_MAX_DEPTH: u32 = 2;
/// Hard ceiling for `delegation.max_depth`.
pub const MAX_DEPTH_CEILING: u32 = 3;
/// Default machine guard: live delegated processes before spawns queue.
pub const DEFAULT_MAX_LIVE_PROCESSES: u32 = 8;
/// Longest goal id.
const MAX_ID_LEN: usize = 64;

/// What a node may spend: tokens and steps are a pot, the deadline a cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub tokens: u64,
    pub steps: u32,
    /// Unix seconds; the node must settle by then.
    pub deadline: i64,
}

/// What a node spent itself (its children report their own).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spend {
    pub tokens: u64,
    pub steps: u32,
}

impl Spend {
    /// Componentwise maximum.
    pub fn max(self, o: Spend) -> Spend {
        Spend {
            tokens: self.tokens.max(o.tokens),
            steps: self.steps.max(o.steps),
        }
    }

    /// The whole of `b`'s tokens and steps.
    pub fn all_of(b: Budget) -> Spend {
        Spend {
            tokens: b.tokens,
            steps: b.steps,
        }
    }
}

/// A child's requested budget; `None` takes half of what is left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetRequest {
    pub tokens: Option<u64>,
    pub steps: Option<u32>,
    pub minutes: Option<u64>,
}

/// `consult` never writes; `execute` may take write leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Consult,
    Execute,
}

/// A node's state. Every status but `queued` and `running` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Queued,
    Running,
    Done,
    Incomplete,
    Failed,
    Cancelled,
    Budget,
    Interrupted,
}

impl NodeStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, NodeStatus::Queued | NodeStatus::Running)
    }
}

/// How the host says a run ended; `Ok` still needs a report to be `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
    Cancelled,
    Budget,
}

/// One check the child ran, e.g. `cargo test` → `passed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub command: String,
    pub result: String,
}

/// The completion report a delegated run must hand in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// What was done or found.
    pub summary: String,
    /// Files changed (empty for a run that changed none).
    #[serde(default)]
    pub files: Vec<String>,
    /// Where the change can be read: a commit, branch or worktree.
    #[serde(default)]
    pub diff_ref: Option<String>,
    #[serde(default)]
    pub checks: Vec<Check>,
    /// Open risks; empty means the child saw none.
    #[serde(default)]
    pub risks: Vec<String>,
}

impl Report {
    /// Why this report does not complete a run in `mode` (empty = complete).
    pub fn problems(&self, mode: Mode) -> Vec<String> {
        let mut out = Vec::new();
        if self.summary.trim().is_empty() {
            out.push("the report has no summary".to_string());
        }
        if !self.files.is_empty() {
            if mode == Mode::Consult {
                out.push("a consult run reported changed files".to_string());
            }
            if self.diff_ref.as_deref().is_none_or(|d| d.trim().is_empty()) {
                out.push("files changed but the report names no diff ref".to_string());
            }
            if self.checks.is_empty() {
                out.push("files changed but the report lists no checks run".to_string());
            }
        }
        out
    }
}

/// Errors from the delegation tree.
#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unknown node `{0}`")]
    UnknownNode(String),
    #[error("goal `{0}` already exists")]
    GoalExists(String),
    #[error("node `{node}` is at depth {depth}; delegation stops at depth {max_depth}")]
    DepthCap {
        node: String,
        depth: u32,
        max_depth: u32,
    },
    #[error("node `{node}` has no budget left to carve ({what})")]
    BudgetExhausted { node: String, what: &'static str },
    #[error("node `{0}` is settled")]
    Settled(String),
    #[error("node `{node}` is {status:?}, not {want}")]
    WrongState {
        node: String,
        status: NodeStatus,
        want: &'static str,
    },
    #[error("queued: {live} of {max} delegated processes are live; it starts when one ends")]
    Queued { live: u32, max: u32 },
    #[error("`{path}` is write-leased by `{holder}`")]
    LeaseBusy { path: String, holder: String },
}

type Result<T> = std::result::Result<T, DelegationError>;

/// A goal: the root budget and allowlist, and the limits of its tree.
#[derive(Debug, Clone, Copy)]
pub struct Goal<'a> {
    pub id: &'a str,
    pub task: &'a str,
    pub budget: Budget,
    pub allow: &'a [String],
    /// 1 to [`MAX_DEPTH_CEILING`]; [`DEFAULT_MAX_DEPTH`] unless configured.
    pub max_depth: u32,
    /// The machine guard; [`DEFAULT_MAX_LIVE_PROCESSES`] unless configured.
    pub max_live: u32,
}

/// A delegation request from `parent`.
#[derive(Debug, Clone)]
pub struct DelegateRequest<'a> {
    pub profile: &'a str,
    pub task: &'a str,
    pub mode: Mode,
    pub budget: BudgetRequest,
    /// The profile's own allow patterns (intersected with the parent's).
    pub profile_allow: &'a [String],
    /// The workspace the child works in (a worktree for a parallel writer).
    pub workspace: Option<&'a str>,
}

/// What a granted delegation got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub node_id: String,
    pub depth: u32,
    pub budget: Budget,
    pub allow: Vec<String>,
    /// Profile patterns no parent pattern covers (dropped, recorded).
    pub dropped: Vec<String>,
}

/// One node of the tree, as folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub goal_id: String,
    pub parent: Option<String>,
    pub profile: Option<String>,
    pub task: String,
    pub mode: Mode,
    pub depth: u32,
    pub budget: Budget,
    pub allow: Vec<String>,
    pub dropped: Vec<String>,
    pub workspace: Option<String>,
    pub status: NodeStatus,
    pub lease: Option<Lease>,
    pub session: Option<String>,
    pub used: Option<Spend>,
    pub report: Option<Report>,
    pub reason: Option<String>,
    pub children: Vec<String>,
    /// The highest own spend this node reported when delegating; a later,
    /// lower report is refused.
    pub own: Spend,
    /// Position in the log among nodes: queued nodes start in this order.
    pub seq: u64,
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Rec {
    Goal {
        goal_id: String,
        task: String,
        budget: Budget,
        allow: Vec<String>,
        max_depth: u32,
        max_live: u32,
        at: i64,
    },
    Spawn {
        node_id: String,
        parent_id: String,
        profile: String,
        task: String,
        mode: Mode,
        budget: Budget,
        allow: Vec<String>,
        dropped: Vec<String>,
        workspace: Option<String>,
        /// The parent's own spend as reported for this spawn.
        #[serde(default)]
        parent_own: Spend,
        at: i64,
    },
    Start {
        node_id: String,
        lease: Lease,
        session: Option<String>,
        at: i64,
    },
    WriteLease {
        node_id: String,
        path: String,
        at: i64,
    },
    Release {
        node_id: String,
        path: String,
        at: i64,
    },
    Settle {
        node_id: String,
        status: NodeStatus,
        used: Spend,
        report: Option<Report>,
        reason: Option<String>,
        at: i64,
    },
}

const KINDS: &[&str] = &["goal", "spawn", "start", "write_lease", "release", "settle"];

impl Rec {
    fn to_line(&self) -> String {
        let mut v = serde_json::to_value(self).expect("delegation records always serialize");
        if let Value::Object(m) = &mut v {
            m.insert("v".into(), DELEGATION_VERSION.into());
        }
        v.to_string()
    }

    fn parse(line: &str) -> std::result::Result<Option<Rec>, LedgerError> {
        let bad = |e: serde_json::Error| LedgerError::Corrupt {
            offset: 0,
            reason: e.to_string(),
        };
        let v: Value = serde_json::from_str(line).map_err(bad)?;
        let kind = v.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !KINDS.contains(&kind) {
            return Ok(None);
        }
        let found = v.get("v").and_then(Value::as_u64).unwrap_or(0);
        if found > u64::from(DELEGATION_VERSION) {
            return Err(LedgerError::NewerVersion {
                kind: kind.to_string(),
                found,
            });
        }
        serde_json::from_value(v).map(Some).map_err(bad)
    }
}

#[derive(Debug, Clone, Copy)]
struct GoalLimits {
    max_depth: u32,
    max_live: u32,
}

/// The folded log.
#[derive(Debug, Default)]
struct State {
    nodes: BTreeMap<String, Node>,
    goals: BTreeMap<String, GoalLimits>,
    /// path → holder node id.
    leases: BTreeMap<String, String>,
}

/// Is `path` absolute and lexically normal: `/` or `/a/b`, with no empty,
/// `.` or `..` component (so no `//` and no trailing slash)? Lease paths
/// must be; anything else is refused, never rewritten. This module does no
/// filesystem access: the host canonicalizes (symlinks, case folding) first.
fn is_normal_path(path: &str) -> bool {
    path == "/"
        || path.strip_prefix('/').is_some_and(|rest| {
            rest.split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..")
        })
}

/// Does `a` equal `b`, or contain it, or sit inside it (by path component)?
/// Both are [`is_normal_path`].
fn paths_overlap(a: &str, b: &str) -> bool {
    let inside = |x: &str, y: &str| {
        x == y || x.strip_prefix(y).is_some_and(|r| r.starts_with('/')) || y == "/"
    };
    inside(a, b) || inside(b, a)
}

impl State {
    fn lease_holder(&self, path: &str, except: &str) -> Option<(&str, &str)> {
        self.leases
            .iter()
            .find(|(p, n)| n.as_str() != except && paths_overlap(p, path))
            .map(|(p, n)| (p.as_str(), n.as_str()))
    }

    fn apply(&mut self, rec: Rec) {
        match rec {
            Rec::Goal {
                goal_id,
                task,
                budget,
                allow,
                max_depth,
                max_live,
                at,
            } => {
                if self.nodes.contains_key(&goal_id) {
                    return;
                }
                self.goals.insert(
                    goal_id.clone(),
                    GoalLimits {
                        max_depth,
                        max_live,
                    },
                );
                self.nodes.insert(
                    goal_id.clone(),
                    Node {
                        id: goal_id.clone(),
                        goal_id,
                        parent: None,
                        profile: None,
                        task,
                        mode: Mode::Execute,
                        depth: 0,
                        budget,
                        allow,
                        dropped: Vec::new(),
                        workspace: None,
                        status: NodeStatus::Running,
                        lease: None,
                        session: None,
                        used: None,
                        report: None,
                        reason: None,
                        children: Vec::new(),
                        own: Spend::default(),
                        seq: self.nodes.len() as u64,
                        at,
                    },
                );
            }
            Rec::Spawn {
                node_id,
                parent_id,
                profile,
                task,
                mode,
                budget,
                allow,
                dropped,
                workspace,
                parent_own,
                at,
            } => {
                if self.nodes.contains_key(&node_id) {
                    return;
                }
                // The log is the authority the host launches from, so the
                // fold re-checks every cap a spawn must respect; a record
                // that breaks one (forged, or from a buggy writer) is
                // ignored, never launched.
                if !self.spawn_allowed(&node_id, &parent_id, mode, &budget, &allow, parent_own) {
                    return;
                }
                let Some(parent) = self.nodes.get_mut(&parent_id) else {
                    return;
                };
                parent.own = parent.own.max(parent_own);
                parent.children.push(node_id.clone());
                let node = Node {
                    id: node_id.clone(),
                    goal_id: parent.goal_id.clone(),
                    parent: Some(parent_id),
                    profile: Some(profile),
                    task,
                    mode,
                    depth: parent.depth + 1,
                    budget,
                    allow,
                    dropped,
                    workspace,
                    status: NodeStatus::Queued,
                    lease: None,
                    session: None,
                    used: None,
                    report: None,
                    reason: None,
                    children: Vec::new(),
                    own: Spend::default(),
                    seq: 0,
                    at,
                };
                let node = Node {
                    seq: self.nodes.len() as u64,
                    ..node
                };
                self.nodes.insert(node_id, node);
            }
            Rec::Start {
                node_id,
                lease,
                session,
                ..
            } => {
                if let Some(n) = self.nodes.get_mut(&node_id) {
                    if n.status == NodeStatus::Queued {
                        n.status = NodeStatus::Running;
                        n.lease = Some(lease);
                        n.session = session;
                    }
                }
            }
            Rec::WriteLease { node_id, path, .. } => {
                if !is_normal_path(&path) {
                    return; // never written by the API; a forged record
                }
                let live = self
                    .nodes
                    .get(&node_id)
                    .is_some_and(|n| n.status == NodeStatus::Running && n.mode == Mode::Execute);
                // First lease in log order wins; a racing loser is ignored.
                if live && self.lease_holder(&path, &node_id).is_none() {
                    self.leases.insert(path, node_id);
                }
            }
            Rec::Release { node_id, path, .. } => {
                if self.leases.get(&path) == Some(&node_id) {
                    self.leases.remove(&path);
                }
            }
            Rec::Settle {
                node_id,
                status,
                used,
                report,
                reason,
                ..
            } => {
                let Some(n) = self.nodes.get_mut(&node_id) else {
                    return;
                };
                if n.status.is_terminal() || !status.is_terminal() {
                    return; // the first terminal record is final
                }
                // A settle never charges below what the node reported, and
                // a budget stop always spent its whole carve.
                let mut used = used.max(n.own);
                if status == NodeStatus::Budget {
                    used = used.max(Spend::all_of(n.budget));
                }
                n.status = status;
                n.used = Some(used);
                n.report = report;
                n.reason = reason;
                self.leases.retain(|_, holder| *holder != node_id);
            }
        }
    }

    /// Does a spawn record respect every cap at this point of the log? The
    /// parent is live, the id is the next one under it, the depth is within
    /// the goal's, a consult parent spawns consult work only, each allow
    /// pattern is covered by the parent's and every parent deny is kept, and
    /// the budget fits what the parent can still carve, deadline included.
    fn spawn_allowed(
        &self,
        node_id: &str,
        parent_id: &str,
        mode: Mode,
        budget: &Budget,
        allow: &[String],
        parent_own: Spend,
    ) -> bool {
        let Some(p) = self.nodes.get(parent_id) else {
            return false;
        };
        let max_depth = self
            .goals
            .get(&p.goal_id)
            .map_or(DEFAULT_MAX_DEPTH, |g| g.max_depth);
        let is_deny = |a: &String| a.starts_with(DENY_PREFIX);
        let left = self.available(p, p.own.max(parent_own));
        !p.status.is_terminal()
            && node_id == format!("{}.{}", p.id, p.children.len() + 1)
            && p.depth < max_depth
            && !(p.mode == Mode::Consult && mode == Mode::Execute)
            && allow.iter().all(|a| is_deny(a) || list_covers(&p.allow, a))
            && p.allow
                .iter()
                .filter(|d| is_deny(d))
                .all(|d| allow.contains(d))
            && budget.tokens >= 1
            && budget.steps >= 1
            && budget.tokens <= left.tokens
            && budget.steps <= left.steps
            && budget.deadline <= p.budget.deadline
    }

    /// What `id`'s subtree has used or reserved, as charged to its parent:
    /// a live node reserves its whole carve; a settled one is charged its
    /// own spend plus its children's charges, never more than its carve.
    fn charge(&self, id: &str) -> Spend {
        let Some(n) = self.nodes.get(id) else {
            return Spend::default();
        };
        if !n.status.is_terminal() {
            return Spend {
                tokens: n.budget.tokens,
                steps: n.budget.steps,
            };
        }
        let mut s = n.used.unwrap_or_default();
        for c in &n.children {
            let cc = self.charge(c);
            s.tokens = s.tokens.saturating_add(cc.tokens);
            s.steps = s.steps.saturating_add(cc.steps);
        }
        Spend {
            tokens: s.tokens.min(n.budget.tokens),
            steps: s.steps.min(n.budget.steps),
        }
    }

    /// What `parent` can still carve, given what it spent itself.
    fn available(&self, parent: &Node, own: Spend) -> Spend {
        let mut left = Spend {
            tokens: parent.budget.tokens.saturating_sub(own.tokens),
            steps: parent.budget.steps.saturating_sub(own.steps),
        };
        for c in &parent.children {
            let cc = self.charge(c);
            left.tokens = left.tokens.saturating_sub(cc.tokens);
            left.steps = left.steps.saturating_sub(cc.steps);
        }
        left
    }

    fn live_in(&self, goal: &str) -> u32 {
        self.nodes
            .values()
            .filter(|n| n.goal_id == goal && n.parent.is_some())
            .filter(|n| n.status == NodeStatus::Running)
            .count() as u32
    }
}

/// The child allowlist: profile patterns a parent pattern covers, plus
/// every deny of both. Returns `(allow, dropped)`.
pub fn child_allow(parent: &[String], profile: &[String]) -> (Vec<String>, Vec<String>) {
    let is_deny = |p: &String| p.starts_with(DENY_PREFIX);
    let (mut allow, mut dropped) = (Vec::new(), Vec::new());
    for p in profile {
        if is_deny(p) || list_covers(parent, p) {
            allow.push(p.clone());
        } else {
            dropped.push(p.clone());
        }
    }
    for d in parent.iter().filter(|p| is_deny(p)) {
        if !allow.contains(d) {
            allow.push(d.clone());
        }
    }
    (allow, dropped)
}

fn check_lease_path(path: &str) -> Result<()> {
    if is_normal_path(path) {
        return Ok(());
    }
    Err(DelegationError::Invalid(format!(
        "lease path {path:?} must be absolute and normalized (no empty, `.` or `..` \
         component, no trailing slash); canonicalize it first"
    )))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Half of `left`, rounded up, so a node keeps room for itself and siblings.
fn half(left: u64) -> u64 {
    left.div_ceil(2)
}

/// The delegation log: fold plus the capped operations.
pub struct Delegation<S: LedgerStore> {
    store: S,
    state: State,
    offset: u64,
}

impl<S: LedgerStore> Delegation<S> {
    /// Reads and folds the log.
    pub fn open(store: S) -> Result<Self> {
        let mut d = Delegation {
            store,
            state: State::default(),
            offset: 0,
        };
        d.sync(false)?;
        Ok(d)
    }

    /// Folds records appended since the last read.
    pub fn refresh(&mut self) -> Result<()> {
        self.sync(false)
    }

    fn sync(&mut self, repair: bool) -> Result<()> {
        let bytes = self.store.read_from(self.offset)?;
        let mut pos = 0usize;
        while let Some(nl) = bytes[pos..].iter().position(|b| *b == b'\n') {
            let end = pos + nl + 1;
            let line = String::from_utf8_lossy(&bytes[pos..end - 1]);
            if !line.trim().is_empty() {
                match Rec::parse(line.trim()) {
                    Ok(Some(rec)) => self.state.apply(rec),
                    Ok(None) => {}
                    Err(LedgerError::Corrupt { reason, .. }) => {
                        let at = self.offset + pos as u64;
                        self.offset = at;
                        return Err(LedgerError::Corrupt { offset: at, reason }.into());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            pos = end;
        }
        self.offset += pos as u64;
        if repair && pos < bytes.len() {
            self.store.truncate(self.offset)?;
        }
        Ok(())
    }

    fn append(&mut self, rec: &Rec) -> Result<()> {
        self.sync(true)?;
        let mut line = rec.to_line().into_bytes();
        line.push(b'\n');
        self.store.append(&line)?;
        self.sync(true)
    }

    fn node(&self, id: &str) -> Result<&Node> {
        self.state
            .nodes
            .get(id)
            .ok_or_else(|| DelegationError::UnknownNode(id.to_string()))
    }

    /// A node, as folded: a pure lookup in the folded state; it reads, settles
    /// or writes nothing.
    pub fn poll(&self, id: &str) -> Option<&Node> {
        self.state.nodes.get(id)
    }

    /// Opens a goal: the root node (the orchestrator, depth 0, running).
    /// `max_depth` above [`MAX_DEPTH_CEILING`] or zero is refused.
    pub fn begin(&mut self, goal: &Goal<'_>, now: i64) -> Result<()> {
        let Goal {
            id: goal_id,
            task,
            budget,
            allow,
            max_depth,
            max_live,
        } = *goal;
        if !valid_id(goal_id) {
            return Err(DelegationError::Invalid(format!(
                "goal id {goal_id:?}: use 1-{MAX_ID_LEN} of a-z, 0-9, - and _"
            )));
        }
        if !(1..=MAX_DEPTH_CEILING).contains(&max_depth) {
            return Err(DelegationError::Invalid(format!(
                "max_depth {max_depth}: must be 1 to {MAX_DEPTH_CEILING}"
            )));
        }
        if max_live == 0 {
            return Err(DelegationError::Invalid("max_live_processes 0".into()));
        }
        self.sync(true)?;
        if self.state.nodes.contains_key(goal_id) {
            return Err(DelegationError::GoalExists(goal_id.to_string()));
        }
        self.append(&Rec::Goal {
            goal_id: goal_id.to_string(),
            task: task.to_string(),
            budget,
            allow: allow.to_vec(),
            max_depth,
            max_live,
            at: now,
        })
    }

    fn limits(&self, node: &Node) -> GoalLimits {
        self.state
            .goals
            .get(&node.goal_id)
            .copied()
            .unwrap_or(GoalLimits {
                max_depth: DEFAULT_MAX_DEPTH,
                max_live: DEFAULT_MAX_LIVE_PROCESSES,
            })
    }

    /// May `id` delegate (live, and above the goal's depth limit)?
    pub fn may_delegate(&self, id: &str) -> bool {
        self.state
            .nodes
            .get(id)
            .is_some_and(|n| !n.status.is_terminal() && n.depth < self.limits(n).max_depth)
    }

    /// Carves a child out of `parent`. `own` is what the parent spent itself
    /// so far. The child is recorded `queued`; [`Self::start`] runs it.
    pub fn delegate(
        &mut self,
        parent: &str,
        req: &DelegateRequest<'_>,
        own: Spend,
        now: i64,
    ) -> Result<Handle> {
        self.sync(true)?;
        let p = self.node(parent)?.clone();
        if p.status.is_terminal() {
            return Err(DelegationError::Settled(p.id));
        }
        let max_depth = self.limits(&p).max_depth;
        if p.depth >= max_depth {
            return Err(DelegationError::DepthCap {
                node: p.id,
                depth: p.depth,
                max_depth,
            });
        }
        if p.mode == Mode::Consult && req.mode == Mode::Execute {
            return Err(DelegationError::Invalid(
                "a consult node can only delegate consult work".into(),
            ));
        }
        if req.task.trim().is_empty() || req.profile.trim().is_empty() {
            return Err(DelegationError::Invalid(
                "profile and task are required".into(),
            ));
        }
        let b = req.budget;
        if b.tokens == Some(0) || b.steps == Some(0) || b.minutes == Some(0) {
            return Err(DelegationError::Invalid("a requested budget of 0".into()));
        }
        if own.tokens < p.own.tokens || own.steps < p.own.steps {
            return Err(DelegationError::Invalid(format!(
                "own spend {own:?} is lower than the {:?} `{}` reported before",
                p.own, p.id
            )));
        }
        let left = self.state.available(&p, own);
        let exhausted = |what| DelegationError::BudgetExhausted {
            node: p.id.clone(),
            what,
        };
        if left.tokens == 0 {
            return Err(exhausted("tokens"));
        }
        if left.steps == 0 {
            return Err(exhausted("steps"));
        }
        if p.budget.deadline <= now {
            return Err(exhausted("time"));
        }
        let minutes_deadline = b
            .minutes
            .map(|m| now.saturating_add(i64::try_from(m.saturating_mul(60)).unwrap_or(i64::MAX)));
        let budget = Budget {
            tokens: b
                .tokens
                .unwrap_or_else(|| half(left.tokens))
                .min(left.tokens),
            steps: b
                .steps
                .unwrap_or_else(|| half(u64::from(left.steps)) as u32)
                .min(left.steps),
            deadline: minutes_deadline.map_or(p.budget.deadline, |d| d.min(p.budget.deadline)),
        };
        let (allow, dropped) = child_allow(&p.allow, req.profile_allow);
        let node_id = format!("{}.{}", p.id, p.children.len() + 1);
        self.append(&Rec::Spawn {
            node_id: node_id.clone(),
            parent_id: p.id.clone(),
            profile: req.profile.to_string(),
            task: req.task.to_string(),
            mode: req.mode,
            budget,
            allow: allow.clone(),
            dropped: dropped.clone(),
            workspace: req.workspace.map(str::to_string),
            parent_own: own,
            at: now,
        })?;
        Ok(Handle {
            node_id,
            depth: p.depth + 1,
            budget,
            allow,
            dropped,
        })
    }

    /// Marks a queued node running under process `lease`, unless the goal
    /// already has `max_live` delegated processes (then [`DelegationError::Queued`]).
    pub fn start(&mut self, id: &str, lease: Lease, session: Option<&str>, now: i64) -> Result<()> {
        self.sync(true)?;
        let n = self.node(id)?;
        if n.status != NodeStatus::Queued {
            return Err(DelegationError::WrongState {
                node: id.to_string(),
                status: n.status,
                want: "queued",
            });
        }
        let max = self.limits(n).max_live;
        let live = self.state.live_in(&n.goal_id);
        if live >= max {
            return Err(DelegationError::Queued { live, max });
        }
        self.append(&Rec::Start {
            node_id: id.to_string(),
            lease,
            session: session.map(str::to_string),
            at: now,
        })
    }

    /// A live lease on `path` itself or on one of its ancestors, as
    /// `(path, holder)`, ignoring `node`'s own. Unlike [`Self::lease_conflict`]
    /// a lease BELOW `path` does not count: the pre-check for a writer that
    /// will get a fresh directory under `path` (a new git worktree) must not
    /// be blocked by its sibling worktrees, only by a writer holding the
    /// whole tree above it.
    pub fn lease_ancestor_conflict(&self, path: &str, node: &str) -> Option<(String, String)> {
        self.state
            .leases
            .iter()
            .find(|(p, h)| {
                h.as_str() != node
                    && (p.as_str() == "/"
                        || path == p.as_str()
                        || path
                            .strip_prefix(p.as_str())
                            .is_some_and(|r| r.starts_with('/')))
            })
            .map(|(p, h)| (p.clone(), h.clone()))
    }

    /// The live lease `path` would conflict with (the same path, an ancestor
    /// or a descendant), as `(path, holder)`, ignoring `node`'s own; for the
    /// pre-check of a writer on the whole of `path`, where a lease anywhere
    /// inside it already blocks.
    pub fn lease_conflict(&self, path: &str, node: &str) -> Option<(String, String)> {
        self.state
            .lease_holder(path, node)
            .map(|(p, h)| (p.to_string(), h.to_string()))
    }

    /// Delegated processes running now in `goal`.
    pub fn live(&self, goal: &str) -> u32 {
        self.state.live_in(goal)
    }

    /// Every node, in id order.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.state.nodes.values()
    }

    /// The roots (goals) of the log, in log order.
    pub fn goals(&self) -> Vec<&Node> {
        let mut g: Vec<&Node> = self.nodes().filter(|n| n.parent.is_none()).collect();
        g.sort_by_key(|n| n.seq);
        g
    }

    /// What `id`'s subtree is charged to its parent: its whole carve while
    /// it runs, its own and its children's usage once settled, capped at the
    /// carve.
    pub fn charged(&self, id: &str) -> Spend {
        self.state.charge(id)
    }

    /// The paths `id` holds a write lease on.
    pub fn leases_of(&self, id: &str) -> Vec<String> {
        self.state
            .leases
            .iter()
            .filter(|(_, h)| h.as_str() == id)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// The node whose recorded session is `session` (the host stores the
    /// child's agent id there).
    pub fn by_session(&self, session: &str) -> Option<&Node> {
        self.state
            .nodes
            .values()
            .find(|n| n.session.as_deref() == Some(session))
    }

    /// Every queued node of every goal, oldest first.
    pub fn queued(&self) -> Vec<&Node> {
        let mut q: Vec<&Node> = self
            .state
            .nodes
            .values()
            .filter(|n| n.status == NodeStatus::Queued)
            .collect();
        q.sort_by_key(|n| n.seq);
        q
    }

    /// The oldest queued node of `goal` whose parent is still live.
    pub fn next_queued(&self, goal: &str) -> Option<&Node> {
        self.state
            .nodes
            .values()
            .filter(|n| n.goal_id == goal && n.status == NodeStatus::Queued)
            .filter(|n| {
                n.parent
                    .as_ref()
                    .and_then(|p| self.state.nodes.get(p))
                    .is_some_and(|p| !p.status.is_terminal())
            })
            .min_by_key(|n| n.seq)
    }

    /// Takes the write lease on `path` (re-entrant for its holder).
    pub fn lease_write(&mut self, id: &str, path: &str, now: i64) -> Result<()> {
        self.sync(true)?;
        let n = self.node(id)?;
        if n.mode != Mode::Execute || n.status != NodeStatus::Running {
            return Err(DelegationError::WrongState {
                node: id.to_string(),
                status: n.status,
                want: "a running execute node",
            });
        }
        check_lease_path(path)?;
        let busy = |s: &State| {
            s.lease_holder(path, id)
                .map(|(p, h)| DelegationError::LeaseBusy {
                    path: p.to_string(),
                    holder: h.to_string(),
                })
        };
        if let Some(e) = busy(&self.state) {
            return Err(e);
        }
        if self.state.leases.get(path).map(String::as_str) == Some(id) {
            return Ok(());
        }
        self.append(&Rec::WriteLease {
            node_id: id.to_string(),
            path: path.to_string(),
            at: now,
        })?;
        // A racing writer's lease may have landed first.
        if self.state.leases.get(path).map(String::as_str) == Some(id) {
            return Ok(());
        }
        Err(
            busy(&self.state).unwrap_or_else(|| DelegationError::LeaseBusy {
                path: path.to_string(),
                holder: "?".into(),
            }),
        )
    }

    /// Gives a write lease back before the node settles.
    pub fn release(&mut self, id: &str, path: &str, now: i64) -> Result<()> {
        self.sync(true)?;
        check_lease_path(path)?;
        if self.state.leases.get(path).map(String::as_str) != Some(id) {
            return Ok(());
        }
        self.append(&Rec::Release {
            node_id: id.to_string(),
            path: path.to_string(),
            at: now,
        })
    }

    /// Settles a node. `Outcome::Ok` becomes `done` only with a complete
    /// report and every child settled; otherwise `incomplete`.
    ///
    /// `used` must be the node's MEASURED own usage (its session's usage
    /// records), for every outcome: it is what the parent is charged, and
    /// the rest of the carve goes back to the parent. It is never taken
    /// below the highest own spend the node reported while delegating, and
    /// `Outcome::Budget` is always charged the whole carve. A host that
    /// cannot measure passes the whole carve ([`Spend::all_of`]).
    pub fn settle(
        &mut self,
        id: &str,
        outcome: Outcome,
        used: Spend,
        report: Option<Report>,
        reason: Option<&str>,
        now: i64,
    ) -> Result<NodeStatus> {
        self.sync(true)?;
        let n = self.node(id)?;
        if n.status.is_terminal() {
            return Err(DelegationError::Settled(id.to_string()));
        }
        let mut why: Vec<String> = reason.map(str::to_string).into_iter().collect();
        // Never below what the node already reported; a budget stop spent
        // its whole carve.
        let mut used = used.max(n.own);
        if outcome == Outcome::Budget {
            used = used.max(Spend::all_of(n.budget));
        }
        let status = match outcome {
            Outcome::Failed => NodeStatus::Failed,
            Outcome::Cancelled => NodeStatus::Cancelled,
            Outcome::Budget => NodeStatus::Budget,
            Outcome::Ok => {
                let mut problems = match &report {
                    None => vec!["no completion report".to_string()],
                    Some(r) => r.problems(n.mode),
                };
                let open = n
                    .children
                    .iter()
                    .filter(|c| {
                        self.state
                            .nodes
                            .get(*c)
                            .is_some_and(|c| !c.status.is_terminal())
                    })
                    .count();
                if open > 0 {
                    problems.push(format!("{open} delegated run(s) not settled"));
                }
                if problems.is_empty() {
                    NodeStatus::Done
                } else {
                    why.extend(problems);
                    NodeStatus::Incomplete
                }
            }
        };
        self.append(&Rec::Settle {
            node_id: id.to_string(),
            status,
            used,
            report,
            reason: (!why.is_empty()).then(|| why.join("; ")),
            at: now,
        })?;
        Ok(self.node(id)?.status)
    }

    /// Settles running nodes whose process is gone (`interrupted`) and
    /// queued nodes whose parent settled (`cancelled`). Returns their ids.
    ///
    /// A dead node's usage is unknown, so it is charged its whole carve
    /// unless `measured` gives what it really spent (e.g. from its session's
    /// usage records): a crash must never refund a budget it may have burned.
    /// A queued node never started, so it spent nothing.
    pub fn recover(
        &mut self,
        now: i64,
        alive: impl Fn(&Lease) -> bool,
        measured: impl Fn(&Node) -> Option<Spend>,
    ) -> Result<Vec<String>> {
        self.sync(true)?;
        let mut todo = Vec::new();
        for n in self.state.nodes.values() {
            let parent_done = n
                .parent
                .as_ref()
                .and_then(|p| self.state.nodes.get(p))
                .is_some_and(|p| p.status.is_terminal());
            match (n.status, n.lease) {
                (NodeStatus::Running, Some(l)) if !alive(&l) => {
                    let used = measured(n).unwrap_or_else(|| Spend::all_of(n.budget));
                    todo.push((
                        n.id.clone(),
                        NodeStatus::Interrupted,
                        used.max(n.own),
                        "process gone",
                    ))
                }
                (NodeStatus::Queued, _) if parent_done => todo.push((
                    n.id.clone(),
                    NodeStatus::Cancelled,
                    Spend::default(),
                    "parent settled before it started",
                )),
                _ => {}
            }
        }
        for (id, status, used, reason) in &todo {
            self.append(&Rec::Settle {
                node_id: id.clone(),
                status: *status,
                used: *used,
                report: None,
                reason: Some((*reason).to_string()),
                at: now,
            })?;
        }
        Ok(todo.into_iter().map(|t| t.0).collect())
    }

    /// The goal's nodes, top-down (depth-first, spawn order).
    pub fn tree(&self, goal: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        let mut stack = vec![goal.to_string()];
        while let Some(id) = stack.pop() {
            if let Some(n) = self.state.nodes.get(&id) {
                out.push(n);
                stack.extend(n.children.iter().rev().cloned());
            }
        }
        out
    }

    /// The audit view of a goal: one line per node, indented by depth.
    pub fn render_tree(&self, goal: &str) -> Vec<String> {
        self.tree(goal)
            .into_iter()
            .map(|n| {
                let charged = self.state.charge(&n.id);
                let mut line = format!(
                    "{}{} {} {:?} [{}] tokens {}/{} steps {}/{}",
                    "  ".repeat(n.depth as usize),
                    n.id,
                    n.profile.as_deref().unwrap_or("orchestrator"),
                    n.status,
                    match n.mode {
                        Mode::Consult => "consult",
                        Mode::Execute => "execute",
                    },
                    charged.tokens,
                    n.budget.tokens,
                    charged.steps,
                    n.budget.steps,
                );
                if !n.allow.is_empty() {
                    line.push_str(&format!(" allow {:?}", n.allow));
                }
                if let Some(w) = &n.workspace {
                    line.push_str(&format!(" workspace {w}"));
                }
                if let Some(s) = &n.session {
                    line.push_str(&format!(" session {s}"));
                }
                if let Some(r) = &n.report {
                    line.push_str(&format!(" report: {}", r.summary));
                }
                if let Some(r) = &n.reason {
                    line.push_str(&format!(" ({r})"));
                }
                line
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "delegation_tests.rs"]
mod tests;
