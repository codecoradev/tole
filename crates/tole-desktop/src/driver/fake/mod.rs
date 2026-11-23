//! A scripted [`Api`] for driver tests: no socket, no serve.
//!
//! Replies are queued per call, in the order the driver will ask for them. A
//! reply is either ready (`reply_*`) or held back by a gate (`gate_*`): the
//! call blocks until the test sends its result, which is how a test makes a
//! reply arrive late, after a loss or a newer call. `panic_*` makes the next
//! call panic. A call with nothing queued fails as a refused connection, so a
//! test that forgot to script it sees a failure, not a hang (but see
//! [`FakeApi::always_status`]). The test waits for calls with `wait_*`, which
//! are bounded in real time only so that a broken driver fails the test instead
//! of hanging it.
//!
//! Every stream a test wants the driver to open is queued with
//! [`FakeApi::open_stream`], which returns the sender of its frames. Dropping
//! the sender ends the stream cleanly; a stream nobody scripted stays silent.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use tole_client::{
    Approval, ApprovalList, CancelReply, CronJobState, CronJobs, CronRuns, Decision, DecisionReply,
    EntriesPage, InterruptReply, InterruptRequest, PromptReply, PromptRequest, Result,
    SessionStatus, SkillsState, ToolsState,
};

mod script;

pub(crate) use self::script::{refused, Frame};
use self::script::{FakeSource, Queue, Scripted, PATIENCE};
use super::api::{Api, EventSource};

#[derive(Default)]
struct Script {
    lists: Queue<ApprovalList>,
    decisions: Queue<DecisionReply>,
    entries: Queue<EntriesPage>,
    statuses: Queue<SessionStatus>,
    prompts: Queue<PromptReply>,
    cancels: Queue<CancelReply>,
    interrupts: Queue<InterruptReply>,
    decides: Queue<DecisionReply>,
    skills: Queue<SkillsState>,
    tools: Queue<ToolsState>,
    cron_jobs: Queue<CronJobs>,
    cron_runs: Queue<CronRuns>,
    pauses: Queue<CronJobState>,
    resumes: Queue<CronJobState>,
    always_status: Option<SessionStatus>,
    streams: HashMap<String, VecDeque<Receiver<Frame>>>,
    /// Senders of streams nobody scripted: they stay silent, open.
    parked: Vec<Sender<Frame>>,
    list_calls: Vec<Option<String>>,
    decision_calls: Vec<(String, Decision, Option<String>)>,
    entries_calls: Vec<u64>,
    status_calls: usize,
    prompt_calls: Vec<String>,
    cancel_calls: usize,
    interrupt_calls: Vec<String>,
    decide_calls: Vec<(String, Decision)>,
    skills_calls: usize,
    tools_calls: usize,
    cron_jobs_calls: usize,
    cron_runs_calls: Vec<(String, usize)>,
    pause_calls: Vec<String>,
    resume_calls: Vec<String>,
    opens: HashMap<String, usize>,
    afters: Vec<u64>,
}

#[derive(Default)]
pub(crate) struct FakeApi {
    script: Mutex<Script>,
    changed: Condvar,
}

/// `reply_*`, `gate_*` and `panic_*` for one queue.
macro_rules! scripted {
    ($queue:ident, $ty:ty, $reply:ident, $gate:ident, $panic:ident) => {
        pub(crate) fn $reply(&self, r: Result<$ty>) {
            self.script().$queue.push_back(Scripted::Now(r));
        }

        /// The next such call blocks until the returned sender is used.
        pub(crate) fn $gate(&self) -> Sender<Result<$ty>> {
            let (tx, rx) = channel();
            self.script().$queue.push_back(Scripted::Gate(rx));
            tx
        }

        /// The next such call panics.
        pub(crate) fn $panic(&self) {
            self.script().$queue.push_back(Scripted::Panic);
        }
    };
}

