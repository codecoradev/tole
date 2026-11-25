//! What the Automations page asks of its host, and the page's own small state.

use super::super::adapter::*;
use super::super::stops::Stops;
use crate::driver::automations::AutomationsView;
use tole_desktop_model::{automations_stops, AutomationId, AutomationsStanding, AutomationsStop};

/// What the page calls on its host view.
pub struct AutomationsHost<V: 'static> {
    /// The owner picked a row (by its id).
    pub select: fn(&mut V, &AutomationId, &mut Context<V>),
    /// The owner used the switch of a row (by its id).
    pub toggle: fn(&mut V, &AutomationId, &mut Context<V>),
    /// The owner dismissed the notice under the table.
    pub dismiss_notice: fn(&mut V, &mut Context<V>),
    /// The owner pressed "Open in Approvals" on a run that stopped to ask (by
    /// the id of the request).
    pub open_in_approvals: fn(&mut V, &str, &mut Context<V>),
}

impl<V: 'static> Clone for AutomationsHost<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> Copy for AutomationsHost<V> {}

/// The page's own state: a focus handle for each control Tab visits.
#[derive(Default)]
pub struct AutomationsUi {
    pub focus: Stops<AutomationsStop>,
}

impl AutomationsUi {
    /// The Tab stops of `view`, in the model's order.
    pub fn stops(view: &AutomationsView) -> Vec<AutomationsStop> {
        automations_stops(
            &view.page.rows,
            view.page.notice.is_some(),
            &view.page.history,
        )
    }

    /// Gives the controls on screen a handle each (call before drawing).
    pub fn sync<V: 'static>(&mut self, view: &AutomationsView, cx: &mut Context<V>) {
        self.focus.sync(&Self::stops(view), cx);
    }
}

/// What the page knows about its list, from the driver's view.
pub fn standing(view: &AutomationsView) -> AutomationsStanding {
    AutomationsStanding {
        load: view.page.load.clone(),
        left_out: view.left_out,
        counts: view.page.counts(),
        history: view.history.load.clone(),
        history_more_at_least: view.history.more_at_least,
        links_known: view.history.links_known,
    }
}
