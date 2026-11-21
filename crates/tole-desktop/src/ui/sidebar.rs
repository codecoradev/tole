//! The sidebar: sections with their counts, the folders and the connection
//! footer (board S2).

use super::adapter::*;
use super::shell::ShellView;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{strings, Action, FocusTarget, Section};

impl ShellView {
    fn section_row(&self, section: Section, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let selected = self.state.section == section;
        let badge = self.state.badge(section).map(|count| {
            let tone = if section.asks_owner() {
                p.amber
            } else {
                p.text_faint
            };
            div()
                .flex_none()
                .text_color(color(tone))
                .child(strings::badge_label(section, count))
        });
        let row = div()
            .id(section.index())
            .track_focus(&self.focus[&FocusTarget::Section(section)])
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .justify_between()
            .gap(len(size::GUTTER))
            .px(len(size::GUTTER))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .focus_visible(|style| style.border_color(color(p.accent)))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.dispatch(Action::SelectSection(section), cx);
            }))
            .child(
                div()
                    .min_w(len(0.0))
                    .truncate()
                    .child(strings::section_label(section)),
            )
            .children(badge);
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
            .track_focus(&self.focus[&FocusTarget::Folder(id)])
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .px(len(size::GUTTER))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .focus_visible(|style| style.border_color(color(p.accent)))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.dispatch(Action::SelectFolder(Some(id)), cx);
            }))
            .child(div().min_w(len(0.0)).truncate().child(folder.name.clone()));
        if selected {
            row.bg(color(p.raised)).text_color(color(p.text_primary))
        } else {
            row
        }
    }

    /// The dot, the target and the status in words. The dot never shrinks and
    /// the two lines are cut at the sidebar edge instead of running over it.
    fn connection_footer(&self) -> impl IntoElement {
        let p = DARK;
        let indicator = self.state.connection_indicator();
        div()
            .h(len(size::FOOTER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .gap(len(size::GUTTER))
            .px(len(size::GUTTER))
            .child(
                div()
                    .size(len(size::DOT))
                    .flex_none()
                    .rounded_full()
                    .bg(color(p.tone(indicator.tone))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(len(0.0))
                    .flex()
                    .flex_col()
                    .child(div().truncate().child(indicator.target))
                    .child(
                        div()
                            .truncate()
                            .text_size(len(font::CAPTION))
                            .text_color(color(p.text_faint))
                            .child(indicator.status),
                    ),
            )
    }

    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
}
