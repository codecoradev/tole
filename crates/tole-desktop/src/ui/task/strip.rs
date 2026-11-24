//! The strip above the transcript (board S3): the tabs of the open tasks, the
//! status sentence with its dot, the "Look only" chip and Stop. What the status
//! says, when Stop works and which colour the dot is come from the model
//! (`StatusRow`, `status_text`, `phase_tone`); this only draws them.

use super::TaskCall;
use crate::driver::task::TaskView;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::task::{phase_tone, Phase, Tabs};
use tole_desktop_model::task_strings as words;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{Action, Section};

impl ShellView {
    fn task_tab(
        &self,
        shown: &str,
        id: &str,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = DARK;
        let selected = shown == id;
        let (open, gone) = (id.to_owned(), id.to_owned());
        let label = title.unwrap_or_else(|| words::NEW_TASK_TAB.to_owned());
        let tab = div()
            .flex()
            .items_center()
            .h(len(size::ROW_HEIGHT))
            .max_w(len(size::TAB_MAX_WIDTH))
            .rounded(len(size::RADIUS))
            .child(
                div()
                    .id(("tab", self.tab_index(id)))
                    .role(Role::Tab)
                    .min_w(len(0.0))
                    .truncate()
                    .px(len(size::GUTTER))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.task_show(&open, window, cx);
                    }))
                    .child(label),
            )
            .child(
                div()
                    .id(("close-tab", self.tab_index(id)))
                    .role(Role::Button)
                    .flex_none()
                    .px(len(size::GUTTER))
                    .text_color(color(p.text_faint))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.task_close(&gone, cx);
                        // The tab that took its place is written in next.
                        if let Some(next) = this.tasks.tabs.shown().map(str::to_owned) {
                            this.focus_composer(&next, window, cx);
                        }
                    }))
                    .child(words::CLOSE_MARK),
            );
        if selected {
            tab.bg(color(p.raised))
                .text_color(color(p.text_primary))
                .into_any_element()
        } else {
            tab.text_color(color(p.text_muted)).into_any_element()
        }
    }

    /// The place of `id` among the open tasks (its element id).
    fn tab_index(&self, id: &str) -> usize {
        let open = self.source().map(|s| s.open_tasks()).unwrap_or_default();
        open.iter().position(|t| t == id).unwrap_or(open.len())
    }

    pub(super) fn task_strip(
        &self,
        shown: &str,
        view: &TaskView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = DARK;
        let open = self.source().map(|s| s.open_tasks()).unwrap_or_default();
        let tabs: Vec<_> = (open.iter())
            .map(|id| {
                let title = if id == shown {
                    view.title.clone()
                } else {
                    self.source()
                        .and_then(|s| s.task_view(id))
                        .and_then(|v| v.title)
                };
                self.task_tab(shown, id, title, cx)
            })
            .collect();
        let status = &view.status;
        let waiting = status.phase == Phase::WaitingApproval;
        let add = div()
            .id("new-tab")
            .role(Role::Button)
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .px(len(size::GUTTER))
            .text_color(color(p.text_muted))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.dispatch(Action::NewTask, cx)))
            .child(words::ADD_MARK);
        let sentence = div()
            .flex_1()
            .min_w(len(0.0))
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .child(
                div()
                    .size(len(size::DOT))
                    .flex_none()
                    .rounded_full()
                    .bg(color(p.tone(phase_tone(status)))),
            )
            .child(
                div()
                    .min_w(len(0.0))
                    .truncate()
                    .text_color(color(p.text_secondary))
                    .child(words::status_text(status)),
            )
            .children(waiting.then(|| {
                div()
                    .id("open-approvals")
                    .role(Role::Button)
                    .flex_none()
                    .text_color(color(p.accent))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dispatch(Action::SelectSection(Section::Approvals), cx);
                    }))
                    .child(words::OPEN_APPROVALS)
            }));
        let chip = div()
            .flex_none()
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .px(len(size::GUTTER))
            .rounded_full()
            .border_1()
            .border_color(color(p.line))
            .text_size(len(font::SMALL))
            .text_color(color(p.text_muted))
            .child(words::look_only_chip(status.look_only));
        let stop = status.can_stop.then(|| {
            div()
                .id("task-stop")
                .role(Role::Button)
                .flex_none()
                .h(len(size::ROW_HEIGHT))
                .flex()
                .items_center()
                .px(len(size::PADDING))
                .rounded(len(size::RADIUS))
                .border_1()
                .border_color(color(p.line))
                .text_color(color(p.text_primary))
                .cursor_pointer()
                .hover(|style| style.bg(color(p.raised)))
                .on_click(cx.listener(|this, _, _, cx| this.task_stop(cx)))
                .child(words::STOP_LABEL)
        });
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .px(len(size::PADDING))
            .py(len(size::GUTTER))
            .border_b_1()
            .border_color(color(p.line))
            .children(tabs)
            .child(add)
            .child(sentence)
            .child(chip)
            .children(stop)
            .into_any_element()
    }

    /// A tab was chosen: its task takes the area, and its composer the keyboard.
    pub(in crate::ui) fn task_show(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tasks.tabs.show(id);
        self.focus_composer(id, window, cx);
        cx.notify();
    }

    /// The tab's × was pressed: this window lets go of the task, and the
    /// neighbouring tab (or the home page) takes its place. Words typed in its
    /// composer and not sent go with it: closing a task lets go of everything
    /// the window keeps of it.
    pub(in crate::ui) fn task_close(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(source) = self.source() else {
            return;
        };
        let next = Tabs::after_close(&source.open_tasks(), id);
        source.close_task(id);
        self.tasks.panes.remove(id);
        if self.tasks.tabs.shown() == Some(id) {
            match next {
                Some(next) => self.tasks.tabs.show(&next),
                None => self.tasks.tabs.leave(),
            }
        }
        cx.notify();
    }

    /// Stop was pressed: the driver stops the running turn if the model allows.
    fn task_stop(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.tasks.tabs.shown().map(str::to_owned) else {
            return;
        };
        if let Some(source) = self.source() {
            let _ = source.task_call(&id, TaskCall::Stop);
        }
        cx.notify();
    }
}
