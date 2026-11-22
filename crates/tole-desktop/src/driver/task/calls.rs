//! The calls the engine asks the I/O threads for, and what comes back.
//!
//! Each [`Lane`] is one thread with one call in flight (a lane's flag is set
//! when the engine hands it a call and cleared when its reply is applied). A
//! panic inside a call is that call's failure, never the thread's.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use tole_client::{
    ApprovalList, CancelReply, Decision as Verdict, DecisionReply, EntriesPage, Error,
    InterruptReply, InterruptRequest, PromptReply, PromptRequest, SessionStatus,
};
use tole_desktop_model::approvals::Decision;

use crate::driver::api::Api;
use crate::driver::guard::{guarded, panicked};

/// The threads that make calls: one call at a time on each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lane {
    /// History pages and the approvals queue (never both at once: the queue is
    /// fetched only after the history is in).
    Read,
    Status,
    Prompt,
    Stop,
    Steer,
    Decide,
}

pub(super) const LANES: [Lane; 6] = [
    Lane::Read,
    Lane::Status,
    Lane::Prompt,
    Lane::Stop,
    Lane::Steer,
    Lane::Decide,
];

impl Lane {
    pub(super) fn index(self) -> usize {
        self as usize
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Lane::Read => "task-read",
            Lane::Status => "task-status",
            Lane::Prompt => "task-prompt",
            Lane::Stop => "task-stop",
            Lane::Steer => "task-steer",
            Lane::Decide => "task-decide",
        }
    }
}

/// Whether a prompt call may still be made: the owner can take the send back
/// (Stop pressed before the call went out) until the lane thread starts it.
#[derive(Debug, Clone, Default)]
pub(super) struct Gate(Arc<AtomicU8>);

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const ABORTED: u8 = 2;

impl Gate {
    /// The lane thread is about to call; `false` when the send was taken back.
    pub(super) fn start(&self) -> bool {
        self.0
            .compare_exchange(QUEUED, STARTED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Whether the lane thread has started the call.
    pub(super) fn started(&self) -> bool {
        self.0.load(Ordering::SeqCst) == STARTED
    }

    /// Take the send back; `false` when the call already went out.
    pub(super) fn abort(&self) -> bool {
        self.0
            .compare_exchange(QUEUED, ABORTED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

pub(super) enum Call {
    Entries {
        after: u64,
    },
    Approvals {
        token: u64,
    },
    Status {
        token: u64,
        check: bool,
    },
    Prompt {
        req: PromptRequest,
        gate: Gate,
        /// Which send this is: a reply of an older one is dropped.
        generation: u64,
    },
    Cancel,
    Interrupt(InterruptRequest),
    Decide {
        id: String,
        decision: Decision,
    },
}

pub(super) enum Reply {
    Entries(u64, Result<EntriesPage, Error>),
    Approvals(u64, Result<ApprovalList, Error>),
    Status {
        token: u64,
        check: bool,
        reply: Result<SessionStatus, Error>,
    },
    /// `None`: the send was taken back and no call was made.
    Prompt(u64, Option<Result<PromptReply, Error>>),
    Cancel(Result<CancelReply, Error>),
    Interrupt(Result<InterruptReply, Error>),
    Decide(String, Result<DecisionReply, Error>),
}

fn caught<T>(call: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    guarded(call).unwrap_or_else(|| Err(panicked()))
}

/// Make one call on the serve; blocks. Never panics.
pub(super) fn run(api: &dyn Api, session: &str, call: Call) -> Reply {
    match call {
        Call::Entries { after } => {
            Reply::Entries(after, caught(|| api.entries(session, after, Some(PAGE))))
        }
        Call::Approvals { token } => {
            Reply::Approvals(token, caught(|| api.approvals(Some(session))))
        }
        Call::Status { token, check } => Reply::Status {
            token,
            check,
            reply: caught(|| api.status(session)),
        },
        Call::Prompt {
            req,
            gate,
            generation,
        } => Reply::Prompt(
            generation,
            gate.start().then(|| caught(|| api.prompt(session, &req))),
        ),
        Call::Cancel => Reply::Cancel(caught(|| api.cancel(session))),
        Call::Interrupt(req) => Reply::Interrupt(caught(|| api.interrupt(session, &req))),
        Call::Decide { id, decision } => {
            let verdict: Verdict = crate::adapter::approvals::decision_to_client(decision);
            let reply = caught(|| api.decide(&id, verdict));
            Reply::Decide(id, reply)
        }
    }
}

/// Entries asked for per history page (serve's own most).
const PAGE: usize = 500;
