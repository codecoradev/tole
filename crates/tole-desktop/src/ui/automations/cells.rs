//! The small pieces the rows and the page share: a cell of the table, the
//! small text under a name, and the ink of a tone.

use super::super::adapter::*;
use tole_desktop_model::tokens::{font, size, Tone, DARK};

/// A cell of fixed width (the name's takes what is left, and never less than
/// [`size::COL_NAME_MIN`]: a name column squeezed to nothing wraps its text one
/// letter to a line).
pub fn cell(width: Option<f32>) -> Div {
    let base = div()
        .min_w(len(0.0))
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP));
    match width {
        Some(w) => base.w(len(w)).flex_none(),
        None => base.flex_1().min_w(len(size::COL_NAME_MIN)),
    }
}

pub fn tone_ink(tone: Tone) -> Rgba {
    color(DARK.tone(tone))
}

/// Small text under the name, in the colour of its tone.
pub fn small(text: &str, tone: Tone) -> impl IntoElement {
    div()
        .min_w(len(0.0))
        .text_size(len(font::CAPTION))
        .text_color(tone_ink(tone))
        .child(text.to_owned())
}