// The whole scripting surface is here for every driver's tests; one test file
// does not use every knob.
#[allow(dead_code)]
impl FakeApi {
    pub(crate) fn new() -> Arc<Self> {
        Arc::default()
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    scripted!(lists, ApprovalList, reply_list, gate_list, panic_list);
    scripted!(
        decisions,
        DecisionReply,
        reply_decision,
        gate_decision,
        panic_decision
    );
    scripted!(
        entries,
        EntriesPage,
        reply_entries,
        gate_entries,
        panic_entries
    );
    scripted!(
        statuses,
        SessionStatus,
        reply_status,
        gate_status,
        panic_status
    );
    scripted!(
        prompts,
        PromptReply,
        reply_prompt,
        gate_prompt,
        panic_prompt
    );
    scripted!(
        cancels,
        CancelReply,
        reply_cancel,
        gate_cancel,
        panic_cancel
    );
    scripted!(
        interrupts,
        InterruptReply,
        reply_interrupt,
        gate_interrupt,
        panic_interrupt
    );
    scripted!(
        decides,
        DecisionReply,
        reply_decide,
        gate_decide,
        panic_decide
    );
    scripted!(skills, SkillsState, reply_skills, gate_skills, panic_skills);
    scripted!(tools, ToolsState, reply_tools, gate_tools, panic_tools);
    scripted!(
        cron_jobs,
        CronJobs,
        reply_cron_jobs,
        gate_cron_jobs,
        panic_cron_jobs
    );
    scripted!(
        cron_runs,
        CronRuns,
        reply_cron_runs,
        gate_cron_runs,
        panic_cron_runs
    );
    scripted!(pauses, CronJobState, reply_pause, gate_pause, panic_pause);
    scripted!(
        resumes,
        CronJobState,
        reply_resume,
        gate_resume,
        panic_resume
    );

    /// A status call with nothing queued answers this (a poll that goes on).
    pub(crate) fn always_status(&self, status: SessionStatus) {
        self.script().always_status = Some(status);
    }

    /// The next stream the driver opens for `session`; its frames come from
    /// the returned sender.
    pub(crate) fn open_stream(&self, session: &str) -> Sender<Frame> {
        let (tx, rx) = channel();
        self.script()
            .streams
            .entry(session.to_owned())
            .or_default()
            .push_back(rx);
        tx
    }

    /// How many `GET /approvals` calls were made.
    pub(crate) fn list_calls(&self) -> usize {
        self.script().list_calls.len()
    }

    /// The `session` filter of every `GET /approvals` call.
    pub(crate) fn list_sessions(&self) -> Vec<Option<String>> {
        self.script().list_calls.clone()
    }

    /// `(id, verdict, fingerprint)` of every `decide_row` call so far.
    pub(crate) fn decisions(&self) -> Vec<(String, Decision, Option<String>)> {
        self.script().decision_calls.clone()
    }

    /// The `after` of every entries call.
    pub(crate) fn entries_calls(&self) -> Vec<u64> {
        self.script().entries_calls.clone()
    }

    pub(crate) fn status_calls(&self) -> usize {
        self.script().status_calls
    }

    /// The text of every prompt call.
    pub(crate) fn prompts(&self) -> Vec<String> {
        self.script().prompt_calls.clone()
    }

    pub(crate) fn cancels(&self) -> usize {
        self.script().cancel_calls
    }

    pub(crate) fn interrupts(&self) -> Vec<String> {
        self.script().interrupt_calls.clone()
    }

    pub(crate) fn decides(&self) -> Vec<(String, Decision)> {
        self.script().decide_calls.clone()
    }

    /// How many `GET /skills` calls were made.
    pub(crate) fn skills_calls(&self) -> usize {
        self.script().skills_calls
    }

    /// How many `GET /tools` calls were made.
    pub(crate) fn tools_calls(&self) -> usize {
        self.script().tools_calls
    }

    /// How many `GET /cron/jobs` calls were made.
    pub(crate) fn cron_jobs_calls(&self) -> usize {
        self.script().cron_jobs_calls
    }

    /// `(job, limit)` of every `GET /cron/runs` call.
    pub(crate) fn cron_runs_calls(&self) -> Vec<(String, usize)> {
        self.script().cron_runs_calls.clone()
    }

    /// The id of every pause call.
    pub(crate) fn pauses(&self) -> Vec<String> {
        self.script().pause_calls.clone()
    }

    /// The id of every resume call.
    pub(crate) fn resumes(&self) -> Vec<String> {
        self.script().resume_calls.clone()
    }

    pub(crate) fn opens(&self, session: &str) -> usize {
        self.script().opens.get(session).copied().unwrap_or(0)
    }

    /// The `after` of every stream opened, in order.
    pub(crate) fn afters(&self) -> Vec<u64> {
        self.script().afters.clone()
    }

    /// Wait until `count(script)` reaches `n`; panics after [`PATIENCE`].
    fn wait(&self, what: &str, n: usize, count: impl Fn(&Script) -> usize) {
        let guard = self.script();
        let (guard, out) = self
            .changed
            .wait_timeout_while(guard, PATIENCE, |s| count(s) < n)
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            !out.timed_out(),
            "timed out waiting for {n} {what} (saw {})",
            count(&guard)
        );
    }

    pub(crate) fn wait_list_calls(&self, n: usize) {
        self.wait("GET /approvals calls", n, |s| s.list_calls.len());
    }

    pub(crate) fn wait_decisions(&self, n: usize) {
        self.wait("decision calls", n, |s| s.decision_calls.len());
    }

    pub(crate) fn wait_entries(&self, n: usize) {
        self.wait("entries calls", n, |s| s.entries_calls.len());
    }

    pub(crate) fn wait_status(&self, n: usize) {
        self.wait("status calls", n, |s| s.status_calls);
    }

    pub(crate) fn wait_prompts(&self, n: usize) {
        self.wait("prompt calls", n, |s| s.prompt_calls.len());
    }

    pub(crate) fn wait_cancels(&self, n: usize) {
        self.wait("cancel calls", n, |s| s.cancel_calls);
    }

    pub(crate) fn wait_interrupts(&self, n: usize) {
        self.wait("interrupt calls", n, |s| s.interrupt_calls.len());
    }

    pub(crate) fn wait_decides(&self, n: usize) {
        self.wait("decide calls", n, |s| s.decide_calls.len());
    }

