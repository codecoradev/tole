//! The app shell: header, sidebar and the main area (board S2). A pure
//! function of [`ShellState`]; clicks become [`Action`]s. The header, the
//! sidebar and the home page are in their own files, as `impl ShellView`.

use std::collections::HashMap;

use super::adapter::*;
use super::status::{badge_text, Source, Status, LOOK_EVERY};
#[cfg(unix)]
use super::task::TaskPanes;
use tole_desktop_model::tokens::{font, size, DARK};
use tole_desktop_model::{
    strings, Action, FocusTarget, OwnerAction, Section, ShellState, SkillsPane,
};

/// The app behind the window and the repeating look at it.
struct Live {
    source: Box<dyn Source>,
    _look: Every,
}

pub struct ShellView {
    pub(super) state: ShellState,
    /// One focus handle per control that Tab visits (`ShellState::tab_order`).
    pub(super) focus: HashMap<FocusTarget, FocusHandle>,
    /// What the app says besides the shell state; `None` for a preview.
    pub(super) status: Option<Status>,
    /// The button of the notice, a tab stop while there is one.
    pub(super) notice_focus: Option<FocusHandle>,
    /// The Skills and tools page as the app last said it, with the tab the
    /// owner chose (see `ui::skills`).
    pub(super) skills: SkillsPane,
    live: Option<Live>,
    /// The open tasks and which one is shown.
    #[cfg(unix)]
    pub(super) tasks: TaskPanes,
    /// A source the render harness gives a window that is not live.
    #[cfg(feature = "render-harness")]
    preview: Option<Box<dyn Source>>,
}

impl ShellView {
    /// A window with nothing behind it: `state` is shown as it is.
    pub fn new(state: ShellState) -> Self {
        ShellView {
            state,
            focus: HashMap::new(),
            status: None,
            notice_focus: None,
            skills: SkillsPane::default(),
            live: None,
            #[cfg(unix)]
            tasks: TaskPanes::default(),
            #[cfg(feature = "render-harness")]
            preview: None,
        }
    }

    /// A window that shows `source`: its state now, and again on every change
    /// the source reports (looked for every [`LOOK_EVERY`], on this thread).
    /// The source is let go when the window closes, so its owner can end it.
    pub fn live(state: ShellState, source: Box<dyn Source>, cx: &mut Context<Self>) -> Self {
        let mut view = ShellView::new(state);
        // A change from before the window existed is shown by this read.
        let _ = source.changed();
        view.status = Some(source.refresh(&mut view.state));
        if let Some(fresh) = source.skills_page() {
            view.adopt_skills(fresh);
        }
        let look = every(cx, LOOK_EVERY, |this: &mut ShellView, cx| this.look(cx));
        on_release(cx, |this: &mut ShellView| this.live = None);
        view.live = Some(Live {
            source,
            _look: look,
        });
        view
    }

    /// A window whose app could not start: `status` says so, with the
    /// connection shown as it reads.
    pub fn failed(state: ShellState, status: Status) -> Self {
        let mut view = ShellView::new(state);
        view.state.apply(Action::SetConnectionStatus(
            status.serve.connection_status(),
        ));
        view.status = Some(status);
        view
    }

    /// A window showing `status` with no app behind it (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn with_status(state: ShellState, status: Status) -> Self {
        let mut view = ShellView::new(state);
        view.status = Some(status);
        view
    }

    /// On the repeating timer: read the app again when it says it changed.
    fn look(&mut self, cx: &mut Context<Self>) {
        let Some(live) = &self.live else {
            return;
        };
        if live.source.changed() {
            self.status = Some(live.source.refresh(&mut self.state));
            if let Some(fresh) = live.source.skills_page() {
                self.adopt_skills(fresh);
            }
            cx.notify();
        }
    }

    /// The Refresh button of the Skills and tools page was pressed.
    pub(super) fn refresh_skills(&self) {
        if let Some(live) = &self.live {
            live.source.refresh_skills();
        }
    }

    /// The button of the notice was pressed.
    pub(super) fn owner_action(&mut self, action: OwnerAction) {
        if let Some(live) = &self.live {
            live.source.owner_action(action);
        }
    }

    /// What the sidebar draws beside a row.
    pub(super) fn badge_text(&self, section: Section) -> Option<String> {
        badge_text(self.status.as_ref(), &self.state, section)
    }

    /// A window showing `status` and reading tasks from `source`, with no timer
    /// behind it (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn with_source(state: ShellState, status: Status, source: Box<dyn Source>) -> Self {
        let mut view = ShellView::with_status(state, status);
        view.preview = Some(source);
        view
    }

    /// What the window reads tasks from and sends the owner's actions to.
    #[cfg(unix)]
    pub(super) fn source(&self) -> Option<&dyn Source> {
        #[cfg(feature = "render-harness")]
        if let Some(preview) = &self.preview {
            return Some(preview.as_ref());
        }
        self.live.as_ref().map(|live| live.source.as_ref())
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
        if self.offer().is_some() {
            self.notice_focus
                .get_or_insert_with(|| cx.focus_handle().tab_stop(true));
        } else {
            self.notice_focus = None;
        }
    }

    pub(super) fn dispatch(&mut self, action: Action, cx: &mut Context<Self>) {
        // A section or the home page takes the main area; the tasks stay open.
        #[cfg(unix)]
        if matches!(action, Action::SelectSection(_) | Action::NewTask) {
            self.tasks.tabs.leave();
        }
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
            Section::SkillsAndTools => self.skills_page(cx).into_any_element(),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        self.sync_focus(cx);
        let task = self.task_pane(window, cx);
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
                    .child(
                        div()
                            .flex_1()
                            .min_w(len(0.0))
                            .flex()
                            .flex_col()
                            .children(self.notice(cx))
                            .child(match task {
                                Some(task) => task,
                                None => self.main_area(cx).into_any_element(),
                            }),
                    ),
            )
    }
}
