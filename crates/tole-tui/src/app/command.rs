//! Slash commands and the picker keys: Enter on a `/…` draft, the effort and
//! model cycling hotkeys, and the keys of an open picker.

use crate::{menu, Action, App, HostCmd, Key, PickKind};

impl App {
    pub(crate) fn cycle_effort(&self) -> Action {
        Action::Host(HostCmd::SetEffort(menu::next_effort(
            self.footer.effort.as_deref(),
        )))
    }

    pub(crate) fn cycle_model(&mut self) -> Action {
        if self.recent.len() < 2 {
            self.note("(no other recent model)");
            return Action::Redraw;
        }
        self.recent.rotate_left(1);
        Action::Host(HostCmd::SetModel(self.recent[0].clone()))
    }

    pub(crate) fn picker_key(&mut self, key: Key) -> Action {
        let Some(p) = self.picker.as_mut() else {
            return Action::None;
        };
        match key {
            Key::Esc => self.picker = None,
            Key::Up => p.up(),
            Key::Down => p.down(),
            Key::Backspace => p.backspace(),
            Key::Char(c) => p.type_char(c),
            Key::Enter => {
                let kind = p.kind;
                let chosen = p.chosen();
                self.picker = None;
                return match (kind, chosen) {
                    (PickKind::Model, Some(id)) => {
                        self.note_model_chosen(&id);
                        Action::Host(HostCmd::SetModel(id))
                    }
                    (PickKind::Session, Some(id)) => Action::Host(HostCmd::Switch(id)),
                    // /tree lists the open session too: picking it is a no-op.
                    (PickKind::Tree, Some(id)) if id == self.footer.session => {
                        self.note(&format!("(already in {id})"));
                        Action::Redraw
                    }
                    (PickKind::Tree, Some(id)) => Action::Host(HostCmd::Switch(id)),
                    _ => Action::Redraw,
                };
            }
            _ => return Action::None,
        }
        Action::Redraw
    }

    /// Enter on a `/…` draft: run the command or refuse it with a hint.
    pub(crate) fn command(&mut self, text: &str) -> Action {
        let (cmd, arg) = match text.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (text, ""),
        };
        let arg_opt = (!arg.is_empty()).then(|| arg.to_string());
        let host = |c| Action::Host(c);
        match cmd {
            "/quit" => Action::Quit,
            "/model" => match arg_opt {
                Some(id) => {
                    self.note_model_chosen(&id);
                    host(HostCmd::SetModel(id))
                }
                None => host(HostCmd::PickModel),
            },
            "/thinking" => match arg {
                "" => self.cycle_effort(),
                "unset" => host(HostCmd::SetEffort(None)),
                l if menu::EFFORTS.contains(&l) => host(HostCmd::SetEffort(Some(l.into()))),
                _ => {
                    self.note("(usage: /thinking [minimal | low | medium | high | unset])");
                    Action::Redraw
                }
            },
            "/compact" => host(HostCmd::Compact(arg_opt)),
            "/new" => host(HostCmd::New),
            "/resume" => host(HostCmd::PickSession),
            "/fork" => host(HostCmd::Fork(arg_opt)),
            "/tree" => host(HostCmd::PickTree),
            "/session" => host(HostCmd::Status),
            "/image" => match arg {
                "" => host(HostCmd::Images),
                "clear" => host(HostCmd::ClearImages),
                spec => host(HostCmd::AttachImage(spec.to_string())),
            },
            "/copy" if arg.is_empty() => host(HostCmd::LastAnswer),
            "/copy" => {
                self.note("(usage: /copy; it takes no argument, /export [path] writes a file)");
                Action::Redraw
            }
            "/export" => host(HostCmd::Export(arg_opt)),
            "/name" => match arg_opt {
                Some(n) => host(HostCmd::Name(n)),
                None => {
                    self.note("(usage: /name <text>)");
                    Action::Redraw
                }
            },
            _ => {
                self.note(&menu::unknown(cmd));
                Action::Redraw
            }
        }
    }
}
