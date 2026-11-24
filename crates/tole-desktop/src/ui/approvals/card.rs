//! The approval card: what Tole wants to do, in plain words, and the two
//! answers (boards S4 and S5).
//!
//! A reusable component: [`approval_card`] draws one [`ApprovalCard`] of the
//! model for any view `V` that hosts it, so the Approvals page and the task view
//! show the same card. It decides nothing: the words, the care level and
//! whether it can be answered are the model's. A card that cannot be answered
//! has no buttons and says why; the buttons that exist are the model's
//! [`BUTTONS`], in its order, and the press carries the id of THIS card.

use super::super::adapter::*;
use super::focus::StopFocus;
use super::parts::{button, labelled, outcome_colours, pill, Look};
use super::preview::preview_block;
use tole_desktop_model::approvals::{
    strings, ApprovalCard, Button, CardState, Care, PageStop, BUTTONS,
};
use tole_desktop_model::tokens::{font, size, tint, Rgb, DARK};

/// What the card calls on its host view. Plain function pointers, so a host
/// is a pair of methods and the card holds no state of its own.
pub struct CardHost<V: 'static> {
    /// The owner pressed an answer of the card with this id.
    pub answer: fn(&mut V, &str, tole_desktop_model::approvals::Decision, &mut Context<V>),
    /// The owner pressed "Show technical details" or "Hide technical details"
    /// on the card with this id.
    pub toggle_details: fn(&mut V, &str, &mut Context<V>),
}

impl<V: 'static> Clone for CardHost<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> Copy for CardHost<V> {}

/// The ink and the fill of the pill and the title mark for a care level.
fn care_colours(care: Care) -> (Rgb, Rgb) {
    match care {
        Care::Notice => (DARK.ok, tint::ADDED),
        Care::Caution => (DARK.amber, tint::WARNING),
        Care::Danger => (DARK.danger, tint::REMOVED),
    }
}

/// Draws `card`. `details_open` is the host's state of the technical details;
/// `focus` holds the handles of the card's Tab stops (see
/// [`tole_desktop_model::approvals::page_stops`]). The technical details sit
/// below the answers: opening them never pushes the buttons away, and the last
/// Tab stop is never an answer. Every element id belongs to the card, so a
/// press that began on one card cannot finish on another.
pub fn approval_card<V: 'static>(
    card: &ApprovalCard,
    details_open: bool,
    focus: &StopFocus,
    host: CardHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let (ink, fill) = match card.state {
        CardState::Decided(outcome) => outcome_colours(outcome),
        _ => care_colours(card.care),
    };
    let pill_text = match card.state {
        CardState::Decided(outcome) => strings::outcome_label(outcome),
        _ => card.effect_line,
    };
    let subject_font = if card.wording.is_file() {
        font::FAMILY_UI
    } else {
        font::FAMILY_MONO
    };
    // The effect is a sentence, sometimes a long one: it goes under the title
    // instead of squeezing it.
    let head = div()
        .flex()
        .flex_col()
        .items_start()
        .gap(len(size::ROW_GAP))
        .children(card.origin.clone().map(|origin| {
            div()
                .text_size(len(font::CAPTION))
                .text_color(color(p.text_muted))
                .child(origin)
        }))
        .child(
            div()
                .text_size(len(font::CARD_TITLE))
                .text_color(color(p.text_primary))
                .child(card.title.clone()),
        )
        .child(
            div()
                .font_family(subject_font)
                .text_color(color(p.text_muted))
                .child(card.subject.clone()),
        )
        .child(
            div()
                .mt(len(size::GUTTER))
                .child(pill(pill_text, ink, fill)),
        );
    let warning = card.warning.map(|text| {
        div()
            .w_full()
            .px(len(size::PADDING))
            .py(len(size::GUTTER))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(color(tint::WARNING_LINE))
            .bg(color(tint::WARNING))
            .text_color(color(p.amber))
            .child(text)
    });
    let preview = (!card.preview.lines.is_empty()).then(|| preview_block(&card.preview));
    let preview_note = card.preview_note.map(|note| {
        div()
            .text_size(len(font::CAPTION))
            .text_color(color(p.text_faint))
            .child(note)
    });
    let where_ink = if card.outside_folder {
        p.danger
    } else {
        p.text_primary
    };
    let mut rows = vec![labelled(
        strings::LABEL_WHERE,
        div().text_color(color(where_ink)).child(card.where_line),
    )
    .into_any_element()];
    if let Some(left) = card.waits_line.clone() {
        rows.push(labelled(strings::LABEL_WAITS, div().child(left)).into_any_element());
    }
    for told in &card.also_told {
        let value = div().child(told.clone());
        rows.push(labelled(strings::LABEL_ALSO_TOLD, value).into_any_element());
    }
    let toggle = details_toggle(&card.id, details_open, focus, host, cx);
    let details = details_open.then(|| {
        let rows: Vec<_> = card
            .details
            .iter()
            .map(|row| {
                let value = div()
                    .font_family(font::FAMILY_MONO)
                    .child(row.value.clone());
                labelled(row.label, value).into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(len(size::GUTTER))
            .children(rows)
    });
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(len(size::PADDING))
        .child(head)
        .children(warning)
        .children(preview)
        .children(preview_note)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(len(size::GUTTER))
                .children(rows),
        )
        .child(footer(card, focus, host, cx))
        .child(toggle)
        .children(details)
}

