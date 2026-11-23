//! The answer to a pause or a resume, as the model's [`ToggleResult`].

use tole_client::{CronJobState, Error};
use tole_desktop_model::{AutomationId, ToggleResult};

use super::text::{bounded, TEXT_MAX};

/// The result of `Client::pause_cron_job` or `Client::resume_cron_job` of the
/// job `id`, for `AutomationsState::finish_toggle`.
///
/// - `200` with the job's own id: `Done` with the state word the server gave
///   (the model turns the switch on only for `active`).
/// - `200` that names another job, or none: the answer is not about this
///   request, so it is `Unreachable` ("could not reach Tole"): the page says
///   nothing was confirmed and the driver reads the list again. Never `Done`:
///   it would flip a switch on a stranger's word.
/// - An HTTP error status: `Refused` with the status and the server's words
///   (`403` "not reviewed" and `409` plan mode have their own notices in the
///   model). A refusal with no words says its status.
/// - Anything else (no connection, a reset, a timeout, an undecodable body):
///   `Unreachable`. The request may have reached the server; nothing here says
///   it did not.
pub fn toggle_result(id: &AutomationId, result: &Result<CronJobState, Error>) -> ToggleResult {
    match result {
        Ok(reply) if reply.id == id.as_str() => ToggleResult::Done {
            state: bounded(&reply.state, TEXT_MAX),
        },
        Ok(_) => ToggleResult::Unreachable,
        Err(Error::Status {
            status, message, ..
        }) => ToggleResult::Refused {
            status: *status,
            message: if message.trim().is_empty() {
                format!("server answered {status}")
            } else {
                bounded(message, TEXT_MAX)
            },
        },
        Err(_) => ToggleResult::Unreachable,
    }
}
