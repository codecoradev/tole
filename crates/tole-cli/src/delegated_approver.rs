//! The approver of a delegated child (#714, design
//! `delegation-approvals-design.md` section 4): instead of denying at EOF it
//! asks through the durable approvals log and parks in `decide`, polling the
//! log, until the request is allowed, denied or expired. Selected only when
//! `[delegation] approvals` is not `off`; `off` keeps today's stdio approver.
//!
//! Nothing here decides: until a deciding surface lands (the owner's
//! command, serve, the agent tools), every request expires to a denial.
//!
//! - The allow list comes first and is unchanged: a deny pattern is a final
//!   denial, an allow pattern allows, a ReadOnly call is allowed.
//! - Anything else is appended as a request, then the child blocks. The wait
//!   is capped by the node's deadline (the window is built that way) and
//!   ends on the session's `CancelToken`. It never extends the deadline.
//! - On an allow the child appends `Consume` BEFORE it returns Allow
//!   (write-ahead): the call runs once, and a crash after the consume cannot
//!   run it again. Anything else is Deny, which takes the existing path (the
//!   turn ends `ApprovalRequired`).
//! - `interactive()` is false, so Destructive tools are not registered at
//!   all in a delegated child; `decide` still denies one.
//! - serve and cron keep their own deny-then-resume path; `resume_turn` is
//!   untouched.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tole_core::approval::{pattern_verdict, Approver, PatternVerdict, ToolRequest, Verdict};
use tole_core::cancel::CancelToken;
use tole_core::delegation::Delegation;
use tole_core::delegation_approvals::{
    fingerprint, ApprovalError, Approvals, ExpireReason, NewRequest, Origin, Record, Status,
    DEFAULT_APPROVAL_SECS, DEFAULT_LEVEL_SECS,
};
use tole_core::secrets::SecretSet;
use tole_core::tool::Risk;

use crate::config::DelegationConfig;
use crate::ledger_file::FileStore;

/// The approvals log next to a delegation log.
pub fn approvals_path(log: &Path) -> PathBuf {
    log.with_file_name("approvals.jsonl")
}

/// The delegation log and its approvals log: one directory, one lock.
#[derive(Debug, Clone)]
pub struct Store {
    log: PathBuf,
}

type Folded = (Delegation<FileStore>, Approvals<FileStore>);

impl Store {
    pub fn new(log: PathBuf) -> Self {
        Self { log }
    }

    fn open(&self) -> Result<Folded, String> {
        let e = |e: &dyn std::fmt::Display| format!("approvals: {e}");
        let d = Delegation::open(FileStore::new(self.log.clone())).map_err(|x| e(&x))?;
        let a =
            Approvals::open(FileStore::new(approvals_path(&self.log)), &d).map_err(|x| e(&x))?;
        Ok((d, a))
    }

    /// Reads without the lock: only whole lines are ever folded.
    pub fn read<R>(
        &self,
        f: impl FnOnce(&Delegation<FileStore>, &Approvals<FileStore>) -> R,
    ) -> Result<R, String> {
        let (d, a) = self.open()?;
        Ok(f(&d, &a))
    }

    /// Runs `f` under the exclusive lock the delegation log uses (held for
    /// milliseconds, never across a wait). The approvals file is created
    /// private (0600) on Unix.
    pub fn write<R>(
        &self,
        f: impl FnOnce(&Delegation<FileStore>, &mut Approvals<FileStore>) -> Result<R, ApprovalError>,
    ) -> Result<R, String> {
        let e = |e: &dyn std::fmt::Display| format!("approvals: {e}");
        let dir = self.log.parent().ok_or("approvals: log has no directory")?;
        std::fs::create_dir_all(dir).map_err(|x| e(&x))?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.log.with_extension("lock"))
            .map_err(|x| e(&x))?;
        lock.lock().map_err(|x| e(&x))?;
        create_private(&approvals_path(&self.log)).map_err(|x| e(&x))?;
        let (d, mut a) = self.open()?;
        f(&d, &mut a).map_err(|x| e(&x))
    }
}

/// Creates `path` if absent, mode 0600 where the platform has modes.
fn create_private(path: &Path) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path).map(drop)
}

/// Built-in tools whose effect a level can read from the request; any other
/// name (MCP, plugin, skill) is an external effect and always the owner's.
const READABLE_TOOLS: &[&str] = &["write_file", "edit_file", "run_command", "job_start"];

fn origin_of(tool: &str) -> Origin {
    if READABLE_TOOLS.contains(&tool) {
        Origin::Builtin
    } else {
        Origin::Plugin
    }
}

