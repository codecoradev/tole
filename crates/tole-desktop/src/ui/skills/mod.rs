//! The Skills and tools page (boards S7 and S8, #1034): two tabs, Skills and
//! Connected tools, each drawn from the model's `SkillsPageState` as the
//! skills driver keeps it. Read-only: there is no button that approves a skill
//! or trusts a tool. That is a terminal command, and the page says so.
//!
//! What a tab shows is the model's decision ([`SkillsBody`], `tab_badge`,
//! `skills_inbox_is_empty`, `no_connected_tools`): a tab that was not read, is
//! being read or failed shows that, never rows and never an empty list, and a
//! partial answer is never read as "nothing is waiting". This module only draws.

mod cards;
mod parts;
mod skill_list;

use super::adapter::*;
use super::shell::ShellView;
use tole_desktop_model::strings::skills as words;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{FocusTarget, SkillsBody, SkillsPane, Tab};

/// The tabs, left to right.
const TABS: [Tab; 2] = [Tab::Skills, Tab::ConnectedTools];

impl ShellView {
    /// A window that shows `view` as the page, with no app behind it (for the
    /// render harness).
    #[cfg(feature = "render-harness")]
    pub fn with_skills(state: tole_desktop_model::ShellState, view: SkillsPane) -> Self {
        let mut shown = ShellView::new(state);
        shown.skills = view;
        shown
    }

    /// The page as the app reads it now, keeping the tab the owner chose.
    pub(super) fn adopt_skills(&mut self, fresh: SkillsPane) {
        self.skills.adopt(fresh);
    }

    fn select_skills_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.skills.page.tab != tab {
            self.skills.page.select_tab(tab);
            cx.notify();
        }
    }

    fn skills_tab_button(&self, tab: Tab, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let page = &self.skills.page;
        let button = div()
            .id(("skills-tab", tab as usize))
            .track_focus(&self.focus[&FocusTarget::SkillsTab(tab)])
            .role(Role::Tab)
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .px(len(size::PADDING))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(transparent())
            .focus_visible(|style| style.border_color(color(p.accent)))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(move |this, _, _, cx| this.select_skills_tab(tab, cx)))
            .child(words::tab_label(tab, page.tab_badge(tab)));
        if page.tab == tab {
            button.bg(color(p.raised)).text_color(color(p.text_primary))
        } else {
            button
        }
    }

    /// Refresh: ask the driver to read both routes now. While a call is out it
    /// says so and is still a button (a second press is one request).
    fn skills_refresh_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let busy = self.skills.refreshing();
        div()
            .id("skills-refresh")
            .track_focus(&self.focus[&FocusTarget::SkillsRefresh])
            .role(Role::Button)
            .h(len(size::ROW_HEIGHT))
            .min_w(len(size::BUTTON_MIN_WIDTH))
            .flex()
            .items_center()
            .justify_center()
            .px(len(size::PADDING))
            .rounded(len(size::RADIUS))
            .border_1()
            .border_color(color(p.line))
            .text_color(color(if busy { p.text_faint } else { p.text_secondary }))
            .focus_visible(|style| style.border_color(color(p.accent)))
            .cursor_pointer()
            .hover(|style| style.bg(color(p.raised)))
            .on_click(cx.listener(|this, _, _, _| this.refresh_skills()))
            .child(if busy {
                words::REFRESHING
            } else {
                words::REFRESH
            })
    }

    /// The rows of the tab, or what stands in their place.
    fn skills_body(&self) -> impl IntoElement {
        let page = &self.skills.page;
        match page.body(page.tab, self.skills.call_out(page.tab)) {
            SkillsBody::NotLoaded => parts::plain(words::NOT_LOADED).into_any_element(),
            SkillsBody::Loading => parts::plain(words::LOADING).into_any_element(),
            SkillsBody::Failed(why) => parts::failure(words::LOAD_FAILED, &why).into_any_element(),
            SkillsBody::Rows => self.skills_rows().into_any_element(),
        }
    }

    /// The tab's answer: the notes of a partial read first, so no list below
    /// them reads as complete.
    fn skills_rows(&self) -> impl IntoElement {
        let page = &self.skills.page;
        let notes = match page.tab {
            Tab::Skills => &page.skills.notes,
            Tab::ConnectedTools => &page.tools_notes,
        };
        div()
            .flex()
            .flex_col()
            .gap(len(size::GUTTER))
            .children(parts::partial(words::PARTIAL, notes))
            .children((page.tab == Tab::Skills).then(|| self.skill_groups()))
            .into_any_element()
    }

    /// What the owner has to do elsewhere, and what Tole can never do.
    fn skills_footnote(&self) -> impl IntoElement {
        let (owner, hints) = match self.skills.page.tab {
            Tab::Skills => (
                words::OWNER_ONLY_SKILLS,
                words::skills_hints(&self.skills.page.skills.review),
            ),
            Tab::ConnectedTools => (words::OWNER_ONLY_TOOLS, vec![words::HINT_TOOLS]),
        };
        div()
            .flex()
            .flex_col()
            .gap(len(size::ROW_GAP))
            .child(parts::caption(owner))
            .children(hints.into_iter().map(parts::caption))
    }

    /// The Skills and tools page: heading, tabs, refresh, the tab's body.
    pub(crate) fn skills_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let subtitle = match self.skills.page.tab {
            Tab::Skills => words::SKILLS_SUBTITLE,
            Tab::ConnectedTools => words::TOOLS_SUBTITLE,
        };
        let tabs: Vec<_> = TABS
            .iter()
            .map(|t| self.skills_tab_button(*t, cx).into_any_element())
            .collect();
        div()
            .id("skills-page")
            .flex_1()
            .min_w(len(0.0))
            .min_h(len(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_start()
            .p(len(size::PADDING))
            .child(
                div()
                    .w_full()
                    .max_w(len(size::PAGE_MAX_WIDTH))
                    .flex()
                    .flex_col()
                    .gap(len(size::PADDING))
                    .child(
                        div()
                            .text_size(len(font::TITLE))
                            .text_color(color(p.text_primary))
                            .child(words::TITLE),
                    )
                    .child(div().text_color(color(p.text_muted)).child(subtitle))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(len(size::GUTTER))
                            .child(div().flex().gap(len(size::ROW_GAP)).children(tabs))
                            .child(self.skills_refresh_button(cx)),
                    )
                    .child(self.skills_body())
                    .child(self.skills_footnote()),
            )
    }
}
