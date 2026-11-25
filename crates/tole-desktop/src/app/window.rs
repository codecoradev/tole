//! The app as the window's source: what the window reads, in the window's terms.

use tole_desktop_model::{
    Action, Badge, ConnectionStatus, OwnerAction, Section, ShellState, SkillsPane,
};

use super::{App, Boot};
use crate::backend::{shell, BackendView, TaskRefused};
use crate::driver::approvals::ApprovalsView;
use crate::driver::automations::AutomationsView;
use crate::driver::task::TaskView;
use crate::ui::approvals::{ApprovalsCall, ApprovalsRefused};
use crate::ui::automations::{AutomationsCall, AutomationsRefused};
use crate::ui::{Source, Status, TaskCall};
use tole_desktop_model::strings::StartProblem;

impl Boot {
    /// The shell the window starts from: the folder the app was started in is
    /// its first working folder, selected. Nothing else is claimed: the
    /// connection reads as connecting and the counts as unknown until the app
    /// says otherwise. A folder that is not a workspace (see
    /// [`Boot::refusal`]) is not shown as one: the shell has no folder.
    pub fn shell(&self) -> ShellState {
        let mut shell = ShellState::new();
        if self.refusal().is_some() {
            return shell;
        }
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
    shell::apply(state, &view);
    let skills_unknown = state.badge_unknown(Section::SkillsAndTools);
    if !running {
        state.apply(Action::SetConnectionStatus(ConnectionStatus::Unreachable));
        state.apply(Action::SetApprovalsPending(Badge::Unknown));
        state.apply(Action::SetSkillsNew(Badge::Unknown));
        state.apply(Action::SetAutomationsOn(Badge::Unknown));
        return Status::backend_ended();
    }
    Status {
        serve: view.serve,
        skills_unknown,
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

    fn open_tasks(&self) -> Vec<String> {
        self.tasks_open()
    }

    fn close_task(&self, session: &str) {
        self.tasks_close(session);
    }

    fn task_view(&self, session: &str) -> Option<TaskView> {
        self.tasks_view(session)
    }

    fn task_call(&self, session: &str, call: TaskCall) -> Result<(), TaskRefused> {
        self.tasks_call(session, call)
    }

    fn new_task(&self, folder: Option<&str>) {
        self.tasks_new(folder);
    }

    fn new_task_problem(&self) -> Option<StartProblem> {
        self.tasks_new_problem()
    }

    /// The skills driver's page: the same read as the view's, so the page and
    /// the sidebar count come from one place.
    fn skills_page(&self) -> Option<SkillsPane> {
        let skills = self.view().skills;
        Some(SkillsPane {
            page: skills.page,
            skills_loading: skills.skills_feed.loading,
            tools_loading: skills.tools_feed.loading,
        })
    }

    /// Both lists again, as soon as the driver's pace allows. Nothing when
    /// there is no driver (the serve is not ready): the page then says it was
    /// not read, and the next attach reads it.
    fn refresh_skills(&self) {
        let _ = self.backend.with_skills(|skills| skills.refresh());
    }

    fn approvals(&self) -> ApprovalsView {
        self.view().approvals
    }

    fn approvals_call(&self, call: ApprovalsCall) -> Result<(), ApprovalsRefused> {
        let sent = self.backend_approvals(|handle| match call {
            ApprovalsCall::Decide { id, decision } => handle.decide(&id, decision),
            ApprovalsCall::Select(id) => handle.select(id),
            ApprovalsCall::SelectTab(tab) => handle.select_tab(tab),
        });
        match sent {
            None => Err(ApprovalsRefused::NoServe),
            Some(Err(_)) => Err(ApprovalsRefused::Busy),
            Some(Ok(())) => Ok(()),
        }
    }

    fn automations(&self) -> AutomationsView {
        self.view().automations
    }

    fn automations_call(&self, call: AutomationsCall) -> Result<(), AutomationsRefused> {
        let sent = self.backend_automations(|handle| match &call {
            AutomationsCall::SetEnabled { id, enabled } => handle.set_enabled(id, *enabled),
            AutomationsCall::Select(id) => handle.select(id),
            AutomationsCall::DismissNotice => handle.dismiss_notice(),
        });
        match sent {
            None => Err(AutomationsRefused::NoServe),
            Some(Err(_)) => Err(AutomationsRefused::Busy),
            Some(Ok(())) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use std::sync::atomic::Ordering;

    use tole_desktop_model::{strings, Load, ServeState, SkillsPane, Tab, ToolTrust};

    use super::super::fake::{launcher_of, running, wait_until, Counts, FakeServe};
    use super::*;
    use crate::driver::SystemClock;

    fn boot(workspace: &str, home: &str) -> Boot {
        Boot::from_parts(
            workspace.into(),
            None,
            Some(home.into()),
            Path::new("/tmp"),
            7,
        )
    }

    #[test]
    fn the_folder_the_app_started_in_is_the_first_folder_and_is_selected() {
        let shell = boot("/home/me/Invoices", "/home/me").shell();
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
    fn a_folder_that_is_not_a_workspace_is_not_shown_as_one() {
        assert!(boot("/", "/home/me").shell().folders.is_empty());
        assert!(boot("/home/me", "/home/me").shell().folders.is_empty());
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
        assert_eq!(shell.skills_new, Badge::Unknown);
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

    /// The page the window draws is the driver's, read as it is: nothing before
    /// the serve is ready (not a zero), the rows once the routes answered.
    #[test]
    fn the_window_reads_the_skills_page_as_the_driver_keeps_it() {
        let counts = Counts {
            approvals: 0,
            drafts: 2,
            untrusted_tools: 1,
        };
        let serve = FakeServe::new(ServeState::Starting, Some(counts));
        let app = App::start_with(launcher_of(serve.clone(), None), Arc::new(SystemClock)).unwrap();
        // The serve is not ready: no driver, nothing read.
        assert_eq!(app.skills_page(), Some(SkillsPane::default()));
        serve.set(running());
        wait_until("both lists", || {
            app.skills_page().is_some_and(|v| {
                v.page.skills_load == Load::Ready && v.page.tools_load == Load::Ready
            })
        });
        let page = app.skills_page().expect("a page").page;
        assert_eq!(page.tab, Tab::Skills);
        assert_eq!(page.skills.review.len(), 2);
        assert_eq!(page.tools.len(), 1);
        assert_eq!(page.tools[0].trust, ToolTrust::NotTrusted);
        let _ = app.quit(Default::default());
    }

    /// Refresh reads the lists again; with no driver it does nothing and says
    /// nothing, and the page stays as it was.
    #[test]
    fn refresh_asks_the_serve_for_the_skills_again_and_is_harmless_without_a_driver() {
        let counts = Counts {
            approvals: 0,
            drafts: 1,
            untrusted_tools: 0,
        };
        let serve = FakeServe::new(ServeState::Starting, Some(counts));
        let app = App::start_with(launcher_of(serve.clone(), None), Arc::new(SystemClock)).unwrap();
        app.refresh_skills();
        assert_eq!(app.skills_page(), Some(SkillsPane::default()));
        assert_eq!(serve.skills_calls.load(Ordering::SeqCst), 0);
        serve.set(running());
        // The call counter goes up when the driver asks the serve, a moment
        // before the backend publishes the driver's handle. A refresh in that
        // moment finds no driver and does nothing, so wait for the page: it is
        // read through the published handle.
        wait_until("the first read", || {
            serve.skills_calls.load(Ordering::SeqCst) >= 1
                && app.skills_page().is_some_and(|v| {
                    v.page.skills_load == Load::Ready && v.page.tools_load == Load::Ready
                })
        });
        let first = serve.skills_calls.load(Ordering::SeqCst);
        app.refresh_skills();
        wait_until("a second read", || {
            serve.skills_calls.load(Ordering::SeqCst) > first
        });
        let _ = app.quit(Default::default());
    }
}
