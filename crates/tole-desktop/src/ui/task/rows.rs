//! One row of the transcript: what the owner said, Tole's note and an answer.

use super::prose;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::task::Item;
use tole_desktop_model::task_strings as words;
use tole_desktop_model::tokens::{font, size, Rgb, DARK};

/// The frame every row sits in: the column of the transcript, centred, with
/// the space under the row (it is part of the row's measured height).
pub(super) fn shell(content: impl IntoElement) -> AnyElement {
    div()
        .w_full()
        .flex_none()
        .flex()
        .justify_center()
        .px(len(size::PADDING))
        .pb(len(size::TRANSCRIPT_GAP))
        .child(
            div()
                .w_full()
                .max_w(len(size::TRANSCRIPT_WIDTH))
                .min_w(len(0.0))
                .child(content),
        )
        .into_any_element()
}

/// A small line above or below a row.
fn caption(text: String, tone: Rgb) -> Div {
    div()
        .text_size(len(font::SMALL))
        .text_color(color(tone))
        .child(text)
}

/// The owner's words in a bubble at the right; a note to a running turn says so.
fn user(text: &str, images: u32, steer: bool) -> AnyElement {
    let p = DARK;
    div()
        .flex()
        .flex_col()
        .items_end()
        .gap(len(size::ROW_GAP))
        .children(steer.then(|| caption(words::STEER_LABEL.to_owned(), p.text_faint)))
        .child(
            div()
                .max_w(rel(0.85))
                .px(len(size::BUBBLE_PAD_X))
                .py(len(size::BUBBLE_PAD_Y))
                .rounded(len(size::BUBBLE_RADIUS))
                .border_1()
                .border_color(color(p.line))
                .bg(color(p.raised))
                .text_color(color(p.text_primary))
                .line_height(rel(font::LINE))
                .child(text.to_owned()),
        )
        .children((images > 0).then(|| caption(words::images_label(images), p.text_faint)))
        .into_any_element()
}

/// Tole's own note to the model: quiet, and not the owner's.
fn note(text: &str) -> AnyElement {
    let p = DARK;
    div()
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .child(caption(words::NOTE_LABEL.to_owned(), p.text_faint))
        .child(
            div()
                .text_size(len(font::SMALL))
                .text_color(color(p.text_muted))
                .child(text.to_owned()),
        )
        .into_any_element()
}

impl ShellView {
    /// One row of the task.
    pub(in crate::ui) fn task_row(&self, item: &Item) -> AnyElement {
        let content = match item {
            Item::User {
                text,
                images,
                steer,
            } => user(text, *images, *steer),
            Item::Note(text) => note(text),
            Item::Answer { markdown, cut, .. } => prose::answer(markdown, *cut),
            // Working groups and plans are drawn by the next commit.
            Item::Work(_) | Item::Plan(_) => div().into_any_element(),
        };
        shell(content)
    }
}
