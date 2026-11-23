//! A task with no serve behind it: the model's state read as a driver's view is.
//!
//! The render harness (`ui::harness_task`) and the window's own tests need a
//! [`TaskView`] they can shape: a history, a stream that is up, a viewport that
//! scrolls. A [`Preview`] holds the same store a driver does (the model's
//! `TaskState` and the facts of a live stream) and reads it through the same
//! `TaskView::of`, so what the window draws from it is what it would draw from a
//! driver. It starts no thread and calls nothing.

use std::sync::Arc;

use tole_desktop_model::task::TaskEvent;

use super::store::Store;
use super::view::TaskView;

/// One task read from a script of events instead of a serve.
pub struct Preview {
    store: Arc<Store>,
}

impl Preview {
    /// A task whose stream is up and whose history is read.
    pub fn new(look_only: bool) -> Self {
        let store = Arc::new(Store::new(look_only));
        {
            let mut shared = store.lock();
            shared.facts.connected = true;
            shared.facts.stream_up = true;
            shared.task.history_complete = true;
        }
        Preview { store }
    }

    /// Fold in events as a history page or the stream would deliver them.
    pub fn feed(&self, events: impl IntoIterator<Item = TaskEvent>) {
        let mut shared = self.store.lock();
        for event in events {
            shared.task.apply(event);
        }
    }

    /// The task as the window reads it now.
    pub fn view(&self) -> TaskView {
        self.store.view(true)
    }

    /// As [`Handle::set_viewport`](super::Handle::set_viewport).
    pub fn set_viewport(&self, scroll_top: f32, height: f32, overscan: f32) {
        if [scroll_top, height, overscan].iter().all(|v| v.is_finite()) {
            self.store.lock().viewport = super::store::Viewport {
                scroll_top,
                height,
                overscan,
            };
        }
    }

    /// As [`Handle::set_heights`](super::Handle::set_heights). Whether a height moved.
    pub fn set_heights(&self, heights: &[(usize, f32)]) -> bool {
        let mut shared = self.store.lock();
        let mut moved = false;
        for (row, height) in heights {
            moved |= shared.task.layout.set_height(*row, *height);
        }
        moved
    }

    /// As [`Handle::toggle_group`](super::Handle::toggle_group).
    pub fn toggle_group(&self, index: usize) {
        self.store.lock().task.toggle_group(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_desktop_model::task::{Item, SendMode, UserKind};

    fn said(seq: u64, text: &str) -> TaskEvent {
        TaskEvent::UserMessage {
            seq,
            text: text.into(),
            images: 0,
            kind: UserKind::Prompt,
        }
    }

    fn call(seq: u64, id: &str, tool: &str) -> [TaskEvent; 1] {
        [TaskEvent::ToolIntent {
            seq,
            id: id.into(),
            tool: tool.into(),
            target: Some("a.pdf".into()),
        }]
    }

    #[test]
    fn a_group_is_active_while_its_call_runs_and_the_tab_is_named_by_the_first_words() {
        let task = Preview::new(false);
        assert_eq!(task.view().title, None);
        task.feed([said(1, "Put my invoices into folders")]);
        task.feed(call(2, "i2", "read_file"));
        let running = task.view();
        assert_eq!(
            running.title.as_deref(),
            Some("Put my invoices into folders")
        );
        assert_eq!(running.rows.len(), 2);
        assert_eq!(
            running.active,
            [false, true],
            "the user row, then the group"
        );
        task.feed([TaskEvent::ToolSettled {
            seq: 3,
            intent_id: "i2".into(),
            ok: true,
        }]);
        assert_eq!(task.view().active, [false, false]);
    }

    #[test]
    fn the_window_follows_the_viewport_and_the_measured_heights() {
        let task = Preview::new(false);
        task.feed((1..=200).map(|n| said(n, "x")));
        task.set_viewport(0.0, 300.0, 0.0);
        let top = task.view();
        assert_eq!((top.total_rows, top.window.start), (200, 0));
        assert!(top.rows.len() < 10, "built {} rows", top.rows.len());
        // Row 0 turns out much taller: the rows after it start lower, so fewer are built.
        let before = top.window.end;
        assert!(task.set_heights(&[(0, 280.0)]));
        assert!(
            !task.set_heights(&[(0, 280.0)]),
            "the same height is no change"
        );
        assert!(task.view().window.end < before);
        task.set_viewport(f32::NAN, 300.0, 0.0);
        assert_eq!(
            task.view().window.start,
            0,
            "a viewport that is not a number is ignored"
        );
        task.set_viewport(9_000.0, 300.0, 0.0);
        let later = task.view();
        assert!(later.window.start > 100 && later.window.before > 0.0);
    }

    #[test]
    fn a_group_opens_and_closes_and_the_send_mode_follows_the_turn() {
        let task = Preview::new(false);
        task.feed(call(1, "i1", "read_file"));
        let opened = |t: &Preview| matches!(&t.view().rows[0], Item::Work(g) if g.expanded);
        assert!(!opened(&task));
        task.toggle_group(0);
        assert!(opened(&task));
        task.toggle_group(0);
        assert!(!opened(&task));
        // Its stream is up and its history read, and a durable entry starts no
        // turn: the view offers a prompt, as a driver's would.
        assert_eq!(task.view().status.send, SendMode::Send);
        task.feed([TaskEvent::Ready { busy: true }]);
        assert_eq!(task.view().status.send, SendMode::Steer);
    }

    #[test]
    fn the_view_counts_the_owners_rows_and_names_its_driver() {
        let task = Preview::new(false);
        assert_eq!(task.view().owner_rows, 0);
        task.feed([said(1, "first")]);
        task.feed(call(2, "i2", "read_file"));
        task.feed([said(3, "second")]);
        let view = task.view();
        assert_eq!(view.owner_rows, 2, "the work group is not the owner's");
        // The same driver reads the same, and another one is another.
        assert_eq!(task.view().driver, view.driver);
        assert_ne!(Preview::new(false).view().driver, view.driver);
        assert_ne!(view.driver, 0);
    }
}
