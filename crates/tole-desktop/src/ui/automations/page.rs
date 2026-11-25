//! The Automations page: the title, the table, the notices and the note about
//! where automations run. A pure function of the driver's [`AutomationsView`]
//! and the page's own small state ([`AutomationsUi`]); the rows, the words and
//! the colours are the model's.

use super::super::adapter::*;
use super::history::history;
use super::notices::{dismiss_button, notice, stand_in};
use super::table::table;
use super::types::{standing, AutomationsHost, AutomationsUi};
use crate::driver::automations::AutomationsView;
use tole_desktop_model::strings::automations as words;
use tole_desktop_model::tokens::{font, size, DARK};

/// The page. `remote` says the serve is on another computer, which changes what
/// the note about the scheduler says.
pub fn automations_page<V: 'static>(
    view: &AutomationsView,
    ui: &AutomationsUi,
    host: AutomationsHost<V>,
    remote: bool,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let st = standing(view);
    let rows = &view.page.rows;
    let bar = div()
        .flex_none()
        .flex()
        .items_center()
        .justify_end()
        .px(len(size::PADDING))
        .h(len(size::TOPBAR_HEIGHT))
        .border_b_1()
        .border_color(color(p.line))
        .text_size(len(font::CAPTION))
        .text_color(color(p.text_muted))
        .children(st.top_line());
    let heading = div()
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .child(
            div()
                .text_size(len(font::TITLE))
                .text_color(color(p.text_primary))
                .child(words::TITLE),
        )
        .child(div().text_color(color(p.text_muted)).child(words::SUBTITLE));
    let body = match st.table_text(rows.len()) {
        Some(text) => stand_in(text, &view.page.load).into_any_element(),
        None => table(view, ui, host, cx).into_any_element(),
    };
    let mut notices: Vec<_> = st
        .notices(rows.len())
        .into_iter()
        .map(|text| notice(text, None).into_any_element())
        .collect();
    if let Some(page_notice) = &view.page.notice {
        let button = dismiss_button(ui, host, cx).into_any_element();
        notices.push(notice(words::notice_text(page_notice), Some(button)).into_any_element());
    }
    let scheduler = div()
        .text_size(len(font::CAPTION))
        .text_color(color(p.text_faint))
        .child(words::scheduler_note(&view.page.scheduler, remote));
    div().size_full().flex().flex_col().child(bar).child(
        div()
            .id("automations-body")
            .flex_1()
            .min_h(len(0.0))
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(len(size::PAGE_MAX_WIDTH))
                    .p(len(size::PADDING))
                    .flex()
                    .flex_col()
                    .gap(len(size::PADDING))
                    .child(heading)
                    .child(body)
                    .children(notices)
                    .children(history(view, ui, host, cx))
                    .child(scheduler),
            ),
    )
}
