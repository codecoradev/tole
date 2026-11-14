//! The durable approval queue of delegated children (issue #714, design
//! `delegation-approvals-design.md`): records and the fold. Nothing here is
//! wired into a child approver yet; the host switch is
//! [`ApprovalsMode`], default `off`.
//!
//! One append-only JSONL log next to the delegation log, over a
//! [`LedgerStore`], folded like [`crate::delegation`]: no clock, no spawn,
//! no lock (`now` is passed in; the host holds the exclusive lock around
//! every append). Five records: `request`, `escalate`, `decide`, `consume`,
//! `expire`.
//!
//! The fold re-validates every record against the delegation tree, so a
//! forged or out-of-order line does no more than a legal one could:
//!
//! - a `request` must name a real child, carry the chain the tree gives
//!   (recomputed, not trusted), a well-formed [`fingerprint`] and a bounded
//!   window; otherwise it is ignored;
//! - the **first valid** `decide` in log order wins; a later one is
//!   `race_lost`. An agent may decide only as the *current* level, only if
//!   that level was eligible (its own allow list allows the call, inside
//!   the workspace guard, and the call is not always-owner), and its
//!   decision carries the request's fingerprint;
//! - `consume` is valid once, only for the requesting node, only after an
//!   allow, with the fingerprint of the call about to run (write-ahead: the
//!   host appends it before it returns Allow);
//! - no answer is **denied**: past `expires_at`, or once the requesting node
//!   is settled, an unconsumed request is `expired` and every later record
//!   for it is refused.
//!
//! A level's window opens at the request (or at the last explicit or `dead`
//! escalation) and lasts `level_secs`; eligible levels take turns, skipped
//! ones cost no time, and the owner decides at any time while the request is
//! pending. Bounds the fold and [`Approvals::append`] enforce:
//! [`MAX_RECORD_BYTES`] per line, [`MAX_LOG_BYTES`] for the log (over it new
//! requests are refused, and the fold stops reading past a hard cap, so a
//! flood cannot grow memory without bound), [`MAX_INPUT_BYTES`] and
//! [`MAX_DISPLAY_BYTES`] in a request. The per-node and per-goal pending
//! caps belong to the host that builds requests (a later PR).
//!
//! Limits of the fold: the host-asserted flags of a request (`secrets`,
//! `protected`, `origin`) are taken as recorded; a forged request can only
//! ask a question, never decide it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::approval::{
    pattern_allows, pattern_verdict, workspace_guard_ok, PatternVerdict, ToolRequest,
    COMMAND_PATTERN_PREFIX,
};
use crate::delegation::{Delegation, Node};
use crate::ledger::{LedgerError, LedgerStore};
use crate::tool::Risk;

/// Version of the approval record format.
pub const APPROVALS_VERSION: u32 = 1;
/// New requests are refused once the log is this big.
pub const MAX_LOG_BYTES: u64 = 8 << 20;
/// Slack past [`MAX_LOG_BYTES`] for decisions of requests already asked; the
/// fold reads no further than the sum.
const LOG_SLACK: u64 = 1 << 20;
/// Longest accepted line.
pub const MAX_RECORD_BYTES: usize = 32 << 10;
/// Redacted input kept in a request; a larger input makes it owner-only.
pub const MAX_INPUT_BYTES: usize = 16 << 10;
/// Host-built display kept in a request.
pub const MAX_DISPLAY_BYTES: usize = 2 << 10;
/// Default and ceiling of `[delegation] approval_secs`; default of
/// `approval_level_secs`.
pub const DEFAULT_APPROVAL_SECS: i64 = 900;
pub const MAX_APPROVAL_SECS: i64 = 3600;
pub const DEFAULT_LEVEL_SECS: i64 = 120;

/// `[delegation] approvals`: `off` is today's behavior exactly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalsMode {
    #[default]
    Off,
    Owner,
    Chain,
}

/// Where a tool comes from; anything but a built-in is always-owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Builtin,
    Mcp,
    Plugin,
    Skill,
}

/// Why an agent level cannot decide a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    OwnerOnly,
    NoDecideTool,
    OutsideAuthority,
}

/// One agent level of a request's chain (level 1 is the child's parent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Level {
    pub node: String,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Skip>,
}

