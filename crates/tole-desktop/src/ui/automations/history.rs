//! The history of the selected automation ("what happened"), under the table:
//! one line for each run with the colour of its result, what it printed, and
//! for a run that stopped to ask the link to the request in Approvals.

use super::super::adapter::*;
use super::cells::{small, tone_ink};
use super::notices::notice;
use super::types::{standing, AutomationsHost, AutomationsUi};
use crate::driver::automations::AutomationsView;
use tole_desktop_model::strings::automations as words;
use tole_desktop_model::tokens::{font, size, Tone, DARK};
use tole_desktop_model::{AutomationsStop, RunEntry};

/// The panel, or nothing while no automation is selected.
pub fn history<V: 'static>(
    view: &AutomationsView,
    ui: &AutomationsUi,
    host: AutomationsHost<V>,
    cx: &mut Context<V>,
) -> Option<impl IntoElement> {
    let p = DARK;
    let name = view.page.row(view.page.selected.as_ref()?)?.name.clone();
    let st = standing(view);
    let entries = &view.page.history;
    let runs: Vec<_> = entries
        .iter()
        .map(|entry| run(entry, ui, host, cx).into_any_element())
        .collect();
    let notes: Vec<_> = st
        .history_notices(entries)
        .into_iter()
        .map(|text| notice(text, None).into_any_element())
        .collect();
    Some(
        div()
            .flex()
            .flex_col()
            .gap(len(size::GUTTER))
            .child(
                div()
                    .text_size(len(font::CARD_TITLE))
                    .text_color(color(p.text_primary))
                    .child(words::history_title(&name)),
            )
            .children(
                st.history_text(entries.len())
                    .map(|text| div().text_color(color(p.text_faint)).child(text)),
            )
            .children(runs)
            .children(notes),
    )
}

/// One run: its line, what it printed, and the link when it has one.
fn run<V: 'static>(
    entry: &RunEntry,
    ui: &AutomationsUi,
    host: AutomationsHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let open = host.open_in_approvals;
    let line = div()
        .flex()
        .items_center()
        .gap(len(size::GUTTER))
        .child(
            div()
                .size(len(size::DOT))
                .flex_none()
                .rounded_full()
                .bg(tone_ink(entry.outcome.tone())),
        )
        .child(
            div()
                .text_color(color(p.text_secondary))
                .child(words::run_line(entry)),
        );
    let link = entry.approval_link().map(|approval| {
        let approval = approval.to_owned();
        div()
            .id(format!("automations-open-{}", entry.fire_id))
            .role(Role::Button)
            .flex_none()
            .px(len(size::GUTTER))
            .py(len(size::ROW_GAP))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .cursor_pointer()
            .text_color(color(p.accent))
            .when_some(
                ui.focus
                    .get(&AutomationsStop::OpenInApprovals(entry.fire_id.clone())),
                |el, handle| el.track_focus(handle),
            )
            .focus_visible(|style| style.border_color(color(p.accent)))
            .on_click(cx.listener(move |this, _, _, cx| open(this, &approval, cx)))
            .child(words::OPEN_IN_APPROVALS)
    });
    div()
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .child(line)
        .when(!entry.output.is_empty(), |el| {
            el.child(small(&entry.output, Tone::Neutral))
        })
        .when(entry.output_truncated, |el| {
            el.child(small(words::OUTPUT_CUT, Tone::Neutral))
        })
        .children(link)
}
