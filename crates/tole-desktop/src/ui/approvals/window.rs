//! The Approvals page inside the window: the section's main area, and what the
//! page's presses do (as `impl ShellView`, like the home page and the sidebar).
//!
//! The page reads the app's view at every frame ([`Source::approvals`]) and
//! sends each press as an [`ApprovalsCall`] with the id of what was pressed. A
//! call the app refuses is said on the page: a press never looks as if it had
//! worked. The page repaints once a second while it is shown, so the countdowns
//! ("14 min left") stay true between the app's own changes.

use std::cell::RefCell;
use std::time::Duration;

use super::super::adapter::*;
use super::super::shell::ShellView;
use super::{approvals_page, ApprovalsCall, CardHost, PageHost, PageUi};
use crate::driver::approvals::ApprovalsView;
use tole_desktop_model::approvals::{Decision, Tab};
use tole_desktop_model::Section;

/// How often the page repaints for its countdowns.
const COUNTDOWN_EVERY: Duration = Duration::from_secs(1);

/// The window's state of the page.
#[derive(Default)]
pub struct ApprovalsUi {
    page: RefCell<PageUi>,
    /// The repeating repaint, started the first time the page is drawn.
    countdown: RefCell<Option<Every>>,
    /// The last call was refused (nothing was sent), and the card that was
    /// shown then (`Some(None)`: none was). The notice belongs to that card: it
    /// goes when another one is shown, and when the section is left.
    refused: RefCell<Option<Option<String>>>,
}

impl ApprovalsUi {
    /// Whether the notice of a refused call is still true for the card shown.
    fn refusal(&self, shown: Option<&str>) -> bool {
        let mut refused = self.refused.borrow_mut();
        if refused
            .as_ref()
            .is_some_and(|then| then.as_deref() != shown)
        {
            *refused = None;
        }
        refused.is_some()
    }

    /// The owner is no longer on the page: what it said about a refused press
    /// is not their concern any more.
    fn left(&self) {
        *self.refused.borrow_mut() = None;
    }
}

impl ShellView {
    /// The main area while Approvals is the section.
    pub(in crate::ui) fn approvals_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = match self.source() {
            Some(source) => source.approvals(),
            None => ApprovalsView::unknown(),
        };
        self.keep_counting_down(cx);
        let host = PageHost {
            card: CardHost {
                answer: ShellView::approvals_answer,
                toggle_details: ShellView::approvals_toggle,
            },
            select: ShellView::approvals_select,
            select_tab: ShellView::approvals_select_tab,
        };
        let mut ui = self.approvals.page.borrow_mut();
        ui.sync(&view, cx);
        let shown = view.selected.as_ref().map(|card| card.id.as_str());
        ui.set_refused(self.approvals.refusal(shown));
        approvals_page(&view, &ui, host, cx).into_any_element()
    }

    fn keep_counting_down(&self, cx: &mut Context<Self>) {
        let mut countdown = self.approvals.countdown.borrow_mut();
        if countdown.is_none() && self.source().is_some() {
            *countdown = Some(every(cx, COUNTDOWN_EVERY, |this: &mut ShellView, cx| {
                if this.state.section == Section::Approvals {
                    cx.notify();
                } else {
                    this.approvals.left();
                }
            }));
        }
    }

    /// Sends `call` to the app and keeps whether it did nothing.
    fn approvals_send(&mut self, call: ApprovalsCall, cx: &mut Context<Self>) {
        let sent = match self.source() {
            Some(source) => source.approvals_call(call),
            None => Err(super::ApprovalsRefused::NoServe),
        };
        let refused = sent.is_err().then(|| {
            let view = self.source().map(|source| source.approvals());
            view.and_then(|v| v.selected).map(|card| card.id)
        });
        *self.approvals.refused.borrow_mut() = refused;
        cx.notify();
    }

    fn approvals_answer(&mut self, id: &str, decision: Decision, cx: &mut Context<Self>) {
        let id = id.to_owned();
        self.approvals_send(ApprovalsCall::Decide { id, decision }, cx);
    }

    fn approvals_select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.approvals_send(ApprovalsCall::Select(Some(id.to_owned())), cx);
    }

    fn approvals_select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.approvals_send(ApprovalsCall::SelectTab(tab), cx);
    }

    fn approvals_toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        self.approvals.page.borrow_mut().toggle_details(id);
        cx.notify();
    }

    /// Whether the last press on the page reached nobody (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn approvals_refused(&self) -> bool {
        self.approvals.refused.borrow().is_some()
    }

    /// The window's own once-a-second look says the section was left (for the
    /// render harness, which has no timer).
    #[cfg(feature = "render-harness")]
    pub fn approvals_left(&self) {
        self.approvals.left();
    }

    /// The control of the page that has keyboard focus (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn approvals_focused(
        &self,
        window: &Window,
    ) -> Option<tole_desktop_model::approvals::PageStop> {
        let view = self.source()?.approvals();
        let stops = PageUi::stops(&view);
        self.approvals.page.borrow().focus.focused(&stops, window)
    }
}