/// The always-owner classes the classifier can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerOnly {
    Secrets,
    Protected,
    ExternalEffect,
    OutsideWorkspace,
    Oversized,
}

/// A child's question. Built by the child's host; never a model string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub req: String,
    pub goal: String,
    pub node: String,
    pub session: String,
    pub tool: String,
    pub risk: Risk,
    pub origin: Origin,
    pub target: Option<String>,
    /// The redacted copy (`null` when over [`MAX_INPUT_BYTES`]).
    pub input: Value,
    pub display: String,
    /// The decider could not see everything the fingerprint covers.
    pub truncated: bool,
    /// The tool is bound in a `[secrets]` release, or the input names a
    /// secret or redaction changed it (the host decides: it owns the redactor).
    pub secrets: bool,
    /// The resolved target is a trust, approval or config file or lies in
    /// the data dir (host-resolved, symlinks followed).
    pub protected: bool,
    pub fingerprint: String,
    pub workspace: String,
    pub chain: Vec<Level>,
    pub level_secs: i64,
    pub expires_at: i64,
    pub at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Tool,
    Cli,
    Serve,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    Agent { node: String, level: u32 },
    Owner,
}

/// `explicit` and `dead` move the request up; `timeout` and `skipped` only
/// record what the clock and the chain already did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalateReason {
    Timeout,
    Skipped,
    Explicit,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpireReason {
    Timeout,
    Orphaned,
    NodeSettled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
// A record is built, written and dropped: boxing the request buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum Record {
    Request(Request),
    Escalate {
        req: String,
        from: String,
        to: Option<String>,
        reason: EscalateReason,
        at: i64,
    },
    Decide {
        req: String,
        by: By,
        verdict: Choice,
        via: Via,
        fingerprint: String,
        at: i64,
    },
    Consume {
        req: String,
        node: String,
        fingerprint: String,
        at: i64,
    },
    Expire {
        req: String,
        reason: ExpireReason,
        at: i64,
    },
}

const KINDS: &[&str] = &["request", "escalate", "decide", "consume", "expire"];

impl Record {
    pub fn to_line(&self) -> String {
        let mut v = serde_json::to_value(self).expect("approval records always serialize");
        if let Value::Object(m) = &mut v {
            m.insert("v".into(), APPROVALS_VERSION.into());
        }
        v.to_string()
    }

    /// `Ok(None)`: an unknown kind (ignored). Corrupt: not a valid record
    /// (unknown field, wrong type, not JSON).
    fn parse(line: &str) -> Result<Option<Record>, LedgerError> {
        let bad = |e: serde_json::Error| LedgerError::Corrupt {
            offset: 0,
            reason: e.to_string(),
        };
        let mut v: Value = serde_json::from_str(line).map_err(bad)?;
        let kind = v.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !KINDS.contains(&kind) {
            return Ok(None);
        }
        let found = v.get("v").and_then(Value::as_u64).unwrap_or(0);
        if found > u64::from(APPROVALS_VERSION) {
            return Err(LedgerError::NewerVersion {
                kind: kind.to_string(),
                found,
            });
        }
        if let Value::Object(m) = &mut v {
            m.remove("v");
        }
        serde_json::from_value(v).map(Some).map_err(bad)
    }
}

/// Why a record was refused (nothing is recorded; the fold ignores it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("malformed request: {0}")]
    Malformed(&'static str),
    #[error("the request id is already used")]
    Duplicate,
    #[error("the chain does not match the delegation tree")]
    ChainMismatch,
    #[error("unknown request")]
    Unknown,
    #[error("the record is older than its request")]
    OutOfOrder,
    #[error("the request has expired: no answer is denied")]
    Expired,
    #[error("the fingerprint does not match the request")]
    Fingerprint,
    #[error("a decision was already recorded (race lost)")]
    RaceLost,
    #[error("the decider is not the current level")]
    NotCurrentLevel,
    #[error("the decision channel does not fit the decider")]
    BadVia,
    #[error("the request is not allowed")]
    NotAllowed,
    #[error("the request was already consumed")]
    AlreadyConsumed,
    #[error("only the requesting node may consume")]
    NotRequester,
    #[error("the request is already settled")]
    Settled,
    #[error("the expiry reason does not hold")]
    NoExpiry,
    #[error("the escalation does not follow the chain")]
    BadEscalation,
}

