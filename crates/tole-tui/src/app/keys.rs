//! Key and paste input (#526): what a key press asks of the loop ([`Action`])
//! and the handling of one input (approval answers, hotkeys, the editor, the
//! menu and the `@` panel).

use tole_core::approval::Verdict;

use crate::{
    files, paste, App, HostCmd, Input, Key, KeyAction, KeyPress, Kind, Pending, MAX_QUEUE,
};

/// What a key press asks of the loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Redraw,
    Quit,
    Submit(String),
    /// Enter while a turn runs: a soft interrupt (#377) for the running turn.
    Steer(String),
    Cancel,
    /// A slash command or hotkey for the host; the loop runs it.
    Host(HostCmd),
    /// A paste that is exactly one image reference (#560): the loop asks the
    /// host to attach `spec`; if the host refuses, `text` is inserted into the
    /// editor as an ordinary paste.
    PasteImage {
        spec: String,
        text: String,
    },
}

impl App {
    /// One input (UI-neutral, #526): a key press or a paste. The terminal
    /// adapter in `term_input` maps crossterm events onto this.
    pub fn on_input(&mut self, input: Input) -> Action {
        match input {
            Input::Key(key) => self.on_key_press(key),
            Input::Paste(text) => self.on_paste(&text),
        }
    }

    /// A pasted block (#560). It never submits, steers or answers anything:
    /// at an approval prompt it is ignored, in a picker it filters, otherwise
    /// it lands in the editor as one edit — unless, while idle, it is exactly
    /// one image reference, which the loop offers to the host to attach.
    fn on_paste(&mut self, raw: &str) -> Action {
        self.armed = false;
        if self.approval.is_some() {
            return Action::None;
        }
        let text = paste::normalize(raw);
        if let Some(p) = self.picker.as_mut() {
            for c in text.chars().filter(|c| !c.is_control()) {
                p.type_char(c);
            }
            return Action::Redraw;
        }
        if text.is_empty() {
            return Action::None;
        }
        if !self.running {
            if let Some(spec) = paste::image_spec(&text) {
                return Action::PasteImage { spec, text };
            }
        }
        self.paste_text(&text);
        Action::Redraw
    }

    /// Insert an (already normalized) paste into the editor as one edit.
    pub fn paste_text(&mut self, text: &str) {
        self.menu_sel = 0;
        self.at_sel = 0;
        self.editor.paste(text);
    }

