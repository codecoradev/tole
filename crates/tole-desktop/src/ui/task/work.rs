//! The "Working · 3 steps" group: calls of tools, folded by default, each in
//! plain words, with the tool and the target one click further.

use super::rows::status_line;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::task::{Step, StepState, WorkGroup};
use tole_desktop_model::task_strings as words;
use tole_desktop_model::tokens::{font, size, DARK};

fn step(step: &Step, technical: bool) -> AnyElement {
    let p = DARK;
    let (dot, tone) = match step.state {
        StepState::Running => (p.accent, p.text_muted),
        StepState::Done => (p.ok, p.text_faint),
        StepState::Failed => (p.danger, p.danger),
        StepState::Unfinished => (p.text_faint, p.text_faint),
    };
    let line = status_line(
        dot,
        words::step_label(step),
        words::step_state(step.state),
        tone,
    );
    let detail = technical.then(|| {
        div()
            .pl(len(size::STEP_DOT + size::GUTTER))
            .font_family(font::FAMILY_MONO)
            .text_size(len(font::SMALL))
            .text_color(color(p.text_muted))
            .child(words::technical_line(step))
    });
    div()
        .flex()
        .flex_col()
        .child(line)
        .children(detail)
        .into_any_element()
}

impl ShellView {
    pub(super) fn work_group(
        &self,
        index: usize,
        group: &WorkGroup,
        active: bool,
        technical: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = DARK;
        let header = div()
            .id(("work", index))
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .text_size(len(font::SMALL))
            .text_color(color(p.text_muted))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.task_toggle(index, cx)))
            .child(if group.expanded {
                words::GROUP_OPEN
            } else {
                words::GROUP_FOLDED
            })
            .child(words::group_header(group.steps.len() as u32, active));
        let steps = group.expanded.then(|| {
            let toggle = if technical {
                words::TECH_HIDE
            } else {
                words::TECH_SHOW
            };
            div()
                .flex()
                .flex_col()
                .gap(len(size::ROW_GAP))
                .pl(len(size::INDENT))
                .pt(len(size::ROW_GAP))
                .children(group.steps.iter().map(|s| step(s, technical)))
                .child(
                    div()
                        .id(("technical", index))
                        .text_size(len(font::SMALL))
                        .text_color(color(p.accent))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.task_technical(index, cx)))
                        .child(toggle),
                )
        });
        div()
            .flex()
            .flex_col()
            .child(header)
            .children(steps)
            .into_any_element()
    }
}
