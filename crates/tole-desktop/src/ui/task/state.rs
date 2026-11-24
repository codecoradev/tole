//! What the window keeps of each open task besides what the driver says: where
//! its list is scrolled and whether it follows the end, which heights the driver
//! has.

use std::collections::HashMap;

use super::measure::{Drawn, Reported};
use crate::ui::adapter::ScrollHandle;
use tole_desktop_model::task::{Anchor, Follow, Tabs};

/// One open task as the window shows it.
pub(in crate::ui) struct Pane {
    pub scroll: ScrollHandle,
    pub follow: Follow,
    pub anchor: Anchor,
    pub reported: Reported,
    /// Which driver of the task the viewport and the heights above were given
    /// to (`TaskView::driver`): a new driver has neither.
    pub driver: Option<u64>,
    /// The rows the last frame built.
    pub drawn: Drawn,
    /// The viewport last given to the driver: how far scrolled and how tall.
    pub viewport: Option<(f32, f32)>,
}

impl Default for Pane {
    fn default() -> Self {
        Pane {
            scroll: ScrollHandle::new(),
            follow: Follow::default(),
            anchor: Anchor::default(),
            reported: Reported::default(),
            driver: None,
            drawn: Drawn::default(),
            viewport: None,
        }
    }
}

/// The open tasks of the window and which one fills the main area.
#[derive(Default)]
pub(in crate::ui) struct TaskPanes {
    pub tabs: Tabs,
    pub panes: HashMap<String, Pane>,
}