/// The deepest existing ancestor of `p`, canonical (symlinks followed), with
/// the rest appended.
fn resolve(p: &Path) -> PathBuf {
    let (mut cur, mut rest) = (p.to_path_buf(), Vec::new());
    loop {
        if let Ok(c) = cur.canonicalize() {
            return rest.iter().rev().fold(c, |a, x| a.join(x));
        }
        match (cur.file_name().map(|n| n.to_owned()), cur.parent()) {
            (Some(n), Some(par)) => {
                rest.push(n);
                cur = par.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Does `target` (relative to `root`) resolve into the tole data dir or a
/// `.tole` directory: where trust and approval records live?
fn is_protected(root: &Path, target: Option<&str>, data_dir: Option<&Path>) -> bool {
    let Some(t) = target else { return false };
    let real = resolve(&root.join(t));
    let in_data = data_dir
        .and_then(|d| d.canonicalize().ok())
        .is_some_and(|d| real.starts_with(d));
    in_data || real.components().any(|c| c.as_os_str() == ".tole")
}

fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("{:016x}", h.finish())
}

pub fn real_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Time and waiting, injectable so a test needs no real second.
#[derive(Clone)]
pub struct Timing {
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub nap: Arc<dyn Fn() + Send + Sync>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            now: Arc::new(real_now),
            nap: Arc::new(|| std::thread::sleep(Duration::from_secs(1))),
        }
    }
}

/// What the host knows about the child it is approving for.
pub struct Params {
    pub patterns: Vec<String>,
    pub auto_write: bool,
    /// The canonical workspace root.
    pub root: PathBuf,
    pub data_dir: Option<PathBuf>,
    pub secrets: Arc<SecretSet>,
    pub cancel: CancelToken,
    pub timing: Timing,
}

pub struct DelegatedApprover {
    p: Params,
    store: Store,
    node: String,
    workspace: String,
    approval_secs: i64,
    level_secs: i64,
}

/// The approver for this process, or `None` (today's stdio approver) when
/// `[delegation] approvals` is `off`, this is not a delegated child, or it is
/// a discussion participant (read-only for good: it never asks for a Write).
pub fn for_child(
    env: &dyn Fn(&str) -> Option<String>,
    cfg: Option<&DelegationConfig>,
    p: Params,
) -> Option<DelegatedApprover> {
    let mode = cfg.and_then(|c| c.approvals).unwrap_or_default();
    if mode == tole_core::delegation_approvals::ApprovalsMode::Off
        || tole_core::child_env::discussion_participant(env)
        || tole_core::child_env::depth(env) < 1
    {
        return None;
    }
    let (log, node) = tole_core::child_env::delegation(env)?;
    let secs = |v: Option<u64>, d: i64| v.and_then(|n| i64::try_from(n).ok()).unwrap_or(d);
    let workspace = p.root.to_str()?.to_string();
    Some(DelegatedApprover {
        store: Store::new(log),
        node,
        workspace,
        approval_secs: secs(cfg.and_then(|c| c.approval_secs), DEFAULT_APPROVAL_SECS),
        level_secs: secs(cfg.and_then(|c| c.approval_level_secs), DEFAULT_LEVEL_SECS),
        p,
    })
}

impl DelegatedApprover {
    fn now(&self) -> i64 {
        (self.p.timing.now)()
    }

    /// Asks, parks, and returns the verdict.
    fn park(&self, req: &ToolRequest<'_>) -> Result<Verdict, String> {
        let session = self
            .store
            .read(|d, _| d.poll(&self.node).and_then(|n| n.session.clone()))?
            .ok_or("approvals: the node has no session in the delegation log")?;
        let shown = self.p.secrets.redact_value(req.input);
        let line = format!("{}: {}", req.tool, req.description);
        let display = self.p.secrets.redact(&line).into_owned();
        let root = self.p.root.as_path();
        let nonce = nonce();
        let new = NewRequest {
            node: &self.node,
            session: &session,
            tool: req.tool,
            origin: origin_of(req.tool),
            target: req.target.as_deref(),
            raw_input: req.input,
            input: &shown,
            display: &display,
            secrets: &shown != req.input,
            protected: is_protected(root, req.target.as_deref(), self.p.data_dir.as_deref()),
            workspace: &self.workspace,
            approval_secs: self.approval_secs,
            level_secs: self.level_secs,
            nonce: &nonce,
        };
        let now = self.now();
        let id = self.store.write(|d, a| a.request(d, &new, now))?;
        // What the child is about to run, as the child sees it: the consume
        // carries this, not what the log says.
        let fp = fingerprint(
            &self.node,
            &session,
            req.tool,
            req.input,
            req.target.as_deref(),
            &self.workspace,
        );
        loop {
            if self.p.cancel.is_cancelled() {
                return Ok(Verdict::Deny);
            }
            let now = self.now();
            let status = self.store.read(|d, a| a.state().status(&id, d, now))?;
            match status.ok_or("approvals: the request vanished")? {
                Status::Pending { .. } => (self.p.timing.nap)(),
                Status::Allowed => return Ok(self.consume(&id, &fp, now)),
                Status::Expired(ExpireReason::Timeout) => {
                    let rec = Record::Expire {
                        req: id.clone(),
                        reason: ExpireReason::Timeout,
                        at: now,
                    };
                    // Best effort: the fold derives the expiry anyway.
                    let _ = self.store.write(|d, a| a.append(d, rec));
                    return Ok(Verdict::Deny);
                }
                Status::Denied | Status::Consumed | Status::Expired(_) => return Ok(Verdict::Deny),
            }
        }
    }

    /// Write-ahead: Allow only once the consume is on disk.
    fn consume(&self, id: &str, fp: &str, now: i64) -> Verdict {
        let rec = Record::Consume {
            req: id.to_string(),
            node: self.node.clone(),
            fingerprint: fp.to_string(),
            at: now,
        };
        match self.store.write(|d, a| a.append(d, rec)) {
            Ok(()) => Verdict::Allow,
            Err(e) => {
                eprintln!("{e}");
                Verdict::Deny
            }
        }
    }
}

impl Approver for DelegatedApprover {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        if req.risk == Risk::Destructive {
            return Verdict::Deny;
        }
        match pattern_verdict(&self.p.patterns, req) {
            PatternVerdict::Deny | PatternVerdict::Ask => return Verdict::Deny,
            PatternVerdict::Allow => return Verdict::Allow,
            PatternVerdict::NoMatch if self.p.auto_write => return Verdict::Allow,
            PatternVerdict::NoMatch => {}
        }
        if req.risk == Risk::ReadOnly {
            return Verdict::Allow;
        }
        self.park(req).unwrap_or_else(|e| {
            // Fail closed: a refused or failed request is a denial.
            eprintln!("{e}");
            Verdict::Deny
        })
    }

    /// No human is on the other end: Destructive is not registered.
    fn interactive(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "delegated_approver_tests.rs"]
mod tests;
