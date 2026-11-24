//! The on/off switch of a row.

use super::super::adapter::*;
use super::types::{AutomationsHost, AutomationsUi};
use tole_desktop_model::tokens::{size, DARK};
use tole_desktop_model::{AutomationId, AutomationsStop, Switch};

/// The on/off switch, drawn from a track and a knob. It shows the state the
/// serve last reported; while its call is out it is dimmed, and it changes only
/// when the serve answers.
pub fn switch<V: 'static>(
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
