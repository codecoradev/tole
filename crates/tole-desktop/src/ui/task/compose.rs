//! The composer (board S3): a box that grows with its text, a hint that says
//! what Enter does now, and the send button. The model decides everything
//! about Enter (`Composer::enter` with the status row's `SendMode`): Enter
//! sends, Shift+Enter breaks the line, Enter while an input method composes is
//! the input method's, blank text and a task that cannot take a message send
//! nothing. This wires the key to it and the result to the driver.

use super::outbox::{Outbox, Seen};
use super::state::{Compose, Pane};
use super::TaskCall;
use crate::driver::task::TaskView;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::task::{Composer, Enter, EnterOutcome, SendMode};
use tole_desktop_model::task_strings as words;
use tole_desktop_model::tokens::{font, size, DARK};

/// Rows the box is tall when empty, and at the most.
const ROWS: (usize, usize) = (2, 8);

impl ShellView {
    /// The composer of the shown task, drawn for `view`. The box is made the
    /// first time (and given focus: the owner is here to write).
    pub(super) fn task_composer(
        &self,
        pane: &mut Pane,
        view: &TaskView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = DARK;
        let mode = view.status.send;
        let compose = pane.compose.get_or_insert_with(|| {
            let hint = words::composer_placeholder(mode);
            let text = TextBox::new(window, cx, hint, ROWS);
            text.focus(window, cx);
            Compose {
                text,
                outbox: Outbox::default(),
                hint,
                refused: None,
            }
        });
        // A note about a refusal is true of the state it was refused in.
        if compose.refused.is_some_and(|at| at != (view.driver, mode)) {
            compose.refused = None;
        }
        let typed = compose.text.text(cx);
        let seen = Seen {
            ignored: view.ignored,
            draft: &view.draft,
            owner_rows: view.owner_rows,
        };
        if let Some(back) = compose.outbox.back(&seen, &typed) {
            compose.text.set_text(&back, window, cx);
        }
        let hint = words::composer_placeholder(mode);
        if hint != compose.hint {
            compose.text.set_hint(hint, window, cx);
            compose.hint = hint;
        }
        let can_send = Composer::with_text(typed).can_submit(mode);
        let blocked = matches!(mode, SendMode::Disabled(_));
        let note = if view.last_send_uncertain {
            Some((words::SEND_UNCERTAIN, p.amber))
        } else if compose.refused.is_some() {
            Some((words::SEND_REFUSED, p.danger))
        } else {
            None
        };
        let send = div()
            .id("task-send")
            .role(Role::Button)
            .flex_none()
            .h(len(size::ROW_HEIGHT))
            .flex()
            .items_center()
            .px(len(size::PADDING))
            .rounded(len(size::RADIUS));
        let send = if can_send {
            send.bg(color(p.accent))
                .text_color(color(p.background))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.task_enter(false, window, cx);
                }))
        } else {
            send.bg(color(p.raised)).text_color(color(p.text_faint))
        };
        let keys = if blocked {
            words::composer_placeholder(mode)
        } else {
            words::COMPOSER_KEYS
        };
        div()
            .flex_none()
            .w_full()
            .flex()
            .justify_center()
            .px(len(size::PADDING))
            .pb(len(size::PADDING))
            .child(
                div()
                    .w_full()
                    .max_w(len(size::TRANSCRIPT_WIDTH))
                    .flex()
                    .flex_col()
                    .gap(len(size::ROW_GAP))
                    .px(len(size::BUBBLE_PAD_X))
                    .py(len(size::GUTTER))
                    .rounded(len(size::BUBBLE_RADIUS))
                    .border_1()
                    .border_color(color(p.line))
                    .bg(color(p.panel))
                    .children(note.map(|(text, tone)| {
                        div()
                            .text_size(len(font::SMALL))
                            .text_color(color(tone))
                            .child(text)
                    }))
                    .child(
                        div()
                            .capture_action(cx.listener(|this, action: &EnterKey, window, cx| {
                                if this.task_enter(action.shift, window, cx) {
                                    cx.stop_propagation();
                                }
                            }))
                            .child(compose.text.element()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(len(size::GUTTER))
                            .child(
                                div()
                                    .min_w(len(0.0))
                                    .truncate()
                                    .text_size(len(font::SMALL))
                                    .text_color(color(p.text_faint))
                                    .child(keys),
                            )
                            .child(send.child(words::send_label(mode))),
                    ),
            )
            .into_any_element()
    }

    /// Give the keyboard to the composer of `id`: the owner chose its tab (or it
    /// took the place of one just closed) to write in it.
    pub(super) fn focus_composer(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(compose) = self.tasks.panes.get(id).and_then(|p| p.compose.as_ref()) {
            compose.text.focus(window, cx);
        }
    }

    /// Enter was pressed in the box of the shown task, or the send button.
    /// Whether the window handled the key: it sent the words, or tried to and
    /// the task refused them. Anything else (a line break, an input method,
    /// nothing to send) is left to the box.
    pub(super) fn task_enter(
        &mut self,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.tasks.tabs.shown().map(str::to_owned) else {
            return false;
        };
        // Out of the map while the source is read through `self`.
        let Some(mut pane) = self.tasks.panes.remove(&id) else {
            return false;
        };
        let handled = self.enter_in(&id, &mut pane, shift, window, cx);
        self.tasks.panes.insert(id, pane);
        cx.notify();
        handled
    }

    fn enter_in(
        &self,
        id: &str,
        pane: &mut Pane,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(compose), Some(source)) = (pane.compose.as_mut(), self.source()) else {
            return false;
        };
        let Some(view) = source.task_view(id) else {
            return false;
        };
        let composing = compose.text.composing(window, cx);
        let mut draft = Composer::with_text(compose.text.text(cx));
        let key = Enter { shift, composing };
        let sent = match draft.enter(key, view.status.send) {
            EnterOutcome::Submit(sent) => sent,
            // The box makes the line break itself.
            EnterOutcome::Newline => return false,
            // Nothing to send (blank, or the task cannot take a message now):
            // the key does nothing, except that an input method keeps its own.
            EnterOutcome::Ignored => return !composing,
        };
        let call = match sent.mode {
            SendMode::Send => TaskCall::Send(sent.text.clone()),
            SendMode::Steer => TaskCall::Steer(sent.text.clone()),
            SendMode::Disabled(_) => return false,
        };
        match source.task_call(id, call) {
            Ok(()) => {
                compose.refused = None;
                compose.text.set_text("", window, cx);
                let seen = Seen {
                    ignored: view.ignored,
                    draft: &view.draft,
                    owner_rows: view.owner_rows,
                };
                compose.outbox.sent(sent, &seen);
            }
            // The words stay in the box.
            Err(_) => compose.refused = Some((view.driver, view.status.send)),
        }
        true
    }
}

