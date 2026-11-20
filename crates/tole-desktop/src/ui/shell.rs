//! The app shell: top bar, sidebar and a placeholder main area (board S2).
//! A pure function of [`ShellState`]; clicks become [`Action`]s.

use super::adapter::*;
use tole_desktop_model::strings;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{Action, ConnectionStatus, Section, ShellState};

pub struct ShellView {
    state: ShellState,
}

impl ShellView {
    pub fn new(state: ShellState) -> Self {
        ShellView { state }
    }

    fn dispatch(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.state.apply(action) {
            cx.notify();
        }
    }

    fn topbar(&self) -> impl IntoElement {
        let p = DARK;
        div()
            .h(len(size::TOPBAR_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .px(len(size::PADDING))
            .border_b_1()
            .border_color(color(p.line))
            .child(
                div()
                    .text_color(color(p.text_primary))
                    .child(strings::APP_NAME),
            )
            .child(
                div()
                    .text_color(color(p.text_faint))
                    .child(strings::BREADCRUMB_SEPARATOR),
            )
            .child(strings::section_label(self.state.section))
    }

    fn section_row(&self, section: Section, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let selected = self.state.section == section;
        let row = div()
            .id(section.index())
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .justify_between()
            .px(len(size::GUTTER))
            .rounded(len(size::RADIUS))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.dispatch(Action::SelectSection(section), cx);
            }))
            .child(strings::section_label(section))
            .children(
                self.state
                    .badge(section)
                    .map(|n| div().text_color(color(p.amber)).child(n.to_string())),
            );
        if selected {
            row.bg(color(p.raised)).text_color(color(p.text_primary))
        } else {
            row
        }
    }

    fn folder_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let folder = &self.state.folders[index];
        let id = folder.id;
        let selected = self.state.selected_folder == Some(id);
        let row = div()
            .id(("folder", id.0 as usize))
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .px(len(size::GUTTER))
            .rounded(len(size::RADIUS))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.dispatch(Action::SelectFolder(Some(id)), cx);
            }))
            .child(folder.name.clone());
        if selected {
            row.bg(color(p.raised)).text_color(color(p.text_primary))
        } else {
            row
        }
    }

    fn connection_footer(&self) -> impl IntoElement {
        let p = DARK;
        let conn = &self.state.connection;
        let dot = match conn.status {
            ConnectionStatus::Preview => p.text_faint,
            ConnectionStatus::Connecting => p.amber,
            ConnectionStatus::Connected => p.ok,
            ConnectionStatus::Unreachable => p.danger,
        };
        div()
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .px(len(size::GUTTER))
            .h(len(size::ROW_HEIGHT))
            .child(div().size(len(size::DOT)).rounded_full().bg(color(dot)))
            .child(strings::target_label(&conn.target))
            .child(
                div()
                    .text_color(color(p.text_faint))
                    .child(strings::status_label(conn.status)),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let sections: Vec<_> = Section::ALL
            .iter()
            .map(|s| self.section_row(*s, cx).into_any_element())
            .collect();
        let folders: Vec<_> = (0..self.state.folders.len())
            .map(|i| self.folder_row(i, cx).into_any_element())
            .collect();
        let folders_empty = self.state.folders.is_empty();
        let sample_note = self.state.sample_data().then(|| {
            div()
                .px(len(size::GUTTER))
                .text_size(len(font::CAPTION))
                .text_color(color(p.amber))
                .child(strings::FOLDERS_SAMPLE_NOTE)
        });
        div()
            .w(len(size::SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .justify_between()
            .p(len(size::GUTTER))
            .bg(color(p.panel))
            .border_r_1()
            .border_color(color(p.line))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(len(size::ROW_GAP))
                    .children(sections)
                    .child(
                        div()
                            .px(len(size::GUTTER))
                            .pt(len(size::PADDING))
                            .text_size(len(font::CAPTION))
                            .text_color(color(p.text_faint))
                            .child(strings::FOLDERS_CAPTION),
                    )
                    .children(sample_note)
                    .children(folders)
                    .children(folders_empty.then(|| {
                        div()
                            .px(len(size::GUTTER))
                            .text_color(color(p.text_faint))
                            .child(strings::FOLDERS_EMPTY)
                    })),
            )
            .child(self.connection_footer())
    }

    fn main_area(&self) -> impl IntoElement {
        let p = DARK;
        let (title, subtitle) = match self.state.section {
            Section::NewTask => (strings::HOME_TITLE, strings::HOME_SUBTITLE),
            other => (strings::section_label(other), strings::SECTION_PLACEHOLDER),
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(len(size::GUTTER))
            .p(len(size::PADDING))
            .child(
                div()
                    .max_w(len(size::CONTENT_MAX_WIDTH))
                    .text_size(len(font::TITLE))
                    .text_color(color(p.text_primary))
                    .child(title),
            )
            .child(
                div()
                    .max_w(len(size::CONTENT_MAX_WIDTH))
                    .text_color(color(p.text_muted))
                    .child(subtitle),
            )
    }
}

impl Render for ShellView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(color(p.background))
            .text_color(color(p.text_secondary))
            .font_family(font::FAMILY_UI)
            .text_size(len(font::BODY))
            .child(self.topbar())
            .child(
                div()
                    .flex_1()
                    .flex()
                    .child(self.sidebar(cx))
                    .child(self.main_area()),
            )
    }
}
