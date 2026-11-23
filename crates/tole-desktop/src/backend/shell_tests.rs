//! `shell::apply`: what a backend view does to the shell state, state by state.

use std::time::Duration;

use tole_desktop_model::{
    Action, Badge, ConnectionStatus, Section, ServeFailure, ServeState, ShellState, Target,
};

use super::shell::{actions, apply, skills_badge};
use super::BackendView;
use crate::driver::approvals::ApprovalsView;
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
    assert_eq!(shell.skills_new, 5);
    assert!(applied.changed && !applied.skills_unknown);
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
            !actions(&v)
                .iter()
                .any(|a| matches!(a, Action::SetSkillsNew(_))),
            "no count is written"
        );
        let mut shell = ShellState::new();
        shell.apply(Action::SetSkillsNew(4));
        let applied = apply(&mut shell, &v);
        assert_eq!(shell.skills_new, 4, "left as it was, not zero");
        assert!(applied.skills_unknown, "and said");
    }
}

#[test]
fn a_new_shell_with_nothing_known_reports_the_skills_count_unknown() {
    // The shell's own count starts at 0 and cannot say "not known": this is the
    // gap `Applied::skills_unknown` reports.
    let mut shell = ShellState::new();
    let applied = apply(&mut shell, &view(ServeState::Starting));
    assert_eq!(shell.skills_new, 0);
    assert!(applied.skills_unknown);
    assert_eq!(shell.approvals_pending, Badge::Unknown);
}

#[test]
fn applying_the_same_view_twice_changes_nothing_the_second_time() {
    let mut shell = ShellState::new();
    let v = known(2, 1, 1);
    assert!(apply(&mut shell, &v).changed);
    let again = apply(&mut shell, &v);
    assert!(!again.changed);
    assert!(!again.skills_unknown);
}

#[test]
fn only_what_the_backend_knows_is_touched() {
    let mut shell = ShellState::placeholder();
    shell.apply(Action::SetAutomationsOn(3));
    shell.apply(Action::SetDiscussionsNeedYou(2));
    let before = shell.clone();
    apply(&mut shell, &known(1, 1, 1));
    assert_eq!(shell.folders, before.folders);
    assert_eq!(shell.section, before.section);
    assert_eq!(shell.automations_on, 3);
    assert_eq!(shell.discussions_need_you, 2);
}

#[test]
fn a_counts_saturate_instead_of_wrapping() {
    let mut v = known(0, usize::MAX, 5);
    v.skills.tools_needing_attention = Badge::Count(5);
    assert_eq!(skills_badge(&v), Badge::Count(usize::MAX));
}
