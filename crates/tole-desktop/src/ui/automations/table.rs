//! The table of automations: a row of column titles, then one row for each
//! automation with its on/off switch. Colours and words are the model's.

use super::super::adapter::*;
use super::cells::cell;
use super::row::automation_row;
use super::types::{AutomationsHost, AutomationsUi};
use crate::driver::automations::AutomationsView;
use tole_desktop_model::strings::automations as words;
use tole_desktop_model::tokens::{font, size, DARK};

/// The column titles, then the rows.
pub fn table<V: 'static>(
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

pub fn titles() -> impl IntoElement {
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
