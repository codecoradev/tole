//! Turning an automation on or off, and what the page says when serve says no.
//!
//! Pausing is always allowed (it only removes authority). Resuming is not:
//! serve refuses to resume a job an agent saved and the owner never reviewed
//! (403, message starting `not reviewed`), and a `--plan-mode` server pauses
//! but never resumes (409). Both are states the page shows in plain words with
//! the owner's action, not errors to retry. Nothing flips before the server
//! answers `200`.

use super::job::AutomationId;

/// What the owner must run in a terminal to review and turn on a job Tole made.
pub const OWNER_RESUME_COMMAND: &str = "tole cron resume";

/// The switch, as a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleIntent {
    Pause,
    Resume,
}

/// A request for the adapter to send (`POST /cron/jobs/{id}/pause` or
/// `/resume`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToggleRequest {
    pub id: AutomationId,
    pub intent: ToggleIntent,
}

/// What came back, as the adapter reports it (from the client's
/// `CronJobState`, or the error with its HTTP status and `error` message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToggleResult {
    /// `200`: the job's new `state` word (`paused` or `active`).
    Done { state: String },
    /// The server answered with an error.
    Refused { status: u16, message: String },
    /// No answer at all.
    Unreachable,
}

/// A message under the table after a switch was used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// An agent's job nobody reviewed: it can only be turned on by the owner
    /// from a terminal, with [`OWNER_RESUME_COMMAND`].
    NeedsOwnerReview {
        name: String,
    },
    /// The server runs in plan mode: it pauses automations, it does not
    /// resume them.
    ServerPlanMode,
    /// The automation no longer exists.
    Gone,
    NotReached,
    /// Anything else: the server's own words, made safe to draw.
    Failed(String),
}

impl Notice {
    /// The notice for a refused resume or pause. `reviewed_needed` says the
    /// row already awaited review.
    pub(crate) fn from_refusal(
        intent: ToggleIntent,
        status: u16,
        message: &str,
        name: &str,
        review_needed: bool,
    ) -> Notice {
        let resume = intent == ToggleIntent::Resume;
        match status {
            403 if resume && (review_needed || message.starts_with("not reviewed")) => {
                Notice::NeedsOwnerReview {
                    name: name.to_owned(),
                }
            }
            409 if resume => Notice::ServerPlanMode,
            404 => Notice::Gone,
            _ => Notice::Failed(crate::plain::printable(message, 200)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_resume_is_a_state_with_an_owner_action() {
        let review =
            Notice::from_refusal(ToggleIntent::Resume, 403, "not reviewed: x", "Job", false);
        assert_eq!(review, Notice::NeedsOwnerReview { name: "Job".into() });
        let known = Notice::from_refusal(ToggleIntent::Resume, 403, "forbidden", "Job", true);
        assert!(matches!(known, Notice::NeedsOwnerReview { .. }));
        assert_eq!(
            Notice::from_refusal(ToggleIntent::Resume, 409, "plan mode", "Job", false),
            Notice::ServerPlanMode
        );
        assert_eq!(
            Notice::from_refusal(ToggleIntent::Pause, 404, "no job", "Job", false),
            Notice::Gone
        );
    }

    /// A 403 that is not the review refusal, and a 409 on a pause, are
    /// failures with the server's words, not review or plan-mode states.
    #[test]
    fn other_refusals_keep_the_servers_words() {
        let n = Notice::from_refusal(ToggleIntent::Resume, 403, "path escape\n", "Job", false);
        assert_eq!(n, Notice::Failed("path escape".into()));
        let n = Notice::from_refusal(ToggleIntent::Pause, 409, "conflict", "Job", false);
        assert_eq!(n, Notice::Failed("conflict".into()));
        let n = Notice::from_refusal(ToggleIntent::Pause, 500, "boom", "Job", false);
        assert_eq!(n, Notice::Failed("boom".into()));
    }
}