/// Errors from the approval log.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("refused: {0}")]
    Refused(#[from] Refusal),
    #[error("approvals log is full: new requests are denied")]
    LogFull,
    #[error("record over {MAX_RECORD_BYTES} bytes")]
    RecordTooLarge,
}

/// The delegation tree the fold reads: a node's parent, allow list, status.
pub trait Tree {
    fn node(&self, id: &str) -> Option<&Node>;
}

impl<S: LedgerStore> Tree for Delegation<S> {
    fn node(&self, id: &str) -> Option<&Node> {
        self.poll(id)
    }
}

/// SHA-256 over one call of one node: `"tole-approval-v1" 0x00 node 0x00
/// session 0x00 tool 0x00 canonical raw input 0x00 target 0x00 workspace`
/// (an absent target is empty), hex. Fields must not contain NUL; a request
/// that does is refused. It replaces the 64-bit loop-guard hash for durable
/// use.
pub fn fingerprint(
    node: &str,
    session: &str,
    tool: &str,
    input: &Value,
    target: Option<&str>,
    workspace: &str,
) -> String {
    let input = crate::turn::canonical_json(input);
    let mut h = Sha256::new();
    h.update(b"tole-approval-v1");
    for part in [
        node,
        session,
        tool,
        input.as_str(),
        target.unwrap_or(""),
        workspace,
    ] {
        h.update([0u8]);
        h.update(part.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

const COMMAND_TOOLS: &[&str] = &["run_command", "job_start"];

/// The always-owner classifier (design 2.3). Fail closed: anything it
/// cannot see is owner-only.
pub fn owner_only_class(r: &Request) -> Option<OwnerOnly> {
    let owner_record =
        r.tool.starts_with("cron") || r.tool.contains("approve") || r.tool.contains("trust");
    if r.secrets {
        Some(OwnerOnly::Secrets)
    } else if r.protected || owner_record {
        Some(OwnerOnly::Protected)
    } else if r.origin != Origin::Builtin {
        Some(OwnerOnly::ExternalEffect)
    } else if r.truncated {
        Some(OwnerOnly::Oversized)
    } else if !COMMAND_TOOLS.contains(&r.tool.as_str())
        && !r.target.as_deref().is_some_and(workspace_guard_ok)
    {
        Some(OwnerOnly::OutsideWorkspace)
    } else {
        None
    }
}

fn tool_request(r: &Request) -> ToolRequest<'_> {
    ToolRequest {
        tool: &r.tool,
        risk: r.risk,
        input: &r.input,
        description: r.display.clone(),
        target: r.target.clone(),
    }
}

/// Rule 2.2.3 and 2.2.5: would `allow` (a level's own list, deny patterns
/// winning) allow this request on its own, inside the workspace guard?
pub fn level_may_decide(allow: &[String], r: &Request) -> bool {
    let tr = tool_request(r);
    if r.risk != Risk::Write || pattern_verdict(allow, &tr) != PatternVerdict::Allow {
        return false;
    }
    match &r.target {
        Some(t) => workspace_guard_ok(t),
        // A command has no path: only a command pattern of this list decides.
        None => {
            COMMAND_TOOLS.contains(&r.tool.as_str())
                && allow
                    .iter()
                    .any(|p| p.starts_with(COMMAND_PATTERN_PREFIX) && pattern_allows(p, &tr))
        }
    }
}

/// Does the list hold a `delegate_decide` grant?
fn has_decide_tool(allow: &[String]) -> bool {
    let input = Value::Null;
    let tr = ToolRequest {
        tool: "delegate_decide",
        risk: Risk::Write,
        input: &input,
        description: String::new(),
        target: None,
    };
    pattern_verdict(allow, &tr) == PatternVerdict::Allow
}

/// The chain of `node` for `r`, read from the tree: parent first, root last.
/// `None` for a node whose parent is missing from the tree.
pub fn build_chain(tree: &dyn Tree, node: &Node, r: &Request) -> Option<Vec<Level>> {
    let owner_only = owner_only_class(r).is_some();
    let mut out = Vec::new();
    let mut cur = node.parent.clone();
    while let Some(id) = cur {
        let n = tree.node(&id)?;
        let skipped = if owner_only {
            Some(Skip::OwnerOnly)
        } else if !has_decide_tool(&n.allow) {
            Some(Skip::NoDecideTool)
        } else if !level_may_decide(&n.allow, r) {
            Some(Skip::OutsideAuthority)
        } else {
            None
        };
        out.push(Level {
            node: id,
            depth: n.depth,
            skipped,
        });
        cur = n.parent.clone();
    }
    Some(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub by: By,
    pub verdict: Choice,
    pub via: Via,
    pub at: i64,
}

/// One request as folded.
#[derive(Debug, Clone)]
pub struct Entry {
    pub req: Request,
    pub decision: Option<Decision>,
    pub consumed_at: Option<i64>,
    pub expired: Option<(ExpireReason, i64)>,
    /// Valid later decisions that lost to the first.
    pub race_lost: u32,
    /// Consumes after the first.
    pub replays: u32,
    /// Index among the eligible levels where the window clock restarted.
    cursor: usize,
    win_start: i64,
}

/// What a request is at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Waiting; `level` is the chain index of the current agent level, or
    /// `None` when only the owner is left.
    Pending {
        level: Option<usize>,
    },
    Allowed,
    Denied,
    Consumed,
    Expired(ExpireReason),
}

impl Entry {
    fn eligible(&self) -> Vec<usize> {
        (0..self.req.chain.len())
            .filter(|i| self.req.chain[*i].skipped.is_none())
            .collect()
    }

    /// Position among the eligible levels at `at` (== their count: owner).
    fn slot(&self, at: i64) -> usize {
        let n = self.eligible().len();
        let (mut k, mut start) = (self.cursor, self.win_start);
        while k < n && at >= start.saturating_add(self.req.level_secs) {
            k += 1;
            start = start.saturating_add(self.req.level_secs);
        }
        k
    }

    /// Chain index of the current agent level at `at`.
    fn current(&self, at: i64) -> Option<usize> {
        self.eligible().get(self.slot(at)).copied()
    }
}

/// The folded log.
#[derive(Debug, Clone, Default)]
pub struct State {
    entries: BTreeMap<String, Entry>,
    /// Records the fold refused (forged, out of order, late, replayed).
    pub rejected: u32,
    /// The log went past its hard cap; the rest is not read.
    pub overflow: bool,
}

fn id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl State {
    fn open_entry(&mut self, id: &str, at: i64) -> Result<&mut Entry, Refusal> {
        let e = self.entries.get_mut(id).ok_or(Refusal::Unknown)?;
        if at < e.req.at {
            return Err(Refusal::OutOfOrder);
        }
        if at >= e.req.expires_at || e.expired.is_some() {
            return Err(Refusal::Expired);
        }
        Ok(e)
    }

    fn apply(&mut self, rec: Record, tree: &dyn Tree) -> Result<(), Refusal> {
        match rec {
            Record::Request(r) => self.add(r, tree),
            Record::Escalate {
                req,
                from,
                to,
                reason,
                at,
            } => {
                let e = self.open_entry(&req, at)?;
                if e.decision.is_some() {
                    return Err(Refusal::Settled);
                }
                if !e.req.chain.iter().any(|l| l.node == from) {
                    return Err(Refusal::BadEscalation);
                }
                let advance = matches!(reason, EscalateReason::Explicit | EscalateReason::Dead);
                if advance {
                    let elig = e.eligible();
                    let k = e.slot(at);
                    let cur = elig.get(k).map(|i| e.req.chain[*i].node.as_str());
                    let next = elig.get(k + 1).map(|i| e.req.chain[*i].node.clone());
                    if cur != Some(from.as_str()) || next != to {
                        return Err(Refusal::BadEscalation);
                    }
                    e.cursor = k + 1;
                    e.win_start = at;
                }
                Ok(())
            }
            Record::Decide {
                req,
                by,
                verdict,
                via,
                fingerprint,
                at,
            } => {
                let e = self.open_entry(&req, at)?;
                if fingerprint != e.req.fingerprint {
                    return Err(Refusal::Fingerprint);
                }
                match &by {
                    By::Owner if via == Via::Tool => return Err(Refusal::BadVia),
                    By::Owner => {}
                    By::Agent { node, level } => {
                        if via != Via::Tool {
                            return Err(Refusal::BadVia);
                        }
                        let idx = e.current(at).ok_or(Refusal::NotCurrentLevel)?;
                        let l = &e.req.chain[idx];
                        if l.node != *node || *level as usize != idx + 1 {
                            return Err(Refusal::NotCurrentLevel);
                        }
                        // `current` only yields eligible levels, and `skipped` was
                        // recomputed from the immutable `Node.allow` of the log
                        // when the request was folded, so no second look here.
                    }
                }
                if e.decision.is_some() {
                    e.race_lost += 1;
                    return Err(Refusal::RaceLost);
                }
                e.decision = Some(Decision {
                    by,
                    verdict,
                    via,
                    at,
                });
                Ok(())
            }
            Record::Consume {
                req,
                node,
                fingerprint,
                at,
            } => {
                let e = self.open_entry(&req, at)?;
                let allowed = e
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.verdict == Choice::Allow && at >= d.at);
                if !allowed {
                    return Err(Refusal::NotAllowed);
                }
                if e.consumed_at.is_some() {
                    e.replays += 1;
                    return Err(Refusal::AlreadyConsumed);
                }
                if node != e.req.node {
                    return Err(Refusal::NotRequester);
                }
                if fingerprint != e.req.fingerprint {
                    return Err(Refusal::Fingerprint);
                }
                e.consumed_at = Some(at);
                Ok(())
            }
            Record::Expire { req, reason, at } => {
                let e = self.entries.get_mut(&req).ok_or(Refusal::Unknown)?;
                let settled = tree
                    .node(&e.req.node)
                    .is_none_or(|n| n.status.is_terminal());
                let holds = match reason {
                    ExpireReason::Timeout => at >= e.req.expires_at,
                    ExpireReason::Orphaned | ExpireReason::NodeSettled => settled,
                };
                if !holds {
                    return Err(Refusal::NoExpiry);
                }
                let denied = e
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.verdict == Choice::Deny);
                if denied || e.consumed_at.is_some() || e.expired.is_some() {
                    return Err(Refusal::Settled);
                }
                e.expired = Some((reason, at));
                Ok(())
            }
        }
    }

    fn add(&mut self, r: Request, tree: &dyn Tree) -> Result<(), Refusal> {
        let bad = Refusal::Malformed;
        if !id_ok(&r.req) || !r.req.starts_with("dap-") {
            return Err(bad("request id"));
        }
        if self.entries.contains_key(&r.req) {
            return Err(Refusal::Duplicate);
        }
        let node = tree.node(&r.node).ok_or(bad("unknown node"))?;
        if node.parent.is_none() || node.goal_id != r.goal {
            return Err(bad("node or goal"));
        }
        if node.session.as_deref().is_some_and(|s| s != r.session) {
            return Err(bad("session"));
        }
        let nul = |s: &str| s.contains('\0');
        let text = [&r.node, &r.session, &r.tool, &r.workspace, &r.display];
        if r.risk != Risk::Write
            || text.iter().any(|s| s.is_empty() || nul(s))
            || r.target.as_deref().is_some_and(|t| t.is_empty() || nul(t))
            || r.display.len() > MAX_DISPLAY_BYTES
        {
            return Err(bad("fields"));
        }
        if r.input.to_string().len() > MAX_INPUT_BYTES || !is_hex64(&r.fingerprint) {
            return Err(bad("input or fingerprint"));
        }
        let span = r.expires_at.checked_sub(r.at).unwrap_or(0);
        if !(1..=MAX_APPROVAL_SECS).contains(&r.level_secs)
            || !(1..=MAX_APPROVAL_SECS).contains(&span)
            || r.expires_at > node.budget.deadline
        {
            return Err(bad("window"));
        }
        if build_chain(tree, node, &r).as_ref() != Some(&r.chain) {
            return Err(Refusal::ChainMismatch);
        }
        let win_start = r.at;
        self.entries.insert(
            r.req.clone(),
            Entry {
                req: r,
                decision: None,
                consumed_at: None,
                expired: None,
                race_lost: 0,
                replays: 0,
                cursor: 0,
                win_start,
            },
        );
        Ok(())
    }

    /// A request as folded.
    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// What `id` is at `now`: a decision or a consume is final; otherwise an
    /// elapsed window, an `expire` record or a settled requesting node
    /// expires it.
    pub fn status(&self, id: &str, tree: &dyn Tree, now: i64) -> Option<Status> {
        let e = self.entries.get(id)?;
        let allow = e.decision.as_ref().map(|d| d.verdict);
        if e.consumed_at.is_some() {
            return Some(Status::Consumed);
        }
        if allow == Some(Choice::Deny) {
            return Some(Status::Denied);
        }
        if let Some((r, _)) = e.expired {
            return Some(Status::Expired(r));
        }
        if now >= e.req.expires_at {
            return Some(Status::Expired(ExpireReason::Timeout));
        }
        if tree
            .node(&e.req.node)
            .is_none_or(|n| n.status.is_terminal())
        {
            return Some(Status::Expired(ExpireReason::NodeSettled));
        }
        Some(match allow {
            Some(_) => Status::Allowed,
            None => Status::Pending {
                level: e.current(now),
            },
        })
    }
}

