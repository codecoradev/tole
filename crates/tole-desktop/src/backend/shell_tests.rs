//! `shell::apply`: what a backend view does to the shell state, state by state.

use std::time::Duration;

use tole_desktop_model::{
    Action, Badge, ConnectionStatus, RawJob, Scheduler, SchedulerHost, Section, ServeFailure,
    ServeState, ShellState, Target,
};

use super::shell::{actions, apply, automations_badge, skills_badge};
use super::BackendView;
use crate::driver::approvals::ApprovalsView;
use crate::driver::automations::AutomationsView;
use crate::driver::skills::SkillsPageView;

fn running() -> ServeState {
    ServeState::Running {
        version: "1".into(),
    }
}

/// A view of a serve that is `serve`, whose drivers know nothing.
fn view(serve: ServeState) -> BackendView {
    BackendView {
        serve,
        approvals: ApprovalsView::unknown(),
        skills: SkillsPageView::not_loaded(),
        automations: AutomationsView::not_loaded(),
        open_tasks: Vec::new(),
    }
}

/// A view whose drivers know these three counts.
fn known(approvals: usize, skills: usize, tools: usize) -> BackendView {
    let mut v = view(running());
    v.approvals.badge = Badge::Count(approvals);
    v.skills.skills_waiting = Badge::Count(skills);
    v.skills.tools_needing_attention = Badge::Count(tools);
    v
}

#[test]
fn an_unloaded_driver_reads_unknown_in_the_view_it_gives() {
    let v = view(running());
    assert_eq!(v.approvals.badge, Badge::Unknown);
    assert_eq!(v.skills.skills_waiting, Badge::Unknown);
    assert_eq!(v.skills.tools_needing_attention, Badge::Unknown);
    assert!(v.approvals.rows.is_empty() && v.approvals.selected.is_none());
}

#[test]
fn every_serve_state_maps_to_a_connection_status() {
    let failures = [
        ServeFailure::Unsupported,
        ServeFailure::BinaryNotFound,
        ServeFailure::DirectoryNotPrivate {
            path: "/p".into(),
            reason: "r".into(),
        },
        ServeFailure::VersionMismatch {
            found: None,
            expected: 2,
        },
        ServeFailure::CredentialUnavailable,
        ServeFailure::NotTole,
        ServeFailure::StartFailed { detail: "d".into() },
        ServeFailure::GaveUp {
            attempts: 5,
            detail: "d".into(),
        },
        ServeFailure::CouldNotStop,
    ];
    let mut cases = vec![
        (ServeState::Starting, ConnectionStatus::Connecting),
        (
            ServeState::Attached {
                version: "1".into(),
            },
            ConnectionStatus::Connected,
        ),
        (running(), ConnectionStatus::Connected),
        (
            ServeState::Restarting {
                attempt: 2,
                retry_in: Duration::from_secs(2),
            },
            ConnectionStatus::Connecting,
        ),
        (ServeState::Stopping, ConnectionStatus::Unreachable),
        (ServeState::Stopped, ConnectionStatus::Unreachable),
    ];
    cases.extend(
        failures
            .into_iter()
            .map(|f| (ServeState::Failed(f), ConnectionStatus::Unreachable)),
    );
    for (state, expected) in cases {
        let mut shell = ShellState::new();
        shell.apply(Action::SetConnectionStatus(ConnectionStatus::Preview));
        apply(&mut shell, &view(state.clone()));
        assert_eq!(shell.connection.status, expected, "{state:?}");
    }
}

#[test]
fn only_a_ready_serve_is_shown_as_connected() {
    let mut shell = ShellState::new();
    apply(&mut shell, &known(0, 0, 0));
    assert_eq!(shell.connection.status, ConnectionStatus::Connected);
    apply(&mut shell, &view(ServeState::Stopping));
    assert_eq!(shell.connection.status, ConnectionStatus::Unreachable);
}

#[test]
fn a_remote_target_is_never_shown_as_connected() {
    let mut shell = ShellState::new();
    let remote = tole_desktop_model::Connection {
        target: Target::Remote { name: "vps".into() },
        status: ConnectionStatus::Connecting,
    };
    shell.apply(Action::SetConnection(remote));
    apply(&mut shell, &known(0, 0, 0));
    assert_eq!(shell.connection.status, ConnectionStatus::Connecting);
}

#[test]
fn an_unknown_approvals_list_stays_unknown_and_a_count_is_copied() {
    let mut shell = ShellState::new();
    shell.apply(Action::SetApprovalsPending(Badge::Count(7)));
    apply(&mut shell, &view(running()));
    assert_eq!(shell.approvals_pending, Badge::Unknown, "never a zero");
    assert!(shell.badge_unknown(Section::Approvals));
    apply(&mut shell, &known(3, 0, 0));
    assert_eq!(shell.approvals_pending, Badge::Count(3));
    apply(&mut shell, &known(0, 0, 0));
    assert_eq!(shell.approvals_pending, Badge::Count(0), "a known zero");
    assert_eq!(shell.badge_text(Section::Approvals), None);
}

