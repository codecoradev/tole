//! The Automations page: the title, the table, the notices and the note about
//! where automations run. A pure function of the driver's [`AutomationsView`]
//! and the page's own small state ([`AutomationsUi`]); the rows, the words and
//! the colours are the model's.

use super::super::adapter::*;
use super::super::stops::Stops;
use super::table::table;
use crate::driver::automations::AutomationsView;
use tole_desktop_model::strings::automations as words;
use tole_desktop_model::tokens::{font, size, tint, DARK};
use tole_desktop_model::{
    automations_stops, AutomationId, AutomationsStanding, AutomationsStop, Load,
};

/// What the page calls on its host view.
pub struct AutomationsHost<V: 'static> {
    /// The owner picked a row (by its id).
    pub select: fn(&mut V, &AutomationId, &mut Context<V>),
    /// The owner used the switch of a row (by its id).
    pub toggle: fn(&mut V, &AutomationId, &mut Context<V>),
    /// The owner dismissed the notice under the table.
    pub dismiss_notice: fn(&mut V, &mut Context<V>),
}

impl<V: 'static> Clone for AutomationsHost<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> Copy for AutomationsHost<V> {}

/// The page's own state: a focus handle for each control Tab visits.
#[derive(Default)]
pub struct AutomationsUi {
    pub focus: Stops<AutomationsStop>,
}

impl AutomationsUi {
    /// The Tab stops of `view`, in the model's order.
    pub fn stops(view: &AutomationsView) -> Vec<AutomationsStop> {
        automations_stops(
            &view.page.rows,
            view.page.notice.is_some(),
            &view.page.history,
        )
    }

    /// Gives the controls on screen a handle each (call before drawing).
    pub fn sync<V: 'static>(&mut self, view: &AutomationsView, cx: &mut Context<V>) {
        self.focus.sync(&Self::stops(view), cx);
    }
}

/// What the page knows about its list, from the driver's view.
pub fn standing(view: &AutomationsView) -> AutomationsStanding {
    AutomationsStanding {
        load: view.page.load.clone(),
        left_out: view.left_out,
        counts: view.page.counts(),
        history: view.history.load.clone(),
        history_more_at_least: view.history.more_at_least,
        links_known: view.history.links_known,
    }
}

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
        let dismiss = host.dismiss_notice;
        let button = div()
            .id("automations-dismiss")
            .role(Role::Button)
            .flex_none()
            .px(len(size::GUTTER))
            .py(len(size::ROW_GAP))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .cursor_pointer()
            .when_some(ui.focus.get(&AutomationsStop::DismissNotice), |el, h| {
                el.track_focus(h)
            })
            .focus_visible(|style| style.border_color(color(p.accent)))
            .on_click(cx.listener(move |this, _, _, cx| dismiss(this, cx)))
            .child(words::DISMISS);
        notices.push(
            notice(
                words::notice_text(page_notice),
                Some(button.into_any_element()),
            )
            .into_any_element(),
        );
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
                    .child(scheduler),
            ),
    )
}

/// The line that stands where the table would be: checking, could not read, or
/// the empty list, with the server's words under a failure.
fn stand_in(text: &'static str, load: &Load) -> impl IntoElement {
    let p = DARK;
    let why = match load {
        Load::Failed(why) => Some(why.clone()),
        _ => None,
    };
    div()
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .py(len(size::PADDING))
        .child(div().text_color(color(p.text_faint)).child(text))
        .children(why.map(|why| {
            div()
                .text_size(len(font::CAPTION))
                .text_color(color(p.text_faint))
                .child(why)
        }))
}

/// A line of warning under the table, with a button when there is one.
fn notice(text: String, button: Option<AnyElement>) -> impl IntoElement {
    let p = DARK;
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(len(size::GUTTER))
        .px(len(size::GUTTER))
        .py(len(size::GUTTER))
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(color(tint::WARNING_LINE))
        .bg(color(tint::WARNING))
        .text_size(len(font::CAPTION))
        .text_color(color(p.amber))
        .child(div().min_w(len(0.0)).child(text))
        .children(button)
}
