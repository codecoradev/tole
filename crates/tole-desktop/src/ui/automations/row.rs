//! One row of the table: the name with its notes, when, folder, next run, the
//! last result, and the switch.

use super::super::adapter::*;
use super::cells::{cell, small, tone_ink};
use super::switch::switch;
use super::types::{AutomationsHost, AutomationsUi};
use tole_desktop_model::strings::automations as words;
use tole_desktop_model::tokens::{size, Tone, DARK};
use tole_desktop_model::{AutomationRow, AutomationsStop, NextRun, RunOutcome};

pub fn automation_row<V: 'static>(
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
