//! What the window shows besides the shell state, and what it asks of the app
//! behind it. No toolkit type: the rules are tested without a window.
//!
//! The shell state ([`ShellState`]) holds the connection dot and the sidebar
//! counts, and the app brings it up to date (`backend::shell::apply`). Two
//! things it cannot hold are here:
//!
//! - **The notice**: while the local serve is not ready (starting, restarting,
//!   stopped, failed) the window says so in words, with the button the failure
//!   calls for ([`ServeState::owner_action`]): "Try again" or "Restart Tole".
//!   A ready serve has no notice: the connection dot says it.
//! - **The skills count**: the shell's count of new skills and tools is a plain
//!   number and cannot say "not known", so while the app says it is not known
//!   the sidebar shows the unknown mark, as it does for approvals, never a zero.

use std::time::Duration;

use tole_desktop_model::tokens::Tone;
use tole_desktop_model::{strings, OwnerAction, ServeState, ShellState};

/// How often the window asks [`Source::changed`]. The backend tells at most
/// every 100 ms, so a change shows within about 150 ms.
pub const LOOK_EVERY: Duration = Duration::from_millis(50);

/// What the window reads from, and calls on, the app behind it. Everything is
/// called on the window's thread and must return at once.
pub trait Source {
    /// Something changed since the last look; clears the flag. Cheap.
    fn changed(&self) -> bool;

    /// Bring `shell` up to the app's state as of now and say what else the
    /// window shows.
    fn refresh(&self, shell: &mut ShellState) -> Status;

    /// The owner pressed the button of a failure. The work is the app's, off
    /// this thread; the result arrives as the next [`refresh`](Self::refresh).
    fn owner_action(&self, action: OwnerAction);
}

/// A source shared with its owner: the window holds one clone and the host
/// keeps another, to end the app after the window closed.
impl<T: Source + ?Sized> Source for std::rc::Rc<T> {
    fn changed(&self) -> bool {
        (**self).changed()
    }

    fn refresh(&self, shell: &mut ShellState) -> Status {
        (**self).refresh(shell)
    }

    fn owner_action(&self, action: OwnerAction) {
        (**self).owner_action(action);
    }
}

/// What the window shows besides the shell state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// What the local serve is doing.
    pub serve: ServeState,
    /// There is an app behind the window to press a button on. False for a
    /// window whose app could not start: its notice then has no button, which
    /// would do nothing.
    pub actionable: bool,
}

/// The button of a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    pub action: OwnerAction,
    pub label: &'static str,
}

/// The line the window shows while the serve is not ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    /// Colour meaning of the line's dot.
    pub tone: Tone,
    /// The button, when the owner can do something about it.
    pub offer: Option<Offer>,
}

impl Status {
    /// The status of a window whose app could not start at all: a failed
    /// serve with no button (there is nothing to press it on).
    pub fn failed_to_start(detail: String) -> Self {
        Status {
            serve: ServeState::Failed(tole_desktop_model::ServeFailure::StartFailed { detail }),
            actionable: false,
        }
    }

    /// The status of a window whose app's background link ended by itself.
    pub fn backend_ended() -> Self {
        Status::failed_to_start(strings::BACKEND_ENDED.to_owned())
    }

    /// The notice for the serve as it is now; none when it is ready.
    pub fn notice(&self) -> Option<Notice> {
        if self.serve.is_ready() {
            return None;
        }
        Some(Notice {
            text: strings::serve_state_label(&self.serve),
            tone: self.serve.connection_status().tone(),
            offer: self
                .serve
                .owner_action()
                .filter(|_| self.actionable)
                .map(|action| Offer {
                    action,
                    label: strings::owner_action_label(action),
                }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_desktop_model::ServeFailure;

    fn status(serve: ServeState) -> Status {
        Status {
            serve,
            actionable: true,
        }
    }

    fn running() -> ServeState {
        ServeState::Running {
            version: "1.2".into(),
        }
    }

    #[test]
    fn a_ready_serve_has_no_notice() {
        assert_eq!(status(running()).notice(), None);
        let attached = ServeState::Attached {
            version: "1.2".into(),
        };
        assert_eq!(status(attached).notice(), None);
    }

    #[test]
    fn a_serve_that_is_not_ready_says_so_in_the_models_words() {
        let restarting = ServeState::Restarting {
            attempt: 2,
            retry_in: Duration::from_secs(4),
        };
        for serve in [
            ServeState::Starting,
            restarting,
            ServeState::Stopping,
            ServeState::Stopped,
            ServeState::Failed(ServeFailure::BinaryNotFound),
        ] {
            let notice = status(serve.clone()).notice().expect("a notice");
            assert_eq!(notice.text, strings::serve_state_label(&serve));
            assert_eq!(notice.tone, serve.connection_status().tone());
        }
    }

    #[test]
    fn the_button_is_the_one_the_failure_calls_for_and_only_then() {
        let gave_up = ServeState::Failed(ServeFailure::GaveUp {
            attempts: 6,
            detail: "exit 1".into(),
        });
        let offer = status(gave_up).notice().unwrap().offer.expect("a button");
        assert_eq!(offer.action, OwnerAction::Retry);
        assert_eq!(offer.label, "Try again");

        let mismatch = ServeState::Failed(ServeFailure::VersionMismatch {
            found: Some(1),
            expected: 2,
        });
        let offer = status(mismatch).notice().unwrap().offer.expect("a button");
        assert_eq!(offer.action, OwnerAction::RestartServe);
        assert_eq!(offer.label, "Restart Tole");

        // Nothing for the owner to press: starting, or a missing binary.
        for serve in [
            ServeState::Starting,
            ServeState::Failed(ServeFailure::BinaryNotFound),
        ] {
            assert_eq!(status(serve).notice().unwrap().offer, None);
        }
    }

    #[test]
    fn an_app_that_could_not_start_is_a_failed_serve_the_owner_is_told_about() {
        let s = Status::failed_to_start("no threads".into());
        let notice = s.notice().expect("a notice");
        assert!(notice.text.contains("no threads"), "{}", notice.text);
        assert_eq!(notice.tone, Tone::Bad);
        assert_eq!(notice.offer, None, "there is no app to press it on");
    }
}
