//! The Tab stops of the Approvals page and of an approval card.

use std::collections::HashMap;

use super::super::adapter::*;
use tole_desktop_model::approvals::PageStop;

/// One focus handle per control that Tab visits. The model decides which
/// controls exist and in which order ([`tole_desktop_model::approvals::page_stops`]);
/// this only keeps a handle for each, so a control that is not on screen has
/// none and is never a stop.
#[derive(Default)]
pub struct StopFocus {
    handles: HashMap<PageStop, FocusHandle>,
}

impl StopFocus {
    /// Gives each of `stops` a handle, and drops those of controls that are gone.
    pub fn sync<V: 'static>(&mut self, stops: &[PageStop], cx: &mut Context<V>) {
        self.handles.retain(|stop, _| stops.contains(stop));
        for stop in stops {
            if !self.handles.contains_key(stop) {
                self.handles
                    .insert(stop.clone(), cx.focus_handle().tab_stop(true));
            }
        }
    }

    /// Forgets the handles of the controls of a card (the buttons and the
    /// technical details). A handle outlives a repaint, so when the card shown
    /// changes these must go before [`sync`](Self::sync): a new card's buttons
    /// start unfocused, and a key aimed at the old card's button can never act
    /// on the new card.
    pub fn forget_card(&mut self) {
        self.handles.retain(|stop, _| {
            !matches!(
                stop,
                PageStop::Details | PageStop::DontAllow | PageStop::AllowOnce
            )
        });
    }

    pub fn get(&self, stop: &PageStop) -> Option<&FocusHandle> {
        self.handles.get(stop)
    }

    /// The control that has keyboard focus in `window`, if any (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn focused(&self, stops: &[PageStop], window: &Window) -> Option<PageStop> {
        let on = |stop: &&PageStop| {
            self.handles
                .get(*stop)
                .is_some_and(|h| is_focused(h, window))
        };
        stops.iter().find(on).cloned()
    }
}
