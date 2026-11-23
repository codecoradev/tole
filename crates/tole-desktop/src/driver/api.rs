//! The calls the drivers make on the serve, as a trait.
//!
//! A driver is written against [`Api`], not against `tole_client::Client`, so
//! its tests script the replies and the event stream without a socket. The
//! implementation for the real client, [`ClientApi`], only forwards: no retry,
//! no decoding, no decision of its own (the client already never retries a
//! call). Its one job besides forwarding is to give the client the bounds a
//! driver relies on: a `Client` from `Client::new` has no request timeout, so
//! a hung server would hold a driver's single call slot for ever.
//!
//! Every method blocks, and a call may take as long as [`CALL_TIMEOUT`] per
//! read and per body, so a driver never calls one on the thread that owns its
//! state or while holding a lock.
//!
//! The prompt call is the exception to the bounds: it answers when its turn
//! ends, which takes minutes, so [`ClientApi`] makes it on a clone of the client
//! with a read timeout of [`PROMPT_TIMEOUT`] (two hours) instead of
//! [`CALL_TIMEOUT`]. A turn that runs longer is reported as a failed call, which
//! the task driver answers by asking the serve whether the turn still runs.
//!
//! Each driver adds the calls it needs below the marker; a method that no
//! driver calls yet is not here.

use std::time::Duration;

use tole_client::{
    Approval, ApprovalList, CancelReply, Client, CronJobState, CronJobs, CronRuns, Decision,
    DecisionReply, EntriesPage, Error, InterruptReply, InterruptRequest, PromptReply,
    PromptRequest, ReconnectPolicy, ResumingStream, SessionStatus, SkillsState, StreamItem,
    ToolsState,
};

/// One session's event stream, as a source of items. It reconnects by itself
/// where the transport can (the real one is a [`ResumingStream`]); an `Err` or
/// an `Ok(None)` means it is over and will yield nothing more.
pub trait EventSource: Send {
    /// The next item. Blocks, also while reconnecting.
    fn next_item(&mut self) -> Result<Option<StreamItem>, Error>;
}

impl EventSource for ResumingStream {
    fn next_item(&mut self) -> Result<Option<StreamItem>, Error> {
        ResumingStream::next_item(self)
    }
}

/// The `after` of a stream that wants no entries: the approvals driver reads
/// the approval frames only, and a server that replays every entry of a long
/// session on each (re)connect would be a cost for nothing.
pub const NO_REPLAY: u64 = u64::MAX;

pub trait Api: Send + Sync + 'static {
    // ---- approvals ----

    /// `GET /approvals`, of one session or of all.
    fn approvals(&self, session: Option<&str>) -> Result<ApprovalList, Error>;

    /// `POST /approvals/{id}/decision`. The row carries the fingerprint a
    /// delegated child's allow needs (`adapter::approvals::row_for_decision`).
    fn decide_row(&self, row: &Approval, decision: Decision) -> Result<DecisionReply, Error>;

    /// The event stream of one session, from the entry after `after`. Does not
    /// block: the connection is made by the first `next_item`.
    fn follow_events(&self, session: &str, after: u64) -> Box<dyn EventSource>;

    // ---- the task driver: one session ----

    /// `GET /sessions/{id}/entries`: the entries after `after`.
    fn entries(
        &self,
        session: &str,
        after: u64,
        limit: Option<usize>,
    ) -> Result<EntriesPage, Error>;

    /// `GET /sessions/{id}/status`.
    fn status(&self, session: &str) -> Result<SessionStatus, Error>;

    /// `POST /sessions/{id}/prompt`. Answers when the turn stops, so it may
    /// block for minutes (see the module docs).
    fn prompt(&self, session: &str, req: &PromptRequest) -> Result<PromptReply, Error>;

    /// `POST /sessions/{id}/cancel`.
    fn cancel(&self, session: &str) -> Result<CancelReply, Error>;

    /// `POST /sessions/{id}/interrupt`: a note to the running turn.
    fn interrupt(&self, session: &str, req: &InterruptRequest) -> Result<InterruptReply, Error>;

    /// `POST /approvals/{id}/decision` of a serve row (no fingerprint; a
    /// delegated child's row goes through [`Api::decide_row`]).
    fn decide(&self, approval_id: &str, decision: Decision) -> Result<DecisionReply, Error>;

    // ---- the skills driver: the Skills and Connected tools pages (read-only) ----

    /// `GET /skills`: the skills with their approval state, the drafts waiting
    /// for review and the archive. A `notes` line says part of it was unreadable.
    fn skills(&self) -> Result<SkillsState, Error>;

    /// `GET /tools`: the connected tools with their trust state. No server is
    /// started to answer it.
    fn tools(&self) -> Result<ToolsState, Error>;

    // ---- the automations driver: the Automations page ----

    /// `GET /cron/jobs`: the jobs with their schedule, next run and last
    /// result, and whether a scheduler is alive.
    fn cron_jobs(&self) -> Result<CronJobs, Error>;

    /// `GET /cron/runs` of one job, newest first, at most `limit`.
    fn cron_runs(&self, job: &str, limit: usize) -> Result<CronRuns, Error>;

    /// `POST /cron/jobs/{id}/pause`. Always allowed.
    fn pause_cron_job(&self, id: &str) -> Result<CronJobState, Error>;

    /// `POST /cron/jobs/{id}/resume`. A `403` is an agent's job the owner has
    /// not reviewed (done at a terminal), a `409` a server in plan mode.
    fn resume_cron_job(&self, id: &str) -> Result<CronJobState, Error>;
}