/// What the render harness does to a composer that a person does with a keyboard.
#[cfg(feature = "render-harness")]
impl ShellView {
    fn shown_compose(&self) -> Option<&Compose> {
        self.tasks
            .panes
            .get(self.tasks.tabs.shown()?)?
            .compose
            .as_ref()
    }

    /// The text in the box of the shown task.
    pub fn composer_text(&self, cx: &App) -> Option<String> {
        self.shown_compose().map(|c| c.text.text(cx))
    }

    /// Put `text` in the box, as typing would.
    pub fn write_composer(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(compose) = self.shown_compose() {
            compose.text.set_text(text, window, cx);
        }
    }

    /// An input method starts composing `text` in the box.
    pub fn compose_composer(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(compose) = self.shown_compose() {
            compose.text.compose(text, window, cx);
        }
    }

    /// Whether the box has keyboard focus.
    pub fn composer_focused(&self, window: &Window, cx: &App) -> bool {
        self.shown_compose()
            .is_some_and(|c| c.text.focused(window, cx))
    }

    /// Whether the note about a refused send is up.
    pub fn composer_refused(&self) -> bool {
        self.shown_compose().is_some_and(|c| c.refused.is_some())
    }

    /// The task in the main area.
    pub fn shown_task(&self) -> Option<String> {
        self.tasks.tabs.shown().map(str::to_owned)
    }
}
