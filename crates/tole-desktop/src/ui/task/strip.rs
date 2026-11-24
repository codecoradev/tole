//! The strip above the transcript (board S3): the status sentence with its
//! dot, the "Look only" chip and Stop. What the status
//! says, when Stop works and which colour the dot is come from the model
//! (`StatusRow`, `status_text`, `phase_tone`); this only draws them.

use super::TaskCall;
use crate::driver::task::TaskView;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::task::{phase_tone, Phase};
use tole_desktop_model::task_strings as words;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{Action, Section};

impl ShellView {
    pub(super) fn task_strip(&self, view: &TaskView, cx: &mut Context<Self>) -> AnyElement {
        let p = DARK;
        let status = &view.status;
        let waiting = status.phase == Phase::WaitingApproval;
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
            .child(sentence)
            .child(chip)
            .children(stop)
            .into_any_element()
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