/// The longest one blocking read of a call, and one answer's body, may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest a prompt call may wait for its turn to end.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// The real client as an [`Api`], with the call bounds applied.
pub struct ClientApi {
    /// Every call but the prompt: bounded reads and bodies.
    short: Client,
    /// The prompt: reads bounded by [`PROMPT_TIMEOUT`], a bounded body.
    long: Client,
}

impl ClientApi {
    /// Wrap `client`, bounding each read of a call and each body by
    /// [`CALL_TIMEOUT`] (a call is then over after about a minute at worst).
    /// The prompt call waits up to [`PROMPT_TIMEOUT`] for its turn.
    pub fn new(client: Client) -> Result<Self, Error> {
        let long = (client.clone())
            .with_request_timeout(PROMPT_TIMEOUT)?
            .with_body_deadline(CALL_TIMEOUT)?;
        let short = client
            .with_request_timeout(CALL_TIMEOUT)?
            .with_body_deadline(CALL_TIMEOUT)?;
        Ok(ClientApi { short, long })
    }
}

impl Api for ClientApi {
    fn approvals(&self, session: Option<&str>) -> Result<ApprovalList, Error> {
        self.short.approvals(session)
    }

    fn decide_row(&self, row: &Approval, decision: Decision) -> Result<DecisionReply, Error> {
        self.short.decide_row(row, decision)
    }

    fn follow_events(&self, session: &str, after: u64) -> Box<dyn EventSource> {
        Box::new(
            self.short
                .follow_events(session, after, ReconnectPolicy::default()),
        )
    }

    fn entries(
        &self,
        session: &str,
        after: u64,
        limit: Option<usize>,
    ) -> Result<EntriesPage, Error> {
        self.short.entries(session, after, limit)
    }

    fn status(&self, session: &str) -> Result<SessionStatus, Error> {
        self.short.status(session)
    }

    fn prompt(&self, session: &str, req: &PromptRequest) -> Result<PromptReply, Error> {
        self.long.prompt(session, req)
    }

    fn cancel(&self, session: &str) -> Result<CancelReply, Error> {
        self.short.cancel(session)
    }

    fn interrupt(&self, session: &str, req: &InterruptRequest) -> Result<InterruptReply, Error> {
        self.short.interrupt(session, req)
    }

    fn decide(&self, approval_id: &str, decision: Decision) -> Result<DecisionReply, Error> {
        self.short.decide(approval_id, decision)
    }

    fn skills(&self) -> Result<SkillsState, Error> {
        self.short.skills()
    }

    fn tools(&self) -> Result<ToolsState, Error> {
        self.short.tools()
    }

    fn cron_jobs(&self) -> Result<CronJobs, Error> {
        self.short.cron_jobs()
    }

    fn cron_runs(&self, job: &str, limit: usize) -> Result<CronRuns, Error> {
        self.short.cron_runs(Some(job), Some(limit))
    }

    fn pause_cron_job(&self, id: &str) -> Result<CronJobState, Error> {
        self.short.pause_cron_job(id)
    }

    fn resume_cron_job(&self, id: &str) -> Result<CronJobState, Error> {
        self.short.resume_cron_job(id)
    }
}
