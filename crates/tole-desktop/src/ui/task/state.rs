//! What the window keeps of each open task besides what the driver says: where
//! its list is scrolled and whether it follows the end, which heights the driver
//! has, which rows show their technical details.

use std::collections::{HashMap, HashSet};

use super::measure::{Drawn, Reported};
use super::outbox::Outbox;
use crate::ui::adapter::{ScrollHandle, TextBox};
use tole_desktop_model::task::{Anchor, Follow, SendMode, Tabs};

/// The composer of one task: its text box and what the window keeps about a send.
pub(in crate::ui) struct Compose {
    pub text: TextBox,
    pub outbox: Outbox,
    /// The hint the box shows now.
    pub hint: &'static str,
    /// The last send was refused (the task is not connected): the words are
    /// kept, and the note stays while the same driver is in the same state
    /// (`TaskView::driver`, what Enter does).
    pub refused: Option<(u64, SendMode)>,
}

/// One open task as the window shows it.
pub(in crate::ui) struct Pane {
    pub scroll: ScrollHandle,
    pub follow: Follow,
    pub anchor: Anchor,
    /// Rows whose "Show technical details" is open.
    pub technical: HashSet<usize>,
    pub reported: Reported,
    /// Which driver of the task the viewport and the heights above were given
    /// to (`TaskView::driver`): a new driver has neither.
    pub driver: Option<u64>,
    /// The rows the last frame built.
    pub drawn: Drawn,
    /// The viewport last given to the driver: how far scrolled and how tall.
    pub viewport: Option<(f32, f32)>,
    /// Made when the composer is first drawn (it needs the window).
    pub compose: Option<Compose>,
}

impl Default for Pane {
    fn default() -> Self {
        Pane {
            scroll: ScrollHandle::new(),
            follow: Follow::default(),
            anchor: Anchor::default(),
            technical: HashSet::new(),
            reported: Reported::default(),
            driver: None,
            drawn: Drawn::default(),
            viewport: None,
            compose: None,
        }
    }
}

/// The open tasks of the window and which one fills the main area.
#[derive(Default)]
pub(in crate::ui) struct TaskPanes {
    pub tabs: Tabs,
    pub panes: HashMap<String, Pane>,
}
