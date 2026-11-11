//! Agent-mode cron runs (#579 part 2): one durable session per run, the
//! job's prompt re-sent verbatim, unattended approvals (design §5).
//!
//! Approvals — never auto-approved:
//! - **`tole serve --cron`** (in-process ticker): a Write that the job's
//!   `allow` list does not cover is parked in serve's `/approvals` queue
//!   (the `QueueApprover`, 15-min expiry → denied) and the run settles
//!   `awaiting_approval`. Resuming an approved run is #581.
//! - **standalone `tole cron tick`**: there is no queue, so such a Write is
//!   refused and the run settles `awaiting_approval`.
//!
//! In both modes the registry is built for a non-interactive approver, so
//! Destructive tools are structurally absent.
//!
//! Budgets: `max_minutes` (wall clock, cancels the turn) and `max_steps`
//! (tool calls, cancels the turn); both settle `budget`. The model pin
//! overrides the env model for this run only.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use tole_core::approval::{AllowlistApprover, Approver, ToolRequest, Verdict};
use tole_core::cancel::CancelToken;
use tole_core::ledger::{Job, RunStatus};
use tole_core::tool::Risk;
use tole_core::turn::{TurnObserver, TurnOutcome};

use super::field;
use crate::approvals::{ApprovalQueue, QueueApprover};

/// Default wall-clock budget of an agent run (minutes).
pub const DEFAULT_MAX_MINUTES: u64 = 15;
/// Default tool-call budget of an agent run.
pub const DEFAULT_MAX_STEPS: u64 = 32;

enum Inner {
    Allow(AllowlistApprover),
    Queue(QueueApprover),
}

/// The unattended approver: the job's allowlist, else park (serve) or
/// refuse (standalone). Remembers whether anything was refused.
pub struct CronApprover {
    inner: Inner,
    refused: Arc<AtomicBool>,
}

impl CronApprover {
    pub fn refused(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.refused)
    }
}

impl Approver for CronApprover {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        if req.risk == Risk::ReadOnly {
            return Verdict::Allow;
        }
        let v = match &self.inner {
            Inner::Allow(a) => a.decide(req),
            Inner::Queue(q) => q.decide(req),
        };
        if v == Verdict::Deny {
            self.refused.store(true, Ordering::SeqCst);
        }
        v
    }

    fn interactive(&self) -> bool {
        false
    }
}

/// Builds the approver for one run of `job` in session `session_id`.
pub fn approver(
    job: &Job,
    queue: Option<&Arc<ApprovalQueue>>,
    session_id: &str,
    session_path: &Path,
) -> CronApprover {
    let allow: Vec<String> = job
        .extra
        .get("allow")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let inner = match queue {
        Some(q) => Inner::Queue(QueueApprover::new(
            allow,
            Arc::clone(q),
            session_id,
            session_path.to_path_buf(),
        )),
        None => Inner::Allow(AllowlistApprover::allow_only(allow)),
    };
    CronApprover {
        inner,
        refused: Arc::new(AtomicBool::new(false)),
    }
}

/// Counts tool calls and cancels the turn past the step budget.
struct StepBudget {
    max: usize,
    count: AtomicUsize,
    tripped: Arc<AtomicBool>,
    cancel: CancelToken,
}

impl TurnObserver for StepBudget {
    fn tool_started(&self, _tool: &str, _input: &serde_json::Value) {
        if self.count.fetch_add(1, Ordering::SeqCst) + 1 >= self.max {
            self.tripped.store(true, Ordering::SeqCst);
            self.cancel.cancel();
        }
    }
}

/// The outcome of one agent run.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub status: RunStatus,
    pub session_id: Option<String>,
    pub text: String,
    pub error: Option<String>,
}

fn u64_field(job: &Job, key: &str, default: u64) -> u64 {
    job.extra
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(default)
}

