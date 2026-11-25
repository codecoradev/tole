//! The Automations page inside the window: the section's main area, and what
//! the page's presses do (as `impl ShellView`, like the Approvals page).
//!
//! The page reads the app's view at every frame ([`Source::automations`]) and
//! sends each press as an [`AutomationsCall`] with the exact id of the row that
//! was pressed. A press the app takes changes nothing by itself: the switch
//! moves only when the serve answers, and that arrives as the next view.

use std::cell::RefCell;

use super::super::adapter::*;
use super::super::shell::ShellView;
use super::page::automations_page;
use super::types::{AutomationsHost, AutomationsUi};
use super::AutomationsCall;
use crate::driver::automations::AutomationsView;
use tole_desktop_model::{AutomationId, Switch, Target};

/// The window's state of the page: a focus handle for each control Tab visits.
#[derive(Default)]
pub struct AutomationsWindow {
    page: RefCell<AutomationsUi>,
}

impl ShellView {
    /// The main area while Automations is the section.
    pub(in crate::ui) fn automations_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = match self.source() {
            Some(source) => source.automations(),
            None => AutomationsView::not_loaded(),
        };
        let remote = !matches!(self.state.connection.target, Target::ThisComputer);
        let host = AutomationsHost {
            select: ShellView::automations_select,
            toggle: ShellView::automations_toggle,
            dismiss_notice: ShellView::automations_dismiss,
        };
        let mut ui = self.automations.page.borrow_mut();
        ui.sync(&view, cx);
        automations_page(&view, &ui, host, remote, cx).into_any_element()
    }

    /// Sends `call` to the app. A refused call sent nothing, and nothing on the
    /// page claims it did: the switch and the selection stay as they were.
    fn automations_send(&mut self, call: AutomationsCall, cx: &mut Context<Self>) {
        if let Some(source) = self.source() {
            let _ = source.automations_call(call);
        }
        cx.notify();
    }

    fn automations_select(&mut self, id: &AutomationId, cx: &mut Context<Self>) {
        self.automations_send(AutomationsCall::Select(id.as_str().to_owned()), cx);
    }

    /// The switch asks for the other state than the one the serve last reported.
    fn automations_toggle(&mut self, id: &AutomationId, cx: &mut Context<Self>) {
        let on = self
            .source()
            .and_then(|source| {
                let view = source.automations();
                view.page.row(id).map(|row| row.switch == Switch::On)
            })
            .unwrap_or(true);
        let call = AutomationsCall::SetEnabled {
            id: id.as_str().to_owned(),
            enabled: !on,
        };
        self.automations_send(call, cx);
    }

    fn automations_dismiss(&mut self, cx: &mut Context<Self>) {
        self.automations_send(AutomationsCall::DismissNotice, cx);
    }
}
