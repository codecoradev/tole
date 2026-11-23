//! The notice: one line above the main area while the local serve is not
//! ready, with the button the failure calls for. What it says and offers is
//! decided by `status.rs`; this only draws it.

use super::adapter::*;
use super::shell::ShellView;
use super::status::Offer;
use tole_desktop_model::tokens::size;
use tole_desktop_model::tokens::DARK;

impl ShellView {
    /// The button of the notice, if there is one.
    pub(super) fn offer(&self) -> Option<Offer> {
        self.status.as_ref()?.notice()?.offer
    }

    fn notice_button(&self, offer: Offer, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let action = offer.action;
        let button = div()
            .id("notice-action")
            .role(Role::Button)
            .h(len(size::ROW_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .px(len(size::PADDING))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .bg(color(p.accent))
            .text_color(color(p.background))
            .focus_visible(|style| style.border_color(color(p.text_primary)))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.owner_action(action);
                cx.notify();
            }))
            .child(offer.label);
        match &self.notice_focus {
            Some(handle) => button.track_focus(handle),
            None => button,
        }
    }

    /// The line, or nothing while the serve is ready (or for a preview).
    pub(super) fn notice(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let p = DARK;
        let notice = self.status.as_ref()?.notice()?;
        let button = notice.offer.map(|offer| self.notice_button(offer, cx));
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(len(size::GUTTER))
                .px(len(size::PADDING))
                .py(len(size::GUTTER))
                .bg(color(p.panel))
                .border_b_1()
                .border_color(color(p.line))
                .child(
                    div()
                        .size(len(size::DOT))
                        .flex_none()
                        .rounded_full()
                        .bg(color(p.tone(notice.tone))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(len(0.0))
                        .text_color(color(p.text_primary))
                        .child(notice.text),
                )
                .children(button),
        )
    }
}