#[test]
fn the_skills_count_is_the_skills_and_the_tools_that_wait() {
    assert_eq!(skills_badge(&known(0, 2, 3)), Badge::Count(5));
    assert_eq!(skills_badge(&known(9, 0, 0)), Badge::Count(0));
    let mut shell = ShellState::new();
    let applied = apply(&mut shell, &known(0, 2, 3));
    assert_eq!(shell.skills_new, Badge::Count(5));
    assert!(applied.changed);
    assert_eq!(shell.badge(Section::SkillsAndTools), Some(5));
}

#[test]
fn an_unknown_skills_count_is_never_written_as_a_zero() {
    for (skills, tools) in [
        (Badge::Unknown, Badge::Count(2)),
        (Badge::Count(2), Badge::Unknown),
        (Badge::Unknown, Badge::Unknown),
    ] {
        let mut v = known(0, 0, 0);
        v.skills.skills_waiting = skills;
        v.skills.tools_needing_attention = tools;
        assert_eq!(skills_badge(&v), Badge::Unknown, "a part is a lower bound");
        assert!(
            actions(&v).contains(&Action::SetSkillsNew(Badge::Unknown)),
            "what is not known is written as unknown"
        );
        let mut shell = ShellState::new();
        shell.apply(Action::SetSkillsNew(Badge::Count(4)));
        apply(&mut shell, &v);
        assert_eq!(
            shell.skills_new,
            Badge::Unknown,
            "not the old count, not zero"
        );
        assert_eq!(
            shell.badge_text(Section::SkillsAndTools).as_deref(),
            Some("?")
        );
    }
}

#[test]
fn a_new_shell_with_nothing_known_reports_the_skills_count_unknown() {
    let mut shell = ShellState::new();
    apply(&mut shell, &view(ServeState::Starting));
    assert_eq!(shell.skills_new, Badge::Unknown);
    assert_eq!(shell.approvals_pending, Badge::Unknown);
}

#[test]
fn applying_the_same_view_twice_changes_nothing_the_second_time() {
    let mut shell = ShellState::new();
    let v = known(2, 1, 1);
    assert!(apply(&mut shell, &v).changed);
    let again = apply(&mut shell, &v);
    assert!(!again.changed);
}

#[test]
fn only_what_the_backend_knows_is_touched() {
    let mut shell = ShellState::placeholder();
    shell.apply(Action::SetDiscussionsNeedYou(2));
    let before = shell.clone();
    apply(&mut shell, &known(1, 1, 1));
    assert_eq!(shell.folders, before.folders);
    assert_eq!(shell.section, before.section);
    assert_eq!(shell.discussions_need_you, 2);
}

/// A job of the list as the serve gives it: `state` is `active` or `paused`.
fn job(id: &str, state: &str) -> RawJob {
    RawJob {
        id: id.into(),
        name: id.into(),
        schedule: "every 1h".into(),
        timezone: "UTC".into(),
        state: state.into(),
        next_run: Some(1_700_000_000),
        next: "soon".into(),
        last: None,
        missed: 0,
        blocked: 0,
        created_by: "cli".into(),
        awaiting_review: false,
        folder: None,
    }
}

/// A view whose automations list was read: these jobs.
fn with_jobs(jobs: Vec<RawJob>) -> BackendView {
    let mut v = view(running());
    let scheduler = Scheduler::Running(SchedulerHost::Serve);
    v.automations.page.loaded(jobs, scheduler);
    v
}

#[test]
fn an_unread_automations_list_stays_unknown_and_never_a_zero() {
    let mut shell = ShellState::new();
    shell.apply(Action::SetAutomationsOn(Badge::Count(5)));
    apply(&mut shell, &view(running()));
    assert_eq!(shell.automations_on, Badge::Unknown);
    assert!(shell.badge_unknown(Section::Automations));
}

#[test]
fn a_read_list_gives_the_number_of_automations_that_are_on() {
    let mut shell = ShellState::new();
    let v = with_jobs(vec![
        job("a", "active"),
        job("b", "active"),
        job("c", "paused"),
    ]);
    assert_eq!(automations_badge(&v), Badge::Count(2));
    apply(&mut shell, &v);
    assert_eq!(
        shell.badge_text(Section::Automations).as_deref(),
        Some("2 on")
    );
    // A known zero is a zero: no count and no mark.
    apply(&mut shell, &with_jobs(vec![job("c", "paused")]));
    assert_eq!(shell.automations_on, Badge::Count(0));
    assert_eq!(shell.badge_text(Section::Automations), None);
}

#[test]
fn a_list_that_is_not_all_of_the_jobs_or_not_current_is_unknown() {
    // Jobs past the cap: the rows are only some of them.
    let mut v = with_jobs(vec![job("a", "active")]);
    v.automations.left_out = 3;
    assert_eq!(automations_badge(&v), Badge::Unknown);
    // A read that failed keeps its rows but they may be out of date.
    let mut v = with_jobs(vec![job("a", "active")]);
    v.automations.page.load_failed("down");
    assert_eq!(automations_badge(&v), Badge::Unknown);
}

#[test]
fn a_counts_saturate_instead_of_wrapping() {
    let mut v = known(0, usize::MAX, 5);
    v.skills.tools_needing_attention = Badge::Count(5);
    assert_eq!(skills_badge(&v), Badge::Count(usize::MAX));
}
