//! The crossterm adapter (#526): maps a terminal key event to the UI-neutral
//! [`Input`] the app logic consumes. All crossterm key types of the input path
//! stay here; `App::on_input` never sees them.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{Action, App, Input, Key, Mods};

/// The input for a key event, or `None` for a key release (terminals that
/// report releases send both; only presses act).
pub fn input_from(ev: KeyEvent) -> Option<Input> {
    if ev.kind == KeyEventKind::Release {
        return None;
    }
    let key = match ev.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        _ => Key::Other,
    };
    Some(Input {
        key,
        mods: Mods {
            ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
            alt: ev.modifiers.contains(KeyModifiers::ALT),
            shift: ev.modifiers.contains(KeyModifiers::SHIFT),
        },
    })
}

impl App {
    /// A terminal key event: map it (releases are ignored) and hand it to
    /// [`App::on_input`].
    pub fn on_key(&mut self, ev: KeyEvent) -> Action {
        match input_from(ev) {
            Some(input) => self.on_input(input),
            None => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn ev(code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind,
            state: KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> Option<Input> {
        input_from(ev(code, mods, KeyEventKind::Press))
    }

    #[test]
    fn every_key_the_logic_distinguishes_maps_to_its_neutral_key() {
        let none = KeyModifiers::NONE;
        let table = [
            (KeyCode::Char('x'), Key::Char('x')),
            (KeyCode::Char('@'), Key::Char('@')),
            (KeyCode::Enter, Key::Enter),
            (KeyCode::Esc, Key::Esc),
            (KeyCode::Tab, Key::Tab),
            (KeyCode::BackTab, Key::BackTab),
            (KeyCode::Backspace, Key::Backspace),
            (KeyCode::Up, Key::Up),
            (KeyCode::Down, Key::Down),
        ];
        for (code, want) in table {
            assert_eq!(press(code, none), Some(Input::new(want)), "{code:?}");
        }
    }

    #[test]
    fn keys_the_logic_ignores_are_other_not_dropped() {
        // They still count as a key press (they disarm Ctrl+C), so they must
        // reach `on_input` as `Key::Other`.
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Delete,
            KeyCode::PageUp,
            KeyCode::F(5),
        ] {
            assert_eq!(
                press(code, KeyModifiers::NONE),
                Some(Input::new(Key::Other)),
                "{code:?}"
            );
        }
    }

    #[test]
    fn modifiers_are_carried_each_on_its_own_and_combined() {
        let c = KeyCode::Char('c');
        assert_eq!(
            press(c, KeyModifiers::CONTROL),
            Some(Input::ctrl(Key::Char('c')))
        );
        assert_eq!(
            press(KeyCode::Enter, KeyModifiers::ALT),
            Some(Input::alt(Key::Enter))
        );
        assert_eq!(
            press(KeyCode::Enter, KeyModifiers::SHIFT),
            Some(Input::shift(Key::Enter))
        );
        let both = press(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT).unwrap();
        assert_eq!(
            both.mods,
            Mods {
                ctrl: false,
                alt: true,
                shift: true
            }
        );
    }

    #[test]
    fn a_release_is_dropped_and_a_repeat_acts() {
        assert_eq!(
            input_from(ev(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            )),
            None
        );
        assert_eq!(
            input_from(ev(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            Some(Input::new(Key::Char('a')))
        );
        // A release reaching `on_key` does nothing, not even disarming.
        let mut app = App::default();
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Redraw
        );
        assert_eq!(
            app.on_key(ev(KeyCode::Left, KeyModifiers::NONE, KeyEventKind::Release)),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }
}
