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

#[cfg(unix)]
use super::approvals::{ApprovalsCall, ApprovalsRefused};
#[cfg(unix)]
use crate::driver::approvals::ApprovalsView;
use tole_desktop_model::strings::StartProblem;
use tole_desktop_model::tokens::Tone;
use tole_desktop_model::{strings, OwnerAction, Section, ServeState, ShellState, SkillsPane};

#[cfg(unix)]
use super::task::TaskCall;
#[cfg(unix)]
use crate::backend::TaskRefused;
#[cfg(unix)]
use crate::driver::task::TaskView;

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

    /// An open task as of now (`None`: not open, or no driver for it yet).
    /// Cheap and never blocks. A source with no tasks has none.
    #[cfg(unix)]
    fn task_view(&self, session: &str) -> Option<TaskView> {
        let _ = session;
        None
    }

    /// The tasks the window has open, in the order they were opened.
    fn open_tasks(&self) -> Vec<String> {
        Vec::new()
    }

    /// Stop following `session`: its tab closes. The task itself goes on
    /// where it runs; only this window lets go of it.
    fn close_task(&self, session: &str) {
        let _ = session;
    }

    /// "New task": make a session in `folder` (the serve's workspace when
    /// `None`) and open it as a task. Returns at once: the task shows in
    /// [`open_tasks`](Self::open_tasks) when the serve made it.
    fn new_task(&self, folder: Option<&str>) {
        let _ = folder;
    }

    /// Why the last [`new_task`](Self::new_task) did not start a task
    /// (refused, or the serve could not make it); `None` when it did, or none
    /// was tried.
    fn new_task_problem(&self) -> Option<StartProblem> {
        None
    }

    /// The last [`new_task`](Self::new_task) did not start a task.
    fn new_task_failed(&self) -> bool {
        self.new_task_problem().is_some()
    }

    /// What the owner did in an open task. Returns at once; the reason it did
    /// nothing, if it did not, is the answer.
    #[cfg(unix)]
    fn task_call(&self, session: &str, call: TaskCall) -> Result<(), TaskRefused> {
        let _ = (session, call);
        Err(TaskRefused::NotOpen)
    }

    /// The Skills and tools page as the app knows it now: the tabs, their
    /// loads and rows, and whether a call is out. `None` for a source with no
    /// such page (the window then shows it as not read yet).
    fn skills_page(&self) -> Option<SkillsPane> {
        None
    }

    /// The owner pressed Refresh on the Skills and tools page: read both lists
    /// again. The work is the app's, off this thread; the result arrives as the
    /// next [`skills_page`](Self::skills_page).
    fn refresh_skills(&self) {}

    /// The Approvals page as of now. Cheap and never blocks. A source with no
    /// approvals reads as the driver does when nothing is known: the list is
    /// not known, never empty.
    #[cfg(unix)]
    fn approvals(&self) -> ApprovalsView {
        ApprovalsView::unknown()
    }

    /// What the owner did on the Approvals page. Returns at once; the outcome
    /// is the next [`approvals`](Self::approvals). `Err` says nothing was sent.
    #[cfg(unix)]
    fn approvals_call(&self, call: ApprovalsCall) -> Result<(), ApprovalsRefused> {
        let _ = call;
        Err(ApprovalsRefused::NoServe)
    }
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

    fn open_tasks(&self) -> Vec<String> {
        (**self).open_tasks()
    }

    fn close_task(&self, session: &str) {
        (**self).close_task(session);
    }

    fn new_task(&self, folder: Option<&str>) {
        (**self).new_task(folder);
    }

    fn new_task_problem(&self) -> Option<StartProblem> {
        (**self).new_task_problem()
    }

    fn new_task_failed(&self) -> bool {
        (**self).new_task_failed()
    }

    #[cfg(unix)]
    fn task_view(&self, session: &str) -> Option<TaskView> {
        (**self).task_view(session)
    }

    #[cfg(unix)]
    fn task_call(&self, session: &str, call: TaskCall) -> Result<(), TaskRefused> {
        (**self).task_call(session, call)
    }

    fn skills_page(&self) -> Option<SkillsPane> {
        (**self).skills_page()
    }

    fn refresh_skills(&self) {
        (**self).refresh_skills();
    }

    #[cfg(unix)]
    fn approvals(&self) -> ApprovalsView {
        (**self).approvals()
    }

    #[cfg(unix)]
    fn approvals_call(&self, call: ApprovalsCall) -> Result<(), ApprovalsRefused> {
        (**self).approvals_call(call)
    }
}

/// What the window shows besides the shell state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// What the local serve is doing.
    pub serve: ServeState,
    /// The count of new skills and tools is not known now: the shell's
    /// [`ShellState::skills_new`] says `Unknown` too, and the sidebar shows
    /// the mark.
    pub skills_unknown: bool,
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
            skills_unknown: true,
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

/// What the sidebar draws beside `section`: the shell's own text, except the
/// skills row while its count is not known, which shows the unknown mark.
/// `status` is `None` for a window with no app behind it (a preview).
pub fn badge_text(status: Option<&Status>, shell: &ShellState, section: Section) -> Option<String> {
    let unknown = section == Section::SkillsAndTools && status.is_some_and(|s| s.skills_unknown);
    if unknown {
        return Some(strings::BADGE_UNKNOWN.to_owned());
    }
    shell.badge_text(section)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_desktop_model::{Action, Badge, ServeFailure};

    fn status(serve: ServeState) -> Status {
        Status {
            serve,
            skills_unknown: false,
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
        assert!(s.skills_unknown);
    }

    #[test]
    fn an_unknown_skills_count_shows_the_mark_and_never_a_zero() {
        let mut shell = ShellState::new();
        shell.apply(Action::SetApprovalsPending(Badge::Unknown));
        let unknown = Status {
            serve: running(),
            skills_unknown: true,
            actionable: true,
        };
        let mark = Some(strings::BADGE_UNKNOWN.to_owned());
        assert_eq!(
            badge_text(Some(&unknown), &shell, Section::SkillsAndTools),
            mark
        );
        // The shell's own count is Unknown too, and says the same.
        assert_eq!(shell.badge_text(Section::SkillsAndTools), mark);
        // Approvals and automations say their own unknown; no other row gains a
        // mark.
        assert_eq!(badge_text(Some(&unknown), &shell, Section::Approvals), mark);
        assert_eq!(
            badge_text(Some(&unknown), &shell, Section::Automations),
            mark
        );
        assert_eq!(
            badge_text(Some(&unknown), &shell, Section::Discussions),
            None
        );
    }

    #[test]
    fn a_known_count_and_a_preview_read_as_the_shell_says() {
        let mut shell = ShellState::new();
        shell.apply(Action::SetSkillsNew(Badge::Count(3)));
        let known = status(running());
        let three = shell.badge_text(Section::SkillsAndTools);
        assert!(three.is_some());
        assert_eq!(
            badge_text(Some(&known), &shell, Section::SkillsAndTools),
            three
        );
        assert_eq!(badge_text(None, &shell, Section::SkillsAndTools), three);
    }
}