fn details_toggle<V: 'static>(
    card_id: &str,
    open: bool,
    focus: &StopFocus,
    host: CardHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let toggle = host.toggle_details;
    let id = card_id.to_owned();
    div()
        .id(format!("approval-details-{card_id}"))
        .role(Role::Button)
        .self_start()
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(transparent())
        .text_color(color(p.accent))
        .cursor_pointer()
        .when_some(focus.get(&PageStop::Details), |el, handle| {
            el.track_focus(handle)
        })
        .focus_visible(|style| style.border_color(color(p.accent)))
        .on_click(cx.listener(move |this, _, _, cx| toggle(this, &id, cx)))
        .child(if open {
            strings::HIDE_DETAILS
        } else {
            strings::SHOW_DETAILS
        })
}

/// The fine print and the answers; or, for a card that cannot be answered, why.
fn footer<V: 'static>(
    card: &ApprovalCard,
    focus: &StopFocus,
    host: CardHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let notes: Vec<_> = card
        .notes
        .iter()
        .map(|note| div().child(*note).into_any_element())
        .collect();
    let fine = div()
        .flex_1()
        .min_w(len(0.0))
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .text_size(len(font::CAPTION))
        .text_color(color(p.text_faint))
        .children(notes);
    let answer = host.answer;
    let right = if card.answerable() {
        let buttons: Vec<_> = BUTTONS
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let id = card.id.clone();
                let decision = b.decision();
                let look = match b {
                    Button::AllowOnce => Look::Primary,
                    Button::DontAllow => Look::Plain,
                };
                let press =
                    move |this: &mut V, cx: &mut Context<V>| answer(this, &id, decision, cx);
                let stop = PageStop::of_button(*b);
                button(
                    format!("approval-answer-{}-{i}", card.id),
                    strings::button_label(*b),
                    look,
                    stop,
                    focus,
                    press,
                    cx,
                )
                .into_any_element()
            })
            .collect();
        div()
            .flex_none()
            .flex()
            .gap(len(size::GUTTER))
            .children(buttons)
            .into_any_element()
    } else {
        let ink = match card.state {
            CardState::Answering(_) => p.text_muted,
            _ => p.amber,
        };
        div()
            .flex_none()
            .max_w(len(size::CONTENT_MAX_WIDTH / 2.0))
            .text_color(color(ink))
            .children(card.state_note)
            .into_any_element()
    };
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(len(size::PADDING))
        .child(fine)
        .child(right)
}