/// The approval log: a store plus the fold of everything read from it.
pub struct Approvals<S: LedgerStore> {
    store: S,
    state: State,
    offset: u64,
}

impl<S: LedgerStore> Approvals<S> {
    /// Reads and folds the log.
    pub fn open(store: S, tree: &dyn Tree) -> Result<Self, ApprovalError> {
        let mut a = Approvals {
            store,
            state: State::default(),
            offset: 0,
        };
        a.sync(tree, false)?;
        Ok(a)
    }

    /// Folds records appended since the last read.
    pub fn refresh(&mut self, tree: &dyn Tree) -> Result<(), ApprovalError> {
        self.sync(tree, false)
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    fn sync(&mut self, tree: &dyn Tree, repair: bool) -> Result<(), ApprovalError> {
        let bytes = self.store.read_from(self.offset)?;
        let mut pos = 0usize;
        while let Some(nl) = bytes[pos..].iter().position(|b| *b == b'\n') {
            if self.offset + pos as u64 >= MAX_LOG_BYTES + LOG_SLACK {
                self.state.overflow = true;
                break;
            }
            let end = pos + nl + 1;
            let line = &bytes[pos..end - 1];
            if line.len() <= MAX_RECORD_BYTES && !line.iter().all(u8::is_ascii_whitespace) {
                let parsed = std::str::from_utf8(line)
                    .map_err(|e| LedgerError::Corrupt {
                        offset: 0,
                        reason: e.to_string(),
                    })
                    .and_then(|l| Record::parse(l.trim()));
                match parsed {
                    Ok(Some(rec)) => {
                        if self.state.apply(rec, tree).is_err() {
                            self.state.rejected += 1;
                        }
                    }
                    Ok(None) => {}
                    Err(LedgerError::Corrupt { .. }) => self.state.rejected += 1,
                    Err(e) => return Err(e.into()),
                }
            } else if line.len() > MAX_RECORD_BYTES {
                self.state.rejected += 1;
            }
            pos = end;
        }
        self.offset += pos as u64;
        if repair && !self.state.overflow && pos < bytes.len() {
            self.store.truncate(self.offset)?;
        }
        Ok(())
    }

    /// Validates `rec` against a copy of the fold, then appends it. The
    /// writer and the fold share one rule set.
    pub fn append(&mut self, tree: &dyn Tree, rec: Record) -> Result<(), ApprovalError> {
        self.sync(tree, true)?;
        // The fold cannot see a node settle between two records, so the writer
        // refuses what the request's status no longer allows.
        if let Record::Escalate { req, at, .. }
        | Record::Decide { req, at, .. }
        | Record::Consume { req, at, .. } = &rec
        {
            if let Some(Status::Expired(_)) = self.state.status(req, tree, *at) {
                return Err(Refusal::Expired.into());
            }
        }
        let is_request = matches!(rec, Record::Request(_));
        self.state.clone().apply(rec.clone(), tree)?;
        let mut line = rec.to_line().into_bytes();
        line.push(b'\n');
        if line.len() > MAX_RECORD_BYTES {
            return Err(ApprovalError::RecordTooLarge);
        }
        let cap = if is_request {
            MAX_LOG_BYTES
        } else {
            MAX_LOG_BYTES + LOG_SLACK
        };
        if self.state.overflow || self.offset + line.len() as u64 > cap {
            return Err(ApprovalError::LogFull);
        }
        self.store.append(&line)?;
        self.sync(tree, true)
    }
}

#[cfg(test)]
#[path = "delegation_approvals_tests.rs"]
mod tests;