    pub(crate) fn wait_skills(&self, n: usize) {
        self.wait("GET /skills calls", n, |s| s.skills_calls);
    }

    pub(crate) fn wait_tools(&self, n: usize) {
        self.wait("GET /tools calls", n, |s| s.tools_calls);
    }

    pub(crate) fn wait_cron_jobs(&self, n: usize) {
        self.wait("GET /cron/jobs calls", n, |s| s.cron_jobs_calls);
    }

    pub(crate) fn wait_cron_runs(&self, n: usize) {
        self.wait("GET /cron/runs calls", n, |s| s.cron_runs_calls.len());
    }

    pub(crate) fn wait_pauses(&self, n: usize) {
        self.wait("pause calls", n, |s| s.pause_calls.len());
    }

    pub(crate) fn wait_resumes(&self, n: usize) {
        self.wait("resume calls", n, |s| s.resume_calls.len());
    }

    pub(crate) fn wait_opens(&self, session: &str, n: usize) {
        let session = session.to_owned();
        self.wait("stream opens", n, move |s| {
            s.opens.get(&session).copied().unwrap_or(0)
        });
    }

    /// Record a call, take its scripted reply and settle it outside the lock.
    fn call<T>(
        &self,
        record: impl FnOnce(&mut Script),
        pick: impl FnOnce(&mut Script) -> Option<Scripted<T>>,
    ) -> Result<T> {
        let next = {
            let mut s = self.script();
            record(&mut s);
            self.changed.notify_all();
            pick(&mut s)
        };
        next.map_or_else(|| Err(refused()), Scripted::settle)
    }
}

impl Api for FakeApi {
    fn approvals(&self, session: Option<&str>) -> Result<ApprovalList> {
        let session = session.map(str::to_owned);
        self.call(|s| s.list_calls.push(session), |s| s.lists.pop_front())
    }

    fn decide_row(&self, row: &Approval, decision: Decision) -> Result<DecisionReply> {
        let fingerprint = row
            .extra
            .get("fingerprint")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let id = row.id.clone();
        self.call(
            |s| s.decision_calls.push((id, decision, fingerprint)),
            |s| s.decisions.pop_front(),
        )
    }

    fn follow_events(&self, session: &str, after: u64) -> Box<dyn EventSource> {
        let mut s = self.script();
        *s.opens.entry(session.to_owned()).or_default() += 1;
        s.afters.push(after);
        self.changed.notify_all();
        let rx = s
            .streams
            .get_mut(session)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| {
                let (tx, rx) = channel();
                s.parked.push(tx);
                rx
            });
        Box::new(FakeSource(rx))
    }

    fn entries(&self, _session: &str, after: u64, _limit: Option<usize>) -> Result<EntriesPage> {
        self.call(|s| s.entries_calls.push(after), |s| s.entries.pop_front())
    }

    fn status(&self, _session: &str) -> Result<SessionStatus> {
        self.call(
            |s| s.status_calls += 1,
            |s| {
                (s.statuses.pop_front())
                    .or_else(|| s.always_status.clone().map(|st| Scripted::Now(Ok(st))))
            },
        )
    }

    fn prompt(&self, _session: &str, req: &PromptRequest) -> Result<PromptReply> {
        let text = req.text.clone();
        self.call(|s| s.prompt_calls.push(text), |s| s.prompts.pop_front())
    }

    fn cancel(&self, _session: &str) -> Result<CancelReply> {
        self.call(|s| s.cancel_calls += 1, |s| s.cancels.pop_front())
    }

    fn interrupt(&self, _session: &str, req: &InterruptRequest) -> Result<InterruptReply> {
        let text = req.text.clone();
        self.call(
            |s| s.interrupt_calls.push(text),
            |s| s.interrupts.pop_front(),
        )
    }

    fn decide(&self, approval_id: &str, decision: Decision) -> Result<DecisionReply> {
        let id = approval_id.to_owned();
        self.call(
            |s| s.decide_calls.push((id, decision)),
            |s| s.decides.pop_front(),
        )
    }

    fn skills(&self) -> Result<SkillsState> {
        self.call(|s| s.skills_calls += 1, |s| s.skills.pop_front())
    }

    fn tools(&self) -> Result<ToolsState> {
        self.call(|s| s.tools_calls += 1, |s| s.tools.pop_front())
    }

    fn cron_jobs(&self) -> Result<CronJobs> {
        self.call(|s| s.cron_jobs_calls += 1, |s| s.cron_jobs.pop_front())
    }

    fn cron_runs(&self, job: &str, limit: usize) -> Result<CronRuns> {
        let job = job.to_owned();
        self.call(
            |s| s.cron_runs_calls.push((job, limit)),
            |s| s.cron_runs.pop_front(),
        )
    }

    fn pause_cron_job(&self, id: &str) -> Result<CronJobState> {
        let id = id.to_owned();
        self.call(|s| s.pause_calls.push(id), |s| s.pauses.pop_front())
    }

    fn resume_cron_job(&self, id: &str) -> Result<CronJobState> {
        let id = id.to_owned();
        self.call(|s| s.resume_calls.push(id), |s| s.resumes.pop_front())
    }
}
