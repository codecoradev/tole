//! The home page: the working folders and the "New task" button (board S2).
//! The composer, "Pick up where you left off" and "Coming up" need the client.

use super::adapter::*;
use super::shell::ShellView;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{strings, Action, FocusTarget, HomeFolder};

impl ShellView {
    fn folder_chip(&self, folder: &HomeFolder, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let id = folder.id;
        let chip = div()
            .id(("home-folder", id.0 as usize))
            .track_focus(&self.focus[&FocusTarget::HomeFolder(id)])
            .role(Role::Button)
            .h(len(size::ROW_HEIGHT))
            .max_w(len(size::CHIP_MAX_WIDTH))
            .flex()
            .items_center()
            .px(len(size::PADDING))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(color(p.line))
            .focus_visible(|style| style.border_color(color(p.accent)))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.dispatch(Action::SelectFolder(Some(id)), cx);
            }))
            .child(div().min_w(len(0.0)).truncate().child(folder.name.clone()));
        if folder.selected {
            chip.bg(color(p.raised)).text_color(color(p.text_primary))
        } else {
            chip
        }
    }

    /// Accent while it can be pressed; flat and faint while it cannot.
    fn start_button(&self, can_start: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let button = div()
            .id("home-start")
            .role(Role::Button)
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .justify_center()
            .px(len(size::PADDING))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .child(strings::HOME_START);
        if can_start {
            button
                .track_focus(&self.focus[&FocusTarget::HomeStart])
                .bg(color(p.accent))
                .text_color(color(p.background))
                .focus_visible(|style| style.border_color(color(p.text_primary)))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.dispatch(Action::NewTask, cx)))
                .into_any_element()
        } else {
            button
                .bg(color(p.raised))
                .text_color(color(p.text_faint))
                .into_any_element()
        }
    }

    pub(super) fn home(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let home = self.state.home();
        let chips: Vec<_> = home
            .folders
            .iter()
            .map(|f| self.folder_chip(f, cx).into_any_element())
            .collect();
        let folders = if chips.is_empty() {
            div()
                .text_color(color(p.text_faint))
                .child(strings::HOME_FOLDERS_EMPTY)
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_wrap()
                .gap(len(size::GUTTER))
                .children(chips)
                .into_any_element()
        };
        let blocked = (!home.can_start).then(|| {
            div()
                .text_size(len(font::CAPTION))
                .text_color(color(p.text_faint))
                .child(strings::HOME_START_BLOCKED)
        });
        div()
            .w_full()
            .max_w(len(size::CONTENT_MAX_WIDTH))
            .flex()
            .flex_col()
            .gap(len(size::PADDING))
            .child(
                div()
                    .text_size(len(font::TITLE))
                    .text_color(color(p.text_primary))
                    .child(strings::HOME_TITLE),
            )
            .child(
                div()
                    .text_color(color(p.text_muted))
                    .child(strings::HOME_SUBTITLE),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(len(size::GUTTER))
                    .child(
                        div()
                            .text_size(len(font::CAPTION))
                            .text_color(color(p.text_faint))
                            .child(strings::HOME_FOLDERS_CAPTION),
                    )
                    .child(folders),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(len(size::GUTTER))
                    .child(self.start_button(home.can_start, cx))
                    .children(blocked),
            )
    }
}
