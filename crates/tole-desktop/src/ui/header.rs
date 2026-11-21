//! The header: sidebar toggle, breadcrumb and connection indicator (board S2).

use super::adapter::*;
use super::shell::ShellView;
use tole_desktop_model::strings;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{Action, FocusTarget};

impl ShellView {
    /// The toggle is an icon drawn from boxes, so its name for a screen reader
    /// is the model's `sidebar_toggle_label`.
    fn sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let label = self.state.sidebar_toggle_label();
        let icon = div()
            .size(len(size::ICON))
            .flex_none()
            .flex()
            .rounded(len(size::RADIUS / 2.0))
            .border_1()
            .border_color(color(p.text_muted))
            .child(
                div()
                    .w(len(size::ICON / 3.0))
                    .h_full()
                    .border_r_1()
                    .border_color(color(p.text_muted)),
            );
        div()
            .id("sidebar-toggle")
            .track_focus(&self.focus[&FocusTarget::SidebarToggle])
            .role(Role::Button)
            .aria_label(label)
            .size(len(size::ROW_HEIGHT - size::GUTTER))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .focus_visible(|style| style.border_color(color(p.accent)))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(|this, _, _, cx| this.dispatch(Action::ToggleSidebar, cx)))
            .child(icon)
    }

    fn breadcrumb(&self) -> impl IntoElement {
        let p = DARK;
        let crumbs = self.state.breadcrumb();
        let last = crumbs.len().saturating_sub(1);
        let mut row = div()
            .flex()
            .flex_1()
            .min_w(len(0.0))
            .items_center()
            .gap(len(size::GUTTER))
            .overflow_hidden();
        for (i, text) in crumbs.into_iter().enumerate() {
            if i > 0 {
                row = row.child(
                    div()
                        .flex_none()
                        .text_color(color(p.text_faint))
                        .child(strings::BREADCRUMB_SEPARATOR),
                );
            }
            // The last crumb is the one that is cut when it is too long.
            let crumb = if i == last {
                div()
                    .min_w(len(0.0))
                    .truncate()
                    .text_color(color(p.text_secondary))
            } else {
                div().flex_none().text_color(color(p.text_primary))
            };
            row = row.child(crumb.child(text));
        }
        row
    }

    /// The dot and the target on the right of the header; the status in words
    /// is in the sidebar footer.
    fn connection_indicator(&self) -> impl IntoElement {
        let p = DARK;
        let indicator = self.state.connection_indicator();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(len(size::GUTTER))
            .max_w(len(size::HEADER_LABEL_MAX_WIDTH))
            .child(
                div()
                    .size(len(size::DOT))
                    .flex_none()
                    .rounded_full()
                    .bg(color(p.tone(indicator.tone))),
            )
            .child(
                div()
                    .min_w(len(0.0))
                    .truncate()
                    .text_size(len(font::BODY))
                    .text_color(color(p.text_muted))
                    .child(indicator.target),
            )
    }

    pub(super) fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        div()
            .h(len(size::TOPBAR_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .px(len(size::GUTTER))
            .border_b_1()
            .border_color(color(p.line))
            .child(self.sidebar_toggle(cx))
            .child(self.breadcrumb())
            .child(self.connection_indicator())
    }
}
