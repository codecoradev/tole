//! The small pieces the Skills and tools page is made of: a line, a caption,
//! a failure and the note of a partial read. Colours, sizes and words are the
//! tokens' and the string table's; nothing here decides what to show.

use crate::ui::adapter::*;
use tole_desktop_model::tokens::{font, size, DARK};

/// A line of text under the name: muted, wraps.
pub(super) fn line(text: &str) -> impl IntoElement {
    div()
        .min_w(len(0.0))
        .text_color(color(DARK.text_muted))
        .child(text.to_owned())
}

/// Small faint text, for details and hints.
pub(super) fn caption(text: &str) -> impl IntoElement {
    div()
        .min_w(len(0.0))
        .text_size(len(font::CAPTION))
        .text_color(color(DARK.text_faint))
        .child(text.to_owned())
}

/// What a tab says instead of rows: not read yet, reading.
pub(super) fn plain(text: &str) -> impl IntoElement {
    div()
        .py(len(size::PADDING))
        .text_color(color(DARK.text_faint))
        .child(text.to_owned())
}

/// The last load failed: say so in the colour of a refusal, with the server's
/// words (already cleaned by the model). No rows are drawn under it.
pub(super) fn failure(what: &str, why: &str) -> impl IntoElement {
    let p = DARK;
    div()
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .p(len(size::CARD_PADDING))
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(color(p.danger))
        .child(div().text_color(color(p.danger)).child(what.to_owned()))
        .child(line(why))
}

/// The server could read only part of the answer: the notes it gave, under a
/// warning that the list below may be missing something. Nothing when it read
/// everything.
pub(super) fn partial(warning: &str, notes: &[String]) -> Option<impl IntoElement> {
    if notes.is_empty() {
        return None;
    }
    let p = DARK;
    Some(
        div()
            .flex()
            .flex_col()
            .gap(len(size::ROW_GAP))
            .p(len(size::CARD_PADDING))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(color(p.amber))
            .child(div().text_color(color(p.amber)).child(warning.to_owned()))
            .children(notes.iter().map(|n| line(n))),
    )
}
