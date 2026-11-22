//! The return of the prompt call.
//!
//! A reply (any `stopReason`: finished, stopped, cancelled, waiting for an
//! approval) is the row of its `approval`, when there is one, then
//! [`TaskEvent::TurnEnded`] (a row already decided: see [`super::reply_approval`]).
//! The reply's `text` is not used: the answer is an entry (a reply carries no
//! `seq`, and the model replaces any draft with the entry).
//!
//! **A bare `approval_required`.** serve omits `approval` when the request was
//! decided or expired before the reply was built (`approval_required_body`). The
//! stream then carries the decision (`approval_resolved`), and an allow has
//! already resumed the turn: a `TurnEnded` now would end a turn that is running a
//! write. So a bare `approval_required` reply says nothing itself and asks for a
//! status check instead (`Ended { check_status: true }`). The driver rule: when
//! the model is already `run.resuming` the allow's [`super::ResumePolling`] is
//! watching and nothing more is needed; otherwise run
//! [`super::BusyPolling::foreign`], whose `Ready` settles the model (the decision
//! frame may be late or lost: a not-busy snapshot ends the turn, a busy one keeps
//! it and ends it when the server is idle).
//!
//! A failed call is not an event: the driver calls `TaskState::send_failed`
//! with [`PromptOutcome::Failed`]'s `server_busy` (a 409: another face's turn
//! holds the session). `check_status` asks the driver to poll `GET /status` with
//! [`super::BusyPolling::foreign`]. It is set for a 409 (the busy turn is not
//! this task's: it may end with no answer entry and no `ready`, such as a cancel,
//! so nothing else would end the model's `Working`) and for every failure that
//! may have come after the request reached the server, because the turn may run
//! anyway: a dropped connection or timeout (`Io`), a reply the client could not
//! read (`Protocol`, `Decode`), a 408 and any 5xx (a proxy or a crash in front
//! of a running turn). Not set when nothing was sent (`Connect`, a bad url, an
//! insecure transport or socket, a bad setting, https unsupported) or the server
//! refused the request (any other status, 429 included).

use super::approval::reply_approval;
use tole_client::{Error, PromptReply};
use tole_desktop_model::task::TaskEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Apply these events; poll the status when `check_status`.
    Ended {
        events: Vec<TaskEvent>,
        check_status: bool,
    },
    /// Call `send_failed(sent, server_busy)`; read the status when `check_status`.
    Failed {
        server_busy: bool,
        check_status: bool,
    },
}

/// Whether a failed call may have been received by the server.
pub fn may_have_been_received(e: &Error) -> bool {
    match e {
        Error::Io(_) | Error::Protocol(_) | Error::Decode(_) => true,
        Error::Status { status, .. } => matches!(status, 408 | 409 | 500..=599),
        Error::Connect(_)
        | Error::InvalidUrl(_)
        | Error::InsecureTransport(_)
        | Error::InsecureSocket { .. }
        | Error::InvalidConfig(_)
        | Error::HttpsUnsupported
        | Error::ReconnectExhausted(_) => false,
    }
}

/// Decide what the prompt call's result means.
pub fn prompt_outcome(result: &Result<PromptReply, Error>) -> PromptOutcome {
    match result {
        Ok(reply) => match reply.approval.as_ref() {
            Some(a) => PromptOutcome::Ended {
                events: reply_approval(a),
                check_status: false,
            },
            None if reply.stop_reason == "approval_required" => PromptOutcome::Ended {
                events: Vec::new(),
                check_status: true,
            },
            None => PromptOutcome::Ended {
                events: vec![TaskEvent::TurnEnded],
                check_status: false,
            },
        },
        Err(e) => PromptOutcome::Failed {
            server_busy: matches!(e, Error::Status { status: 409, .. }),
            check_status: may_have_been_received(e),
        },
    }
}
