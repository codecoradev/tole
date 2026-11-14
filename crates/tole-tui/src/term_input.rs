//! The crossterm adapter (#526): maps a terminal key event (or a bracketed
//! paste, #560) to the UI-neutral [`Input`] the app logic consumes. All
//! crossterm types of the input path stay here; `App::on_input` never sees
//! them.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{Action, App, Input, Key, KeyPress, Mods};

/// The input for a terminal event: a key press or a bracketed paste. `None`
/// for everything the app logic does not consume (key releases, resize,
/// focus, mouse).
pub fn input_from_event(ev: Event) -> Option<Input> {
    match ev {
        Event::Key(k) => input_from(k),
        Event::Paste(text) => Some(Input::Paste(text)),
        _ => None,
    }
}

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
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::F(n @ 1..=12) => Key::F(n),
        _ => Key::Other,
    };
    Some(Input::Key(KeyPress {
        key,
        mods: Mods {
            ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
            alt: ev.modifiers.contains(KeyModifiers::ALT),
            shift: ev.modifiers.contains(KeyModifiers::SHIFT),
        },
    }))
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
            (KeyCode::Left, Key::Left),
            (KeyCode::Right, Key::Right),
            (KeyCode::F(5), Key::F(5)),
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
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Delete,
            KeyCode::PageUp,
            KeyCode::F(13),
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
        assert_eq!(
            press(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT),
            Some(Input::Key(KeyPress {
                key: Key::Up,
                mods: Mods {
                    ctrl: false,
                    alt: true,
                    shift: true
                }
            }))
        );
    }

    #[test]
    fn a_bracketed_paste_is_one_paste_input_and_other_events_are_dropped() {
        assert_eq!(
            input_from_event(Event::Paste("a\r\nb".into())),
            Some(Input::Paste("a\r\nb".into())),
            "the raw block; normalizing is the app's job"
        );
        assert_eq!(
            input_from_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            Some(Input::new(Key::Enter))
        );
        assert_eq!(input_from_event(Event::Resize(80, 24)), None);
        assert_eq!(input_from_event(Event::FocusGained), None);
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
            app.on_key(ev(KeyCode::Home, KeyModifiers::NONE, KeyEventKind::Release)),
            Action::None
        );
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }
}
