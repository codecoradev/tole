//! The Approvals page: the list of what waits or was decided, and the card of
//! the picked request beside it (board S5).
//!
//! A pure function of the driver's [`ApprovalsView`] and the page's own small
//! state ([`PageUi`]); the rows, the words and the care levels are the model's.
//! An unknown list never reads as an empty one (see [`Standing`]), and a press
//! goes to the host as the id of the row or the card that was pressed.

use super::super::adapter::*;
use super::card::{approval_card, CardHost};
use super::focus::StopFocus;
use super::parts::{outcome_colours, pill};
use crate::driver::approvals::ApprovalsView;
use tole_desktop_model::approvals::{page_stops, strings, Care, PageStop, Row, Standing, Tab};
use tole_desktop_model::tokens::{font, size, tint, Rgb, DARK};

/// What the page calls on its host view.
pub struct PageHost<V: 'static> {
    pub card: CardHost<V>,
    /// The owner picked a request of the list (by its id).
    pub select: fn(&mut V, &str, &mut Context<V>),
    /// The owner picked a tab.
    pub select_tab: fn(&mut V, Tab, &mut Context<V>),
}

impl<V: 'static> Clone for PageHost<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> Copy for PageHost<V> {}

/// The page's own state: the Tab stops, and which card has its technical
/// details open. Everything else is in the driver's view.
#[derive(Default)]
pub struct PageUi {
    pub focus: StopFocus,
    details_for: Option<String>,
    /// The card the handles of the card's controls were made for.
    shown: Option<String>,
}

impl PageUi {
    /// The Tab stops of `view`, in the model's order.
    pub fn stops(view: &ApprovalsView) -> Vec<PageStop> {
        page_stops(&view.rows, view.selected.as_ref())
    }

    /// Gives the controls on screen a handle each (call before drawing).
    ///
    /// When the card shown is another one, the handles of the card's controls
    /// are dropped first, so a new card's buttons start unfocused: a handle
    /// that outlived its card would put the keyboard on a button of a request
    /// the owner never read.
    pub fn sync<V: 'static>(&mut self, view: &ApprovalsView, cx: &mut Context<V>) {
        let shown = view.selected.as_ref().map(|card| card.id.clone());
        if shown != self.shown {
            self.focus.forget_card();
            self.shown = shown;
        }
        self.focus.sync(&Self::stops(view), cx);
    }

    /// Whether the card shown has its technical details open: they open for
    /// one card, and a different card starts closed.
    pub fn details_open(&self, view: &ApprovalsView) -> bool {
        let shown = view.selected.as_ref().map(|c| c.id.as_str());
        shown.is_some() && shown == self.details_for.as_deref()
    }

    pub fn toggle_details(&mut self, id: &str) {
        let open = self.details_for.as_deref() == Some(id);
        self.details_for = (!open).then(|| id.to_owned());
    }
}

/// What the page knows about its list, from the driver's view.
pub fn standing(view: &ApprovalsView) -> Standing {
    Standing {
        badge: view.badge,
        waiting_at_least: view.waiting_at_least,
        refused: view.refused,
        fetch_failures: view.fetch_failures,
        parked: view.parked_sessions.len(),
    }
}

/// The page: a title bar, the list on the left, the card on the right.
pub fn approvals_page<V: 'static>(
    view: &ApprovalsView,
    ui: &PageUi,
    host: PageHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let st = standing(view);
    // The header above already names the page ("Tole / Approvals"), so the bar
    // says what is waiting and that an answer is final.
    let bar = div()
        .flex_none()
        .flex()
        .items_center()
        .px(len(size::PADDING))
        .h(len(size::TOPBAR_HEIGHT))
        .border_b_1()
        .border_color(color(p.line))
        .text_size(len(font::CAPTION))
        .text_color(color(p.text_muted))
        .child(div().min_w(len(0.0)).truncate().child(st.top_line()));
    let detail = match &view.selected {
        Some(card) => div()
            .p(len(size::PADDING))
            .flex()
            .flex_col()
            .gap(len(size::PADDING))
            .child(approval_card(
                card,
                ui.details_open(view),
                &ui.focus,
                host.card,
                cx,
            ))
            .into_any_element(),
        None => div()
            .p(len(size::PADDING))
            .flex()
            .flex_col()
            .gap(len(size::PADDING))
            .text_color(color(p.text_faint))
            .child(match view.tab {
                Tab::Waiting => strings::NO_CARD,
                Tab::Decided => strings::NO_CARD_DECIDED,
            })
            .into_any_element(),
    };
    div().size_full().flex().flex_col().child(bar).child(
        div()
            .flex_1()
            .min_h(len(0.0))
            .flex()
            .child(list_pane(view, &st, ui, host, cx))
            .child(
                div()
                    .id("approvals-detail")
                    .flex_1()
                    .min_w(len(0.0))
                    .overflow_y_scroll()
                    .child(detail),
            ),
    )
}

