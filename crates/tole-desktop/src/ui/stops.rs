//! One focus handle per control that Tab visits, for a page whose model decides
//! which controls exist and in which order.

use std::collections::HashMap;
use std::hash::Hash;

use super::adapter::*;

/// The handles of the controls on screen. The model gives the stops in order
/// (for the Automations page, [`tole_desktop_model::automations_stops`]); this
/// only keeps a handle for each, so a control that is not on screen has none
/// and is never a stop.
pub struct Stops<K> {
    handles: HashMap<K, FocusHandle>,
}

impl<K> Default for Stops<K> {
    fn default() -> Self {
        Stops {
            handles: HashMap::new(),
        }
    }
}

impl<K: Hash + Eq + Clone> Stops<K> {
    /// Gives each of `stops` a handle, and drops those of controls that are gone.
    pub fn sync<V: 'static>(&mut self, stops: &[K], cx: &mut Context<V>) {
        self.handles.retain(|stop, _| stops.contains(stop));
        for stop in stops {
            if !self.handles.contains_key(stop) {
                self.handles
                    .insert(stop.clone(), cx.focus_handle().tab_stop(true));
            }
        }
    }

    pub fn get(&self, stop: &K) -> Option<&FocusHandle> {
        self.handles.get(stop)
    }

    /// The control that has keyboard focus in `window`, if any (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn focused(&self, stops: &[K], window: &Window) -> Option<K> {
        let on = |stop: &&K| {
            self.handles
                .get(*stop)
                .is_some_and(|h| is_focused(h, window))
        };
        stops.iter().find(on).cloned()
    }
}
