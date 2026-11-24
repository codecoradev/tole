//! The table of automations: a row of column titles, then one row for each
//! automation with its on/off switch. Colours and words are the model's.

use super::super::adapter::*;
use super::page::{AutomationsHost, AutomationsUi};
use crate::driver::automations::AutomationsView;
use tole_desktop_model::strings::automations as words;
use tole_desktop_model::tokens::{font, size, Tone, DARK};
use tole_desktop_model::{
    AutomationId, AutomationRow, AutomationsStop, NextRun, RunOutcome, Switch,
};

/// The column titles, then the rows.
pub(super) fn table<V: 'static>(
    view: &AutomationsView,
    ui: &AutomationsUi,
    host: AutomationsHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let rows: Vec<_> = view
        .page
        .rows
        .iter()
        .map(|row| {
            let selected = view.page.selected.as_ref() == Some(&row.id);
            let busy = view.toggling.as_ref() == Some(&row.id);
            automation_row(row, selected, busy, ui, host, cx).into_any_element()
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(color(p.line))
        .bg(color(p.panel))
        .child(titles())
        .children(rows)
}

/// A cell of fixed width (the name's takes what is left).
fn cell(width: Option<f32>) -> Div {
    let base = div()
        .min_w(len(0.0))
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP));
    match width {
        Some(w) => base.w(len(w)).flex_none(),
        None => base.flex_1(),
    }
}

fn titles() -> impl IntoElement {
    let [name, when, folder, next, last, on] = words::COLUMN_NAMES;
    let title = |width: Option<f32>, text: &'static str| {
        cell(width)
            .text_size(len(font::CAPTION))
            .text_color(color(DARK.text_faint))
            .child(text)
    };
    div()
        .flex()
        .items_center()
        .gap(len(size::GUTTER))
        .px(len(size::CARD_PADDING))
        .py(len(size::GUTTER))
        .child(title(None, name))
        .child(title(Some(size::COL_WHEN), when))
        .child(title(Some(size::COL_FOLDER), folder))
        .child(title(Some(size::COL_NEXT), next))
        .child(title(Some(size::COL_LAST), last))
        .child(title(Some(size::COL_ON), on))
}

fn tone_ink(tone: Tone) -> Rgba {
    color(DARK.tone(tone))
}

fn automation_row<V: 'static>(
    row: &AutomationRow,
    selected: bool,
    busy: bool,
    ui: &AutomationsUi,
    host: AutomationsHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let select = host.select;
    let id = row.id.clone();
    let paused = matches!(row.next, NextRun::Paused);
    let last_tone = row.last.as_ref().map_or(Tone::Neutral, RunOutcome::tone);
    let name = cell(None)
        .child(
            div()
                .truncate()
                .text_color(color(p.text_primary))
                .child(row.name.clone()),
        )
        .children(
            row.note()
                .map(|note| small(words::row_note(note), Tone::Attention)),
        )
        .children(words::row_warning(row).map(|text| small(&text, Tone::Attention)));
    let last = cell(Some(size::COL_LAST)).child(
        div()
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .child(
                div()
                    .size(len(size::DOT))
                    .flex_none()
                    .rounded_full()
                    .bg(tone_ink(last_tone)),
            )
            .child(
                div()
                    .truncate()
                    .child(words::outcome_label(row.last.as_ref())),
            ),
    );
    div()
        .id(format!("automations-row-{}", row.id.display()))
        .role(Role::Button)
        .w_full()
        .flex()
        .items_center()
        .gap(len(size::GUTTER))
        .px(len(size::CARD_PADDING))
        .py(len(size::GUTTER))
        .border_t_1()
        .border_color(color(p.line))
        .border_l_1()
        .border_r_1()
        .border_b_1()
        .cursor_pointer()
        .when_some(
            ui.focus.get(&AutomationsStop::Row(row.id.clone())),
            |el, handle| el.track_focus(handle),
        )
        .focus_visible(|style| style.border_color(color(p.accent)))
        .hover(|style| style.bg(color(p.raised)))
        .when(selected, |el| el.bg(color(p.raised)))
        .on_click(cx.listener({
            let id = id.clone();
            move |this, _, _, cx| select(this, &id, cx)
        }))
        .text_color(color(p.text_secondary))
        .child(name)
        .child(
            cell(Some(size::COL_WHEN))
                .child(div().child(words::schedule_line(&row.schedule, &row.timezone))),
        )
        .child(
            cell(Some(size::COL_FOLDER)).child(
                div().truncate().child(
                    row.folder
                        .clone()
                        .unwrap_or_else(|| words::NO_FOLDER.to_owned()),
                ),
            ),
        )
        .child(
            cell(Some(size::COL_NEXT)).child(
                div()
                    .text_color(color(if paused {
                        p.text_faint
                    } else {
                        p.text_secondary
                    }))
                    .child(words::next_label(&row.next)),
            ),
        )
        .child(last)
        .child(cell(Some(size::COL_ON)).child(switch(&row.id, row.switch, busy, ui, host, cx)))
}

/// Small text under the name, in the colour of its tone.
fn small(text: &str, tone: Tone) -> impl IntoElement {
    div()
        .min_w(len(0.0))
        .text_size(len(font::CAPTION))
        .text_color(tone_ink(tone))
        .child(text.to_owned())
}

/// The on/off switch, drawn from a track and a knob. It shows the state the
/// serve last reported; while its call is out it is dimmed, and it changes only
/// when the serve answers.
fn switch<V: 'static>(
    id: &AutomationId,
    state: Switch,
    busy: bool,
    ui: &AutomationsUi,
    host: AutomationsHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let toggle = host.toggle;
    let on = state == Switch::On;
    let target = id.clone();
    let track = div()
        .id(format!("automations-switch-{}", id.display()))
        .role(Role::Switch)
        .w(len(size::SWITCH_WIDTH))
        .h(len(size::SWITCH_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .px(len(size::ROW_GAP))
        .rounded_full()
        .border_1()
        .border_color(transparent())
        .bg(color(if on { p.accent } else { p.line }))
        .cursor_pointer()
        .when_some(
            ui.focus.get(&AutomationsStop::Switch(id.clone())),
            |el, handle| el.track_focus(handle),
        )
        .focus_visible(|style| style.border_color(color(p.text_primary)))
        .on_click(cx.listener(move |this, _, _, cx| toggle(this, &target, cx)))
        .child(
            div()
                .size(len(size::SWITCH_KNOB))
                .rounded_full()
                .bg(color(p.text_primary)),
        );
    let track = if on {
        track.justify_end()
    } else {
        track.justify_start()
    };
    track.when(busy, |el| el.opacity(0.5))
}
