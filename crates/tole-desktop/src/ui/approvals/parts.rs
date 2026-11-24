//! Small pieces the card and the page share: a pill, a button, a labelled row.

use super::super::adapter::*;
use super::focus::StopFocus;
use tole_desktop_model::approvals::{Outcome, PageStop};
use tole_desktop_model::tokens::{font, size, tint, Rgb, DARK};

/// The ink and the fill that say how a request ended: green for allowed, amber
/// for not allowed, grey when the time ran out or nobody knows.
pub fn outcome_colours(outcome: Outcome) -> (Rgb, Rgb) {
    match outcome {
        Outcome::Allowed => (DARK.ok, tint::ADDED),
        Outcome::NotAllowed => (DARK.amber, tint::WARNING),
        Outcome::Expired | Outcome::Closed => (DARK.text_muted, DARK.raised),
    }
}

/// A short status in a coloured pill.
pub fn pill(text: impl Into<String>, fg: Rgb, bg: Rgb) -> impl IntoElement {
    div()
        .flex_none()
        .px(len(size::GUTTER))
        .py(len(size::ROW_GAP))
        .rounded(len(size::RADIUS))
        .bg(color(bg))
        .text_color(color(fg))
        .text_size(len(font::CAPTION))
        .child(text.into())
}

/// A row of a card: a faint label and its value.
pub fn labelled(label: &'static str, value: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .gap(len(size::PADDING))
        .child(
            div()
                .w(len(size::LABEL_WIDTH))
                .flex_none()
                .text_color(color(DARK.text_muted))
                .child(label),
        )
        .child(div().flex_1().min_w(len(0.0)).child(value))
}

/// How a button looks: the amber one is the answer that lets Tole go on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Plain,
    Primary,
}

/// A button that is a Tab stop when `focus` has a handle for `stop`. `press`
/// runs on the host view. `id` names the element: it must belong to the card
/// (a press that started on one card must not finish on another).
pub fn button<V: 'static>(
    id: String,
    label: &'static str,
    look: Look,
    stop: PageStop,
    focus: &StopFocus,
    press: impl Fn(&mut V, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let base = div()
        .id(id)
        .role(Role::Button)
        .h(len(size::ROW_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .px(len(size::PADDING))
        .rounded(len(size::RADIUS))
        .border_1()
        .cursor_pointer()
        .when_some(focus.get(&stop), |el, handle| el.track_focus(handle))
        .focus_visible(|style| style.border_color(color(p.text_primary)))
        .on_click(cx.listener(move |this, _, _, cx| press(this, cx)))
        .child(label);
    match look {
        Look::Primary => base
            .bg(color(p.amber))
            .border_color(color(p.amber))
            .text_color(color(p.background)),
        Look::Plain => base
            .border_color(color(p.line))
            .text_color(color(p.text_primary))
            .hover(|style| style.bg(color(p.raised))),
    }
}
