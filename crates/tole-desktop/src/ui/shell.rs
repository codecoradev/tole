//! The app shell: header, sidebar and the main area (board S2). A pure
//! function of [`ShellState`]; clicks become [`Action`]s. The header, the
//! sidebar and the home page are in their own files, as `impl ShellView`.

use std::collections::HashMap;

use super::adapter::*;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{strings, Action, FocusTarget, Section, ShellState};

pub struct ShellView {
    pub(super) state: ShellState,
    /// One focus handle per control that Tab visits (`ShellState::tab_order`).
    pub(super) focus: HashMap<FocusTarget, FocusHandle>,
}

impl ShellView {
    pub fn new(state: ShellState) -> Self {
        ShellView {
            state,
            focus: HashMap::new(),
        }
    }

    /// The control that has keyboard focus in `window`, if any (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn focused(&self, window: &Window) -> Option<FocusTarget> {
        self.state
            .tab_order()
            .into_iter()
            .find(|t| self.focus.get(t).is_some_and(|h| is_focused(h, window)))
    }

    /// Gives every control in the tab order a handle, and drops those of
    /// controls that are gone.
    fn sync_focus(&mut self, cx: &mut Context<Self>) {
        let order = self.state.tab_order();
        self.focus.retain(|t, _| order.contains(t));
        for target in order {
            self.focus
                .entry(target)
                .or_insert_with(|| cx.focus_handle().tab_stop(true));
        }
    }

    pub(super) fn dispatch(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.state.apply(action) {
            cx.notify();
        }
    }

    fn main_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let frame = div()
            .flex_1()
            .min_w(len(0.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(len(size::GUTTER))
            .p(len(size::PADDING));
        match self.state.section {
            Section::NewTask => frame.child(self.home(cx)).into_any_element(),
            other => frame
                .child(
                    div()
                        .max_w(len(size::CONTENT_MAX_WIDTH))
                        .text_size(len(font::TITLE))
                        .text_color(color(p.text_primary))
                        .child(strings::section_label(other)),
                )
                .child(
                    div()
                        .max_w(len(size::CONTENT_MAX_WIDTH))
                        .text_color(color(p.text_muted))
                        .child(strings::SECTION_PLACEHOLDER),
                )
                .into_any_element(),
        }
    }
}

impl Render for ShellView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        self.sync_focus(cx);
        let sidebar = self
            .state
            .sidebar_visible()
            .then(|| self.sidebar(cx).into_any_element());
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(color(p.background))
            .text_color(color(p.text_secondary))
            .font_family(font::FAMILY_UI)
            .text_size(len(font::BODY))
            .child(self.header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(len(0.0))
                    .flex()
                    .children(sidebar)
                    .child(self.main_area(cx)),
            )
    }
}