/// Runs one agent job to its terminal status. Never panics on a broken
/// job: every failure is an `error` with a reason.
pub fn run_agent(job: &Job, queue: Option<&Arc<ApprovalQueue>>) -> AgentRun {
    let fail = |session_id: Option<String>, e: String| AgentRun {
        status: RunStatus::Error,
        session_id,
        text: String::new(),
        error: Some(e),
    };
    let prompt = field(job, "prompt").unwrap_or_default();
    let workdir = field(job, "workdir").unwrap_or_default();
    let mut cfg = match crate::turn_runner::provider_config() {
        Ok(c) => c,
        Err(e) => return fail(None, e),
    };
    if let Some(m) = field(job, "model") {
        cfg.model = m.to_string();
    }
    let canon = match Path::new(workdir).canonicalize() {
        Ok(p) => p,
        Err(e) => return fail(None, format!("workdir {workdir}: {e}")),
    };
    let dir = match crate::session_host::sessions_dir_for(workdir) {
        Ok(d) => d,
        Err(e) => return fail(None, e),
    };
    let session_id = crate::session_host::new_session_id("cron");
    let path = dir.join(format!("{session_id}.jsonl"));
    let approver = approver(job, queue, &session_id, &path);
    let refused = approver.refused();
    let mut reg = match crate::tool_roster::build(approver, crate::tool_roster::Roster::new(canon))
    {
        Ok(r) => r,
        Err(e) => return fail(None, e.to_string()),
    };
    let name = field(job, "name").unwrap_or(&job.id).to_string();
    let mut storage = match crate::session_host::create_session(
        &dir,
        &session_id,
        None,
        Some(&name),
        Some("cron"),
    ) {
        Ok(s) => s,
        Err(e) => return fail(None, e.to_string()),
    };
    let sid = Some(session_id.clone());
    use tole_core::storage::Storage;
    if let Err(e) = crate::tool_roster::register_todo(&mut reg, storage.entries(), false) {
        return fail(sid, e.to_string());
    }

    let cancel = CancelToken::new();
    let tripped = Arc::new(AtomicBool::new(false));
    let observer: Arc<dyn TurnObserver> = Arc::new(StepBudget {
        max: u64_field(job, "max_steps", DEFAULT_MAX_STEPS) as usize,
        count: AtomicUsize::new(0),
        tripped: Arc::clone(&tripped),
        cancel: cancel.clone(),
    });
    // Wall-clock budget: a timer that cancels the turn at the deadline.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let minutes = u64_field(job, "max_minutes", DEFAULT_MAX_MINUTES);
    let (timer_cancel, timer_tripped) = (cancel.clone(), Arc::clone(&tripped));
    let timer = std::thread::spawn(move || {
        if done_rx.recv_timeout(Duration::from_secs(minutes * 60))
            == Err(mpsc::RecvTimeoutError::Timeout)
        {
            timer_tripped.store(true, Ordering::SeqCst);
            timer_cancel.cancel();
        }
    });
    let plan = crate::turn_runner::TurnPlan {
        cfg,
        registry: &reg,
        system_prompt: None,
        cancel: &cancel,
        observer: Some(&observer),
        resolve_parked: false,
        compaction: None,
    };
    let outcome = crate::turn_runner::run_turn_on(&mut storage, &plan, prompt);
    drop(done_tx);
    let _ = timer.join();
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return fail(sid, e.into_message()),
    };
    let refused = refused.load(Ordering::SeqCst);
    let (status, text, error) = match outcome {
        TurnOutcome::Final { text, .. } if refused => (RunStatus::AwaitingApproval, text, None),
        TurnOutcome::Final { text, .. } => (RunStatus::Ok, text, None),
        TurnOutcome::ApprovalRequired { .. } => (RunStatus::AwaitingApproval, String::new(), None),
        TurnOutcome::Cancelled if tripped.load(Ordering::SeqCst) => (
            RunStatus::Budget,
            String::new(),
            Some("budget exceeded".into()),
        ),
        TurnOutcome::BudgetExhausted => (
            RunStatus::Budget,
            String::new(),
            Some("turn step limit".into()),
        ),
        TurnOutcome::Cancelled => (RunStatus::Cancelled, String::new(), None),
        other => (RunStatus::Error, String::new(), Some(format!("{other:?}"))),
    };
    AgentRun {
        status,
        session_id: sid,
        text,
        error,
    }
}