fn list_pane<V: 'static>(
    view: &ApprovalsView,
    st: &Standing,
    ui: &PageUi,
    host: PageHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let tabs = div()
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .gap(len(size::GUTTER))
        .p(len(size::GUTTER))
        .child(
            div()
                .flex()
                .gap(len(size::ROW_GAP))
                .child(tab_button(Tab::Waiting, view, st, ui, host, cx))
                .child(tab_button(Tab::Decided, view, st, ui, host, cx)),
        )
        .child(
            div()
                .text_size(len(font::CAPTION))
                .text_color(color(p.text_faint))
                .child(strings::ALL_FOLDERS),
        );
    let notices: Vec<_> = st
        .notices(view.rows.len())
        .into_iter()
        .map(|text| notice(text).into_any_element())
        .collect();
    let body = if view.rows.is_empty() {
        div()
            .px(len(size::PADDING))
            .py(len(size::GUTTER))
            .text_color(color(p.text_faint))
            .child(st.empty_text(view.tab))
            .into_any_element()
    } else {
        let rows: Vec<_> = view
            .rows
            .iter()
            .map(|row| list_row(row, ui, host, cx).into_any_element())
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(len(size::ROW_GAP))
            .px(len(size::GUTTER))
            .children(rows)
            .into_any_element()
    };
    div()
        .id("approvals-list")
        .w(len(size::LIST_WIDTH))
        .flex_none()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(color(p.line))
        .overflow_y_scroll()
        .child(tabs)
        .children(notices)
        .child(body)
}

fn notice(text: String) -> impl IntoElement {
    let p = DARK;
    div()
        .mx(len(size::GUTTER))
        .mb(len(size::GUTTER))
        .px(len(size::GUTTER))
        .py(len(size::GUTTER))
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(color(tint::WARNING_LINE))
        .bg(color(tint::WARNING))
        .text_size(len(font::CAPTION))
        .text_color(color(p.amber))
        .child(text)
}

fn tab_button<V: 'static>(
    tab: Tab,
    view: &ApprovalsView,
    st: &Standing,
    ui: &PageUi,
    host: PageHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let stop = match tab {
        Tab::Waiting => PageStop::TabWaiting,
        Tab::Decided => PageStop::TabDecided,
    };
    let select = host.select_tab;
    let shown = view.tab == tab;
    let button = div()
        .id(match tab {
            Tab::Waiting => "approvals-tab-waiting",
            Tab::Decided => "approvals-tab-decided",
        })
        .role(Role::Tab)
        .h(len(size::ROW_HEIGHT))
        .flex()
        .items_center()
        .px(len(size::GUTTER))
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(transparent())
        .cursor_pointer()
        .when_some(ui.focus.get(&stop), |el, handle| el.track_focus(handle))
        .focus_visible(|style| style.border_color(color(p.accent)))
        .on_click(cx.listener(move |this, _, _, cx| select(this, tab, cx)))
        .child(st.tab_label(tab));
    if shown {
        button.bg(color(p.raised)).text_color(color(p.text_primary))
    } else {
        button.text_color(color(p.text_muted))
    }
}

/// The ink of the status at the right of a row.
fn status_ink(row: &Row) -> Rgb {
    match row.outcome {
        None => DARK.amber,
        Some(outcome) => outcome_colours(outcome).0,
    }
}

fn list_row<V: 'static>(
    row: &Row,
    ui: &PageUi,
    host: PageHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let id = row.id.clone();
    let select = host.select;
    let status = row
        .left
        .clone()
        .or_else(|| row.outcome_label.map(str::to_owned));
    let title_ink = if row.care == Care::Danger {
        p.danger
    } else {
        p.text_primary
    };
    let frame = div()
        .id(format!("approvals-row-{}", row.id))
        .role(Role::Button)
        .min_h(len(size::LIST_ROW_HEIGHT))
        .w_full()
        .flex()
        .flex_col()
        .gap(len(size::ROW_GAP))
        .px(len(size::GUTTER))
        .py(len(size::GUTTER))
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(transparent())
        .cursor_pointer()
        .when_some(
            ui.focus.get(&PageStop::Row(row.id.clone())),
            |el, handle| el.track_focus(handle),
        )
        .focus_visible(|style| style.border_color(color(p.accent)))
        .hover(|style| style.bg(color(p.raised)))
        .on_click(cx.listener({
            let id = id.clone();
            move |this, _, _, cx| select(this, &id, cx)
        }))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(len(size::GUTTER))
                .child(
                    div()
                        .min_w(len(0.0))
                        .truncate()
                        .text_color(color(title_ink))
                        .child(strings::list_title(row.wording)),
                )
                // Its own chip, never part of the cut text: a long title may be
                // ellipsised, the mark that the file is outside the folder not.
                .children(
                    row.outside_folder
                        .then(|| pill(strings::LIST_OUTSIDE_CHIP, p.danger, tint::REMOVED)),
                )
                .children(status.map(|text| {
                    div()
                        .flex_none()
                        .text_size(len(font::CAPTION))
                        .text_color(color(status_ink(row)))
                        .child(text)
                })),
        )
        .child(
            div()
                .min_w(len(0.0))
                .truncate()
                .text_size(len(font::CAPTION))
                .text_color(color(p.text_muted))
                .child(row.subject.clone()),
        )
        .children(row.from_helper.then(|| {
            div()
                .text_size(len(font::CAPTION))
                .text_color(color(p.text_faint))
                .child(strings::from_helper(""))
        }));
    match (row.selected, row.outcome) {
        (true, None) => frame
            .bg(color(tint::WARNING))
            .border_color(color(tint::WARNING_LINE)),
        (true, Some(_)) => frame.bg(color(p.raised)),
        _ => frame,
    }
}
