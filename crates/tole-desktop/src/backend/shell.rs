//! The backend's view applied to the model's shell state: pure, no I/O.
//!
//! The window calls [`apply`] with every [`BackendView`] it reads (on each
//! `on_change`), so the sidebar counts and the connection dot follow the
//! serve. Three rules:
//!
//! - **Unknown is never a zero.** The approvals badge is copied as it is
//!   (`Unknown` stays `Unknown`), the skills count is `Unknown` unless
//!   both drivers' badges are known ([`skills_badge`]), and the automations
//!   count is `Unknown` unless the list was read, is current and is all of the
//!   jobs ([`automations_badge`]): the sidebar then shows the unknown mark.
//! - **The connection follows the serve**, every state of it
//!   ([`ServeState::connection_status`](tole_desktop_model::ServeState::connection_status)): starting and restarting read as
//!   connecting, attached and running as connected, stopping, stopped and
//!   every failure (a protocol version mismatch among them) as unreachable.
//!   A remote target is never shown as connected ([`ShellState::apply`]
//!   clamps it).
//! - **Only what the backend knows is touched**: not the folders or the
//!   discussions.

use tole_desktop_model::{Action, Badge, ShellState};

use super::BackendView;

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// The shell changed (redraw).
    pub changed: bool,
}

/// The sidebar count of skills and tools that need a decision: the skills and
/// drafts waiting plus the tools that changed or are not trusted yet, `Unknown`
/// when either is, because the part that is known is only a lower bound.
pub fn skills_badge(view: &BackendView) -> Badge {
    match (
        view.skills.skills_waiting,
        view.skills.tools_needing_attention,
    ) {
        (Badge::Count(skills), Badge::Count(tools)) => Badge::Count(skills.saturating_add(tools)),
        _ => Badge::Unknown,
    }
}

/// The sidebar count of automations that are on: the count of the list, only
/// when the list was read, is current and is all of the jobs (the driver's own
/// rule, [`AutomationsView::counts`](crate::driver::automations::AutomationsView::counts)),
/// `Unknown` otherwise: a count of the rows that are known is only a lower
/// bound, and a list that failed or is out of date may have changed.
pub fn automations_badge(view: &BackendView) -> Badge {
    match view.automations.counts() {
        Some((on, _paused)) => Badge::Count(on),
        None => Badge::Unknown,
    }
}

/// The actions that bring the shell up to `view`, in the order to apply them.
/// A count that is not known is written as `Unknown`, never as a zero.
pub fn actions(view: &BackendView) -> Vec<Action> {
    vec![
        Action::SetConnectionStatus(view.serve.connection_status()),
        Action::SetApprovalsPending(view.approvals.badge),
        Action::SetSkillsNew(skills_badge(view)),
        Action::SetAutomationsOn(automations_badge(view)),
    ]
}

/// Apply `view` to `shell`.
pub fn apply(shell: &mut ShellState, view: &BackendView) -> Applied {
    let mut changed = false;
    for action in actions(view) {
        changed |= shell.apply(action);
    }
    Applied { changed }
}
