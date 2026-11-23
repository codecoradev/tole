//! The app as the window's source: what the window reads, in the window's terms.

use tole_desktop_model::{Action, Badge, ConnectionStatus, OwnerAction, ShellState};

use super::{App, Boot};
use crate::backend::{shell, BackendView};
use crate::ui::{Source, Status};

impl Boot {
    /// The shell the window starts from: the folder the app was started in is
    /// its first working folder, selected. Nothing else is claimed: the
    /// connection reads as connecting and the counts as unknown until the app
    /// says otherwise.
    pub fn shell(&self) -> ShellState {
        let mut shell = ShellState::new();
        let name = self.workspace.file_name().map_or_else(
            || self.workspace.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        shell.apply(Action::AddFolder {
            name,
            path: self.workspace.display().to_string(),
        });
        shell
    }
}

/// `view` applied to `state`, for a backend that `running` says still runs. A
/// backend that ended (a worker panicked) is a failed state: its view reads
/// unknown, but the link may still say the serve is ready, and the window
/// must not keep showing a connected serve it no longer follows.
pub(super) fn refresh(state: &mut ShellState, view: BackendView, running: bool) -> Status {
    let applied = shell::apply(state, &view);
    if !running {
        state.apply(Action::SetConnectionStatus(ConnectionStatus::Unreachable));
        state.apply(Action::SetApprovalsPending(Badge::Unknown));
        return Status::backend_ended();
    }
    Status {
        serve: view.serve,
        skills_unknown: applied.skills_unknown,
        actionable: true,
    }
}

impl Source for App {
    fn changed(&self) -> bool {
        self.take_change()
    }

    /// The backend's view applied to the shell state (the connection dot and
    /// the counts; an unknown count is never written as zero), and the serve
    /// and the skills count's certainty for what the shell cannot say. When
    /// the backend has ended, a failed status (see the free function `refresh`): its worker
    /// raises the signal as it ends, so the window looks once more.
    fn refresh(&self, state: &mut ShellState) -> Status {
        refresh(state, self.view(), self.is_running())
    }

    fn owner_action(&self, action: OwnerAction) {
        App::owner_action(self, action);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use tole_desktop_model::{strings, ServeState};

    use super::super::fake::{launcher_of, running, wait_until, Counts, FakeServe};
    use super::*;
    use crate::driver::SystemClock;

    fn boot(workspace: &str) -> Boot {
        Boot::from_parts(workspace.into(), None, Path::new("/tmp"), 7)
    }

    #[test]
    fn the_folder_the_app_started_in_is_the_first_folder_and_is_selected() {
        let shell = boot("/home/me/Invoices").shell();
        let folder = shell.selected().expect("selected");
        assert_eq!(folder.name, "Invoices");
        assert_eq!(folder.path, "/home/me/Invoices");
        assert!(!folder.sample, "a real folder, not sample data");
        assert_eq!(shell.folders.len(), 1);
        assert!(!shell.sample_data());
        // Nothing is claimed before the app speaks.
        assert_eq!(shell.connection.status, ConnectionStatus::Connecting);
        assert_eq!(shell.approvals_pending, Badge::Unknown);
    }

    #[test]
    fn a_backend_that_ended_is_a_failed_state_whatever_the_serve_says() {
        let serve = FakeServe::new(
            running(),
            Some(Counts {
                approvals: 2,
                drafts: 0,
                untrusted_tools: 0,
            }),
        );
        let app = App::start_with(launcher_of(serve, None), Arc::new(SystemClock)).unwrap();
        wait_until("the counts", || {
            app.view().approvals.badge == Badge::Count(2)
        });
        let view = app.view();
        let mut shell = ShellState::new();

        // Still running: connected, with its counts.
        let status = refresh(&mut shell, view.clone(), true);
        assert_eq!(status.serve, running());
        assert_eq!(shell.connection.status, ConnectionStatus::Connected);
        assert_eq!(shell.approvals_pending, Badge::Count(2));

        // Ended: the same view, which still says the serve is ready, shows as
        // a failure the owner can read, not as a live connection.
        let status = refresh(&mut shell, view, false);
        assert_eq!(shell.connection.status, ConnectionStatus::Unreachable);
        assert!(matches!(status.serve, ServeState::Failed(_)));
        assert!(status.skills_unknown);
        assert_eq!(shell.approvals_pending, Badge::Unknown);
        assert!(!status.actionable, "there is nothing to press it on");
        let notice = status.notice().expect("a notice");
        assert_eq!(notice.offer, None);
        assert!(
            notice.text.contains(strings::BACKEND_ENDED),
            "{}",
            notice.text
        );
        let _ = app.quit(Default::default());
    }
}
