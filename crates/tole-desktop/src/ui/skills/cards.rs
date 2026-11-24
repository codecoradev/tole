//! The pieces of a card: a tag, the card itself, its title row and the small
//! lines under it. Colours and sizes are the tokens'; the words and the choice
//! of what goes on a card come from the model and the string table.

use crate::ui::adapter::*;
use tole_desktop_model::tokens::{font, size, Tone, DARK};

/// A small label with a border in the colour of `tone`.
pub(super) fn tag(label: &str, tone: Tone) -> impl IntoElement {
    let colour = color(DARK.tone(tone));
    div()
        .flex_none()
        .h(len(size::TAG_HEIGHT))
        .px(len(size::GUTTER))
        .flex()
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(colour)
        .text_color(colour)
        .text_size(len(font::CAPTION))
        .child(label.to_owned())
}

/// A card of a list. `ask` draws it with the border of "needs the owner"
/// (amber), which is how a draft that was written after reading outside text,
/// or a tool that changed, stands out from the quiet rows around it.
pub(super) fn card(
    ask: bool,
    children: impl IntoIterator<Item = impl IntoElement>,
) -> impl IntoElement {
    let p = DARK;
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .p(len(size::CARD_PADDING))
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(color(if ask { p.amber } else { p.line }))
        .bg(color(p.panel))
        .children(children)
}

/// The first line of a card: the name, and a tag at the right.
pub(super) fn title_row(name: &str, tag: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(len(size::GUTTER))
        .child(
            div()
                .min_w(len(0.0))
                .truncate()
                .text_color(color(DARK.text_primary))
                .child(name.to_owned()),
        )
        .child(tag)
}

/// A group heading: a caption in the colour of `tone`.
pub(super) fn heading(text: &str, tone: Tone) -> impl IntoElement {
    div()
        .pt(len(size::GUTTER))
        .text_size(len(font::CAPTION))
        .text_color(color(DARK.tone(tone)))
        .child(text.to_owned())
}

/// A technical detail (a hash, a command): mono and faint.
pub(super) fn mono(text: &str) -> impl IntoElement {
    div()
        .min_w(len(0.0))
        .font_family(font::FAMILY_MONO)
        .text_size(len(font::CAPTION))
        .text_color(color(DARK.text_faint))
        .child(text.to_owned())
}

/// A line of small text in the colour of `tone`: a warning on a card, or the
/// note under it.
pub(super) fn small(text: &str, tone: Tone) -> impl IntoElement {
    div()
        .min_w(len(0.0))
        .text_size(len(font::CAPTION))
        .text_color(color(DARK.tone(tone)))
        .child(text.to_owned())
}
