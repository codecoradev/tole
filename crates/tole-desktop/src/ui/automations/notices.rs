//! The lines of the page that are not the table: what stands where the table
//! would be, and the warnings under it.

use super::super::adapter::*;
use super::types::{AutomationsHost, AutomationsUi};
use tole_desktop_model::strings::automations as words;
use tole_desktop_model::tokens::{font, size, tint, DARK};
use tole_desktop_model::{AutomationsStop, Load};

/// The line that stands where the table would be: checking, could not read, or
/// the empty list, with the server's words under a failure.
pub fn stand_in(text: &'static str, load: &Load) -> impl IntoElement {
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
pub fn notice(text: String, button: Option<AnyElement>) -> impl IntoElement {
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

/// The button that dismisses the notice about the last press.
pub fn dismiss_button<V: 'static>(
    ui: &AutomationsUi,
    host: AutomationsHost<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let p = DARK;
    let dismiss = host.dismiss_notice;
    div()
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
        .child(words::DISMISS)
}
