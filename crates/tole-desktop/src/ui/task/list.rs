//! The scrolling column of rows of the shown task.
//!
//! The driver builds the window: `TaskView::rows` and the padding before and
//! after them. This draws it, and each frame reports back what the toolkit laid
//! out (the heights of the rows it built, how far the list is scrolled) so the
//! next window fits. The model decides the rest: [`Follow`] keeps a running task
//! at its end until the owner scrolls up, [`Anchor`] keeps the text being read
//! where it is while heights above it are corrected, [`mark`] says when to show
//! "Thinking...".

use super::measure::{self, Drawn};
use super::rows::shell;
use super::state::Pane;
use super::TaskCall;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::task::mark;
use tole_desktop_model::task_strings as words;
use tole_desktop_model::tokens::{size, DARK};

impl ShellView {
    /// Put `session` in the main area.
    #[cfg(feature = "render-harness")]
    pub fn show_task(&mut self, session: &str) {
        self.tasks.tabs.show(session);
    }

    /// The main area of the shown task, if one is shown.
    pub(in crate::ui) fn task_pane(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.tasks.tabs.shown()?.to_owned();
        // The pane is taken out while the list is built: building reads the
        // window's source through `self`.
        let mut pane = self.tasks.panes.remove(&id).unwrap_or_default();
        let body = self.task_list(&id, &mut pane, cx);
        self.tasks.panes.insert(id, pane);
        Some(
            div()
                .flex_1()
                .min_w(len(0.0))
                .min_h(len(0.0))
                .flex()
                .flex_col()
                .child(body)
                .into_any_element(),
        )
    }

    /// Learn from the last frame, then draw the window the driver gives now.
    fn task_list(&self, id: &str, pane: &mut Pane, cx: &mut Context<Self>) -> AnyElement {
        let p = DARK;
        let source = self.source();
        let frame = laid_out(&pane.scroll);
        let mut view = source.and_then(|s| s.task_view(id));
        // After a restart of the serve the backend makes a new driver for the
        // task: its window starts at the top and it has no measured heights,
        // so what was given to the old one is given again.
        let driver = view.as_ref().map(|v| v.driver);
        if driver.is_some() && driver != pane.driver {
            pane.driver = driver;
            pane.viewport = None;
            pane.reported = Default::default();
        }
        let mut sent = false;
        let mut top = frame.top;
        if frame.viewport > 0.0 {
            let step = pane.follow.step(frame.top, frame.viewport, frame.content);
            let laid = measure::rows(pane.drawn, &frame.children, frame.top);
            let hold = !pane.follow.following() && !step.moved;
            let delta = pane.anchor.settle(&laid, hold);
            if let Some(end) = step.pin {
                top = end;
            } else if delta != 0.0 {
                top = (top + delta).max(0.0);
                pane.follow.shifted(delta);
            }
            if top != frame.top {
                scroll_to(&pane.scroll, top);
                cx.notify(); // one more frame to settle on the new position
            }
            if let Some(source) = source {
                let news = pane.reported.news(&laid);
                if !news.is_empty() {
                    sent = true;
                    let _ = source.task_call(id, TaskCall::Heights(news));
                }
                if pane.viewport != Some((top, frame.viewport)) {
                    pane.viewport = Some((top, frame.viewport));
                    sent = true;
                    let overscan = size::OVERSCAN;
                    let call = TaskCall::Viewport {
                        top,
                        height: frame.viewport,
                        overscan,
                    };
                    let _ = source.task_call(id, call);
                }
            }
        }
        if sent {
            // The window the driver builds for what was just given.
            view = source.and_then(|s| s.task_view(id));
        }
        let Some(view) = view else {
            return note_line(words::LOADING);
        };
        let window = view.window;
        pane.drawn = Drawn {
            start: window.start,
            count: view.rows.len(),
            before: window.before,
            after: window.after,
        };
        let rows: Vec<_> = view.rows.iter().map(|item| self.task_row(item)).collect();
        let last = view.rows.last().filter(|_| window.end == view.total_rows);
        let working = mark(&view.status, last).map(|m| {
            shell(
                div()
                    .text_color(color(p.text_faint))
                    .child(words::mark_label(m)),
            )
        });
        let empty = view.total_rows == 0 && working.is_none();
        if empty && view.loading {
            return note_line(words::LOADING);
        }
        if empty {
            return note_line(words::EMPTY_TASK);
        }
        let list = div()
            .id("transcript")
            .flex_1()
            .min_h(len(0.0))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&pane.scroll)
            .pt(len(size::PADDING))
            .child(div().h(len(window.before)).flex_none())
            .children(rows)
            .child(div().h(len(window.after)).flex_none())
            .children(working);
        div()
            .relative()
            .flex_1()
            .min_h(len(0.0))
            .flex()
            .flex_col()
            .child(list)
            .into_any_element()
    }
}

/// A line of words in the middle of the area (the task is opening, or empty).
fn note_line(text: &'static str) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p(len(size::PADDING))
        .text_color(color(DARK.text_muted))
        .child(div().max_w(len(size::CONTENT_MAX_WIDTH)).child(text))
        .into_any_element()
}

/// What the render harness does to a list that a person does with a wheel.
#[cfg(feature = "render-harness")]
impl ShellView {
    /// Scroll the shown task's list to `top`, as the owner's wheel would.
    pub fn scroll_task_to(&mut self, top: f32) {
        if let Some(pane) = self
            .tasks
            .tabs
            .shown()
            .and_then(|id| self.tasks.panes.get(id))
        {
            scroll_to(&pane.scroll, top);
        }
    }

    /// How the shown task's list stands: following the end, and how far it is scrolled.
    pub fn task_scroll(&self) -> Option<(bool, f32)> {
        let pane = self.tasks.panes.get(self.tasks.tabs.shown()?)?;
        Some((pane.follow.following(), laid_out(&pane.scroll).top))
    }
}