    fn on_key_press(&mut self, key: KeyPress) -> Action {
        let ctrl = key.mods.ctrl;
        if ctrl && key.key == Key::Char('c') {
            if self.running {
                return self.cancel();
            }
            if self.armed {
                return Action::Quit;
            }
            self.armed = true;
            return Action::Redraw;
        }
        self.armed = false;
        if let Some(p) = self.approval.take() {
            return self.answer(p, key);
        }
        if self.picker.is_some() {
            return self.picker_key(key.key);
        }
        // Everything below is reached only after the approval prompt, the
        // picker and Ctrl+C had their say: a keymap cannot intercept them.
        // Hotkeys wait for idle (they take the session lock). Shift+Tab is
        // `BackTab`, a different key code from Enter, so Shift+Enter keeps
        // its newline meaning below.
        let act = self.keymap.action(&key);
        match act {
            Some(KeyAction::ToggleTools) => {
                let mut v = self.transcript.toggle_tools();
                let state = if self.transcript.expand_tools {
                    "expanded"
                } else {
                    "collapsed"
                };
                v.extend(
                    self.transcript
                        .line(Kind::Note, &format!("(tool output {state})")),
                );
                self.out.extend(v);
                return Action::Redraw;
            }
            Some(KeyAction::ToggleReasoning) => {
                let mut v = self.transcript.toggle_reasoning();
                let state = if self.transcript.hide_reasoning {
                    "hidden"
                } else {
                    "shown"
                };
                v.extend(
                    self.transcript
                        .line(Kind::Note, &format!("(reasoning {state})")),
                );
                self.out.extend(v);
                return Action::Redraw;
            }
            Some(KeyAction::ModelPicker) => {
                return if self.running {
                    Action::None
                } else {
                    Action::Host(HostCmd::PickModel)
                }
            }
            Some(KeyAction::CycleModel) => {
                return if self.running {
                    Action::None
                } else {
                    self.cycle_model()
                }
            }
            Some(KeyAction::CycleThinking) => {
                return if self.running {
                    Action::None
                } else {
                    self.cycle_effort()
                }
            }
            _ => {}
        }
        if !self.at_matches().is_empty() {
            let n = self.at_matches().len();
            match key.key {
                Key::Tab | Key::Enter if self.at_insert() => return Action::Redraw,
                Key::Up if !ctrl && !key.mods.alt => {
                    self.at_sel = self.at_sel.min(n - 1).saturating_sub(1);
                    return Action::Redraw;
                }
                Key::Down if !ctrl && !key.mods.alt => {
                    self.at_sel = (self.at_sel + 1).min(n - 1);
                    return Action::Redraw;
                }
                _ => {}
            }
        }
        if act == Some(KeyAction::PopQueue) {
            return match self.queue.pop() {
                Some(last) => {
                    self.prepend_draft(vec![last]);
                    Action::Redraw
                }
                None => Action::None,
            };
        }
        let menu = self.menu();
        match key.key {
            Key::Tab if !menu.is_empty() => {
                let pick = menu[self.menu_sel.min(menu.len() - 1)];
                self.editor.set(&format!("{pick} "));
                self.menu_sel = 0;
            }
            Key::Up if !menu.is_empty() => self.menu_sel = self.menu_sel.saturating_sub(1),
            Key::Down if !menu.is_empty() => {
                self.menu_sel = (self.menu_sel + 1).min(menu.len() - 1)
            }
            Key::Esc if self.running => return self.cancel(),
            // Alt+Enter while a turn runs queues a follow-up; idle it is a newline.
            _ if act == Some(KeyAction::QueueFollowUp) && self.running => {
                let t = self.editor.text().trim();
                if t.is_empty() || t.starts_with('/') {
                    return Action::None;
                }
                if self.queue.len() >= MAX_QUEUE {
                    self.note(&format!("({MAX_QUEUE} follow-ups are already queued)"));
                    return Action::Redraw;
                }
                if let Some(text) = self.editor.submit() {
                    self.queue.push(text);
                }
            }
            // Idle, the follow-up key is a newline (as Alt+Enter always was).
            _ if matches!(act, Some(KeyAction::Newline | KeyAction::QueueFollowUp)) => {
                self.editor.newline()
            }
            // Shift/Alt+Enter never send, whatever is (not) bound to them.
            Key::Enter if key.mods.shift || key.mods.alt => return Action::None,
            Key::Enter => {
                if self.running {
                    // Steer: a non-empty, non-slash draft becomes a soft interrupt.
                    let t = self.editor.text().trim();
                    if t.is_empty() || t.starts_with('/') {
                        return Action::None;
                    }
                    return self.editor.submit().map_or(Action::None, Action::Steer);
                }
                if self.editor.text().trim_start().starts_with('/') {
                    let text = self.editor.submit().unwrap_or_default();
                    self.menu_sel = 0;
                    return self.command(&text);
                }
                return match self.editor.submit() {
                    Some(text) => {
                        self.running = true;
                        self.saw_text = false;
                        self.images_sent();
                        let echoed = self.transcript.line(Kind::User, &format!("> {text}"));
                        self.out.extend(echoed);
                        Action::Submit(text)
                    }
                    None => Action::Redraw,
                };
            }
            _ if act == Some(KeyAction::HistoryPrev) => self.editor.prev(),
            _ if act == Some(KeyAction::HistoryNext) => self.editor.next(),
            Key::Char(c) if !ctrl => {
                self.menu_sel = 0;
                self.at_sel = 0;
                if c == '@' && self.files.is_none() {
                    self.files = Some(
                        std::env::current_dir().map_or_else(|_| Vec::new(), |d| files::list(&d)),
                    );
                }
                self.editor.insert(c)
            }
            Key::Backspace => {
                self.menu_sel = 0;
                self.at_sel = 0;
                self.editor.backspace()
            }
            _ => return Action::None,
        }
        Action::Redraw
    }

    /// y/n answer (plain keys only); Esc denies AND cancels the turn; anything else re-asks.
    fn answer(&mut self, p: Pending, key: KeyPress) -> Action {
        // y / n are the plain keys: Alt+Y or Ctrl+N (a user may bind those to
        // something else) never answer a prompt by accident.
        let plain = !key.mods.ctrl && !key.mods.alt;
        let (verdict, note) = match key.key {
            Key::Char('y' | 'Y') if plain => (Verdict::Allow, "allowed"),
            Key::Char('n' | 'N') if plain => (Verdict::Deny, "denied"),
            Key::Esc => {
                p.resolve(Verdict::Deny);
                let lines = self.transcript.line(Kind::Note, "  denied");
                self.out.extend(lines);
                self.return_queue(Vec::new());
                return Action::Cancel;
            }
            _ => {
                self.approval = Some(p);
                return Action::None;
            }
        };
        p.resolve(verdict);
        let lines = self.transcript.line(Kind::Note, &format!("  {note}"));
        self.out.extend(lines);
        Action::Redraw
    }

    fn cancel(&mut self) -> Action {
        // A pending approval is dropped unanswered: the turn thread sees Deny.
        self.approval = None;
        self.return_queue(Vec::new());
        Action::Cancel
    }
}
