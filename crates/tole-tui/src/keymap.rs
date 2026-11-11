//! Key bindings (#463): which key triggers which convenience action.
//!
//! A [`Keymap`] is plain data the host builds (from the user config) and
//! hands to the app, exactly like the theme: this crate reads no file or env.
//! The default keymap is today's behavior.
//!
//! **What is not rebindable, structurally.** Only the closed set of
//! [`KeyAction`]s can be bound, and a user binding must pass [`check`]: a bare
//! character, Enter, Esc, Backspace, Tab, Ctrl+C and the keys a terminal
//! cannot tell apart are rejected at parse time. Dispatch is a second,
//! independent layer: the app consults the keymap only after the approval
//! prompt, the picker and Ctrl+C have taken their keys (see
//! `App::on_key_press`), so no keymap can intercept `y` / `n` / Esc / Ctrl+C.

use crate::{Key, KeyPress, Mods};

/// A rebindable action. Convenience only: sending, cancelling, typing and the
/// approval keys are not in this set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    ToggleTools,
    ToggleReasoning,
    ModelPicker,
    CycleModel,
    CycleThinking,
    Newline,
    QueueFollowUp,
    PopQueue,
    HistoryPrev,
    HistoryNext,
}

impl KeyAction {
    /// Every action, in table order.
    pub const ALL: [KeyAction; 10] = [
        KeyAction::ToggleTools,
        KeyAction::ToggleReasoning,
        KeyAction::ModelPicker,
        KeyAction::CycleModel,
        KeyAction::CycleThinking,
        KeyAction::Newline,
        KeyAction::QueueFollowUp,
        KeyAction::PopQueue,
        KeyAction::HistoryPrev,
        KeyAction::HistoryNext,
    ];

    /// The name used in `[tui.keys]`.
    pub fn name(self) -> &'static str {
        match self {
            KeyAction::ToggleTools => "toggle_tools",
            KeyAction::ToggleReasoning => "toggle_reasoning",
            KeyAction::ModelPicker => "model_picker",
            KeyAction::CycleModel => "cycle_model",
            KeyAction::CycleThinking => "cycle_thinking",
            KeyAction::Newline => "newline",
            KeyAction::QueueFollowUp => "queue_follow_up",
            KeyAction::PopQueue => "pop_queue",
            KeyAction::HistoryPrev => "history_prev",
            KeyAction::HistoryNext => "history_next",
        }
    }

    pub fn from_name(name: &str) -> Option<KeyAction> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }

    /// The built-in keys (today's behavior).
    fn default_keys(self) -> &'static [&'static str] {
        match self {
            KeyAction::ToggleTools => &["ctrl+o"],
            KeyAction::ToggleReasoning => &["ctrl+t"],
            KeyAction::ModelPicker => &["ctrl+l"],
            KeyAction::CycleModel => &["ctrl+p"],
            KeyAction::CycleThinking => &["shift+tab"],
            KeyAction::Newline => &["shift+enter", "ctrl+j"],
            KeyAction::QueueFollowUp => &["alt+enter"],
            KeyAction::PopQueue => &["alt+up"],
            KeyAction::HistoryPrev => &["up"],
            KeyAction::HistoryNext => &["down"],
        }
    }
}

/// One key with its modifiers. `shift+tab` is stored as [`Key::BackTab`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeySpec {
    key: Key,
    mods: Mods,
}

impl KeySpec {
    /// Does this press trigger the spec? Letters match case-insensitively and
    /// ignore Shift (a terminal reports Shift for an uppercase letter);
    /// Shift+Tab ignores Shift too; Ctrl and Alt always match exactly.
    fn matches(&self, p: &KeyPress) -> bool {
        let same_key = match (self.key, p.key) {
            (Key::Char(a), Key::Char(b)) => a == b.to_ascii_lowercase(),
            (a, b) => a == b,
        };
        let shift_free = matches!(self.key, Key::Char(_) | Key::BackTab);
        same_key
            && self.mods.ctrl == p.mods.ctrl
            && self.mods.alt == p.mods.alt
            && (shift_free || self.mods.shift == p.mods.shift)
    }

    /// `Ctrl+O`, `Alt+Enter`, `Shift+Tab`, `F5`.
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.mods.ctrl {
            s.push_str("Ctrl+");
        }
        if self.mods.alt {
            s.push_str("Alt+");
        }
        if self.mods.shift {
            s.push_str("Shift+");
        }
        match self.key {
            Key::Char(c) => s.push(c.to_ascii_uppercase()),
            Key::Enter => s.push_str("Enter"),
            Key::BackTab => s.push_str("Shift+Tab"),
            Key::Up => s.push_str("Up"),
            Key::Down => s.push_str("Down"),
            Key::Left => s.push_str("Left"),
            Key::Right => s.push_str("Right"),
            Key::F(n) => s.push_str(&format!("F{n}")),
            Key::Tab => s.push_str("Tab"),
            Key::Esc => s.push_str("Esc"),
            Key::Backspace => s.push_str("Backspace"),
            Key::Other => s.push('?'),
        }
        s
    }
}

/// Parse `ctrl+o`, `Alt+Up`, `shift+tab`, `f5` (case-insensitive) without
/// the binding rules; those are [`check`].
fn parse(spec: &str) -> Result<KeySpec, String> {
    let lower = spec.trim().to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').map(str::trim).collect();
    let name = parts.pop().unwrap_or("");
    let mut mods = Mods::default();
    for m in parts {
        let flag = match m {
            "ctrl" => &mut mods.ctrl,
            "alt" => &mut mods.alt,
            "shift" => &mut mods.shift,
            _ => return Err(format!("unknown modifier `{m}` (use ctrl, alt or shift)")),
        };
        if std::mem::replace(flag, true) {
            return Err(format!("modifier `{m}` is repeated"));
        }
    }
    let key = match name {
        "" => return Err("no key (expected e.g. `ctrl+o`)".into()),
        "enter" | "return" => Key::Enter,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "tab" if mods.shift && !mods.ctrl && !mods.alt => {
            mods.shift = false;
            Key::BackTab
        }
        "tab" => {
            return Err("Tab completes menus: only `shift+tab` can be bound".into());
        }
        "esc" | "escape" | "backspace" => {
            return Err(format!("`{name}` is protected and cannot be bound"));
        }
        n if n.len() >= 2 && n.starts_with('f') && n[1..].chars().all(|c| c.is_ascii_digit()) => {
            match n[1..].parse::<u8>() {
                Ok(i @ 1..=12) => Key::F(i),
                _ => return Err(format!("`{n}` is not a function key (f1 to f12)")),
            }
        }
        n if n.chars().count() == 1 => {
            let c = n.chars().next().unwrap_or(' ');
            if !c.is_ascii_alphanumeric() {
                return Err(format!(
                    "`{n}` is not supported: terminals cannot tell punctuation keys apart \
                     (Ctrl+[ is Esc, Ctrl+\\ is a signal key); use a letter, digit or function key"
                ));
            }
            Key::Char(c)
        }
        n => {
            return Err(format!(
                "unknown key `{n}` (a letter or digit, enter, tab, up, down, left, right, f1 to f12)"
            ))
        }
    };
    Ok(KeySpec { key, mods })
}

/// The binding rules for a USER-supplied key (the built-in defaults skip
/// them: bare Up/Down are the history defaults).
fn check(action: KeyAction, k: &KeySpec) -> Result<(), String> {
    let m = k.mods;
    let modified = m.ctrl || m.alt || m.shift;
    match k.key {
        Key::Char('c') if m.ctrl => Err("Ctrl+C cancels and quits and is protected".into()),
        Key::Char(c) if m.ctrl && matches!(c, 'i' | 'm' | 'h') => Err(format!(
            "Ctrl+{} is sent as {} by terminals and cannot be told apart",
            c.to_ascii_uppercase(),
            match c {
                'i' => "Tab",
                'm' => "Enter",
                _ => "Backspace",
            }
        )),
        Key::Char(c) if m.ctrl && matches!(c, 's' | 'q' | 'z') => Err(format!(
            "Ctrl+{} is a terminal flow-control / suspend key and is not rebindable",
            c.to_ascii_uppercase()
        )),
        Key::Char(c) if m.ctrl && c.is_ascii_digit() => Err(
            "terminals do not report Ctrl+digit distinctly (Ctrl+4..7 are Ctrl+\\ ] ^ _); \
             use a letter, or Alt+digit"
                .into(),
        ),
        Key::Char(_) if m.shift => {
            Err("Shift+letter is not supported (a terminal reports it as the capital); use ctrl+ or alt+".into())
        }
        Key::Char(_) if !m.ctrl && !m.alt => Err(
            "a bare character is typing and cannot be bound; use ctrl+, alt+ or a function key"
                .into(),
        ),
        Key::Enter if !modified => Err("Enter sends the message and is protected".into()),
        Key::Up | Key::Down | Key::Left | Key::Right if !modified => {
            // Plain Up/Down are the history defaults; every other action
            // needs a modifier so menus and pickers keep their arrows.
            let history = (k.key == Key::Up && action == KeyAction::HistoryPrev)
                || (k.key == Key::Down && action == KeyAction::HistoryNext);
            if history {
                Ok(())
            } else {
                Err("a bare arrow key navigates; add ctrl+, alt+ or shift+".into())
            }
        }
        _ => Ok(()),
    }
}

/// Key -> action table. [`Keymap::default`] is today's behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    binds: Vec<(KeyAction, Vec<KeySpec>)>,
}

impl Default for Keymap {
    fn default() -> Self {
        let binds = KeyAction::ALL
            .into_iter()
            .map(|a| {
                let keys = a.default_keys().iter().filter_map(|s| parse(s).ok());
                (a, keys.collect())
            })
            .collect();
        Keymap { binds }
    }
}

impl Keymap {
    /// The default keymap with `overrides` (action name, key specs) applied.
    /// Actions not listed keep their default keys. Any problem (unknown
    /// action, bad spec, protected or ambiguous key, a key bound to two
    /// actions) is an `Err` naming it; the caller then uses
    /// [`Keymap::default`] and shows the message once.
    pub fn from_overrides(overrides: &[(String, Vec<String>)]) -> Result<Keymap, String> {
        let mut map = Keymap::default();
        for (name, specs) in overrides {
            let action = KeyAction::from_name(name).ok_or_else(|| {
                let all: Vec<&str> = KeyAction::ALL.iter().map(|a| a.name()).collect();
                format!("tui.keys: unknown action {name:?} (actions: {})", all.join(", "))
            })?;
            if specs.is_empty() {
                return Err(format!("tui.keys.{name}: needs at least one key"));
            }
            let mut keys = Vec::new();
            for s in specs {
                let k = parse(s)
                    .and_then(|k| check(action, &k).map(|()| k))
                    .map_err(|e| format!("tui.keys.{name}: {s:?}: {e}"))?;
                keys.push(k);
            }
            if let Some(slot) = map.binds.iter_mut().find(|(a, _)| *a == action) {
                slot.1 = keys;
            }
        }
        // A key may trigger one action only (this also catches a rebinding
        // that lands on another action's untouched default).
        let mut seen: Vec<(KeySpec, KeyAction)> = Vec::new();
        for (action, keys) in &map.binds {
            for k in keys {
                if let Some((_, other)) = seen.iter().find(|(s, _)| s == k) {
                    return Err(format!(
                        "tui.keys: {} is bound to both `{}` and `{}`",
                        k.label(),
                        other.name(),
                        action.name()
                    ));
                }
                seen.push((*k, *action));
            }
        }
        Ok(map)
    }

    /// The action a key press triggers, if any.
    pub fn action(&self, p: &KeyPress) -> Option<KeyAction> {
        self.binds
            .iter()
            .find(|(_, keys)| keys.iter().any(|k| k.matches(p)))
            .map(|(a, _)| *a)
    }

    /// The keys bound to `action`.
    pub fn keys(&self, action: KeyAction) -> &[KeySpec] {
        self.binds
            .iter()
            .find(|(a, _)| *a == action)
            .map_or(&[], |(_, k)| k)
    }

    /// The first key of `action` as a hint (`Ctrl+L`).
    pub fn label(&self, action: KeyAction) -> String {
        self.keys(action)
            .first()
            .map_or_else(|| "(unbound)".into(), KeySpec::label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, ctrl: bool, alt: bool, shift: bool) -> KeyPress {
        KeyPress {
            key,
            mods: Mods { ctrl, alt, shift },
        }
    }

    fn over(items: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        items
            .iter()
            .map(|(a, k)| (a.to_string(), k.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn err(items: &[(&str, &[&str])]) -> String {
        Keymap::from_overrides(&over(items)).unwrap_err()
    }

    /// Today's keys, one row per action: pin the default keymap.
    #[test]
    fn the_default_keymap_is_todays_behavior() {
        use Key::*;
        let k = Keymap::default();
        let table: [(KeyAction, KeyPress); 11] = [
            (KeyAction::ToggleTools, press(Char('o'), true, false, false)),
            (KeyAction::ToggleReasoning, press(Char('t'), true, false, false)),
            (KeyAction::ModelPicker, press(Char('l'), true, false, false)),
            (KeyAction::CycleModel, press(Char('p'), true, false, false)),
            (KeyAction::CycleThinking, press(BackTab, false, false, true)),
            (KeyAction::Newline, press(Enter, false, false, true)),
            (KeyAction::Newline, press(Char('j'), true, false, false)),
            (KeyAction::QueueFollowUp, press(Enter, false, true, false)),
            (KeyAction::PopQueue, press(Up, false, true, false)),
            (KeyAction::HistoryPrev, press(Up, false, false, false)),
            (KeyAction::HistoryNext, press(Down, false, false, false)),
        ];
        for (action, p) in table {
            assert_eq!(k.action(&p), Some(action), "{action:?}");
        }
        // BackTab without the Shift flag is the same key.
        assert_eq!(
            k.action(&press(BackTab, false, false, false)),
            Some(KeyAction::CycleThinking)
        );
        // Letters are case-insensitive (a terminal may report Ctrl+Shift+O).
        assert_eq!(
            k.action(&press(Char('O'), true, false, true)),
            Some(KeyAction::ToggleTools)
        );
        // Keys that are not bound, and the protected ones, trigger nothing.
        for p in [
            press(Enter, false, false, false),
            press(Esc, false, false, false),
            press(Backspace, false, false, false),
            press(Char('y'), false, false, false),
            press(Char('o'), false, false, false),
            press(Char('o'), true, true, false),
            press(Char('c'), true, false, false),
        ] {
            assert_eq!(k.action(&p), None, "{p:?}");
        }
        assert_eq!(
            KeyAction::ALL.map(|a| k.keys(a).len()),
            [1, 1, 1, 1, 1, 2, 1, 1, 1, 1]
        );
        // Hints for the default keys are the texts the footer always had.
        assert_eq!(k.label(KeyAction::ModelPicker), "Ctrl+L");
        assert_eq!(k.label(KeyAction::CycleThinking), "Shift+Tab");
        assert_eq!(k.label(KeyAction::Newline), "Shift+Enter");
        assert_eq!(k.label(KeyAction::QueueFollowUp), "Alt+Enter");
        assert_eq!(k.label(KeyAction::PopQueue), "Alt+Up");
        assert_eq!(k.label(KeyAction::ToggleTools), "Ctrl+O");
    }

    #[test]
    fn a_rebinding_replaces_the_old_keys_and_keeps_the_other_defaults() {
        let k = Keymap::from_overrides(&over(&[
            ("toggle_tools", &["alt+o", "F5"]),
            ("history_prev", &["up", "ctrl+k"]),
        ]))
        .unwrap();
        assert_eq!(
            k.action(&press(Key::Char('o'), false, true, false)),
            Some(KeyAction::ToggleTools)
        );
        assert_eq!(
            k.action(&press(Key::F(5), false, false, false)),
            Some(KeyAction::ToggleTools)
        );
        assert_eq!(k.action(&press(Key::Char('o'), true, false, false)), None);
        assert_eq!(
            k.action(&press(Key::Char('k'), true, false, false)),
            Some(KeyAction::HistoryPrev)
        );
        assert_eq!(
            k.action(&press(Key::Char('l'), true, false, false)),
            Some(KeyAction::ModelPicker)
        );
        assert_eq!(k.label(KeyAction::ToggleTools), "Alt+O");
        assert_eq!(Keymap::from_overrides(&[]).unwrap(), Keymap::default());
    }

    #[test]
    fn specs_are_case_insensitive_and_modifier_order_free() {
        for s in ["Ctrl+Alt+X", "alt+ctrl+x", " ALT + CTRL + x "] {
            let k = Keymap::from_overrides(&over(&[("cycle_model", &[s])])).unwrap();
            assert_eq!(
                k.action(&press(Key::Char('x'), true, true, false)),
                Some(KeyAction::CycleModel),
                "{s}"
            );
        }
        let k = Keymap::from_overrides(&over(&[("newline", &["Ctrl+Enter"])])).unwrap();
        assert_eq!(
            k.action(&press(Key::Enter, true, false, false)),
            Some(KeyAction::Newline)
        );
        let k = Keymap::from_overrides(&over(&[("cycle_thinking", &["alt+Left"])])).unwrap();
        assert_eq!(
            k.action(&press(Key::Left, false, true, false)),
            Some(KeyAction::CycleThinking)
        );
    }

    #[test]
    fn bare_characters_and_protected_keys_are_rejected() {
        for s in [
            "o", "y", "n", "/", "@", "9", "shift+o", "enter", "esc", "escape", "backspace",
            "alt+esc", "ctrl+backspace", "tab", "ctrl+tab", "alt+tab", "ctrl+c", "Ctrl+C",
            "up", "down", "left", "right", "ctrl+/",
        ] {
            assert!(
                Keymap::from_overrides(&over(&[("toggle_tools", &[s])])).is_err(),
                "`{s}` must be rejected"
            );
        }
        // Bare Up / Down only for the history pair.
        assert!(Keymap::from_overrides(&over(&[("history_next", &["up"])])).is_err());
        assert!(Keymap::from_overrides(&over(&[("history_prev", &["down"])])).is_err());
        // Function keys need no modifier.
        assert!(Keymap::from_overrides(&over(&[("toggle_tools", &["f12"])])).is_ok());
        assert!(Keymap::from_overrides(&over(&[("toggle_tools", &["f13"])])).is_err());
        assert!(Keymap::from_overrides(&over(&[("toggle_tools", &["f0"])])).is_err());
    }

    #[test]
    fn keys_a_terminal_cannot_tell_apart_are_rejected() {
        for (s, why) in [
            ("ctrl+i", "Tab"),
            ("ctrl+m", "Enter"),
            ("ctrl+h", "Backspace"),
            ("ctrl+[", "Esc"),
            ("ctrl+s", "flow-control"),
            ("ctrl+q", "flow-control"),
            ("ctrl+z", "flow-control"),
            ("ctrl+\\", "signal"),
            ("ctrl+5", "Ctrl+digit"),
        ] {
            let e = err(&[("toggle_tools", &[s])]);
            assert!(e.contains(why) && e.contains("toggle_tools"), "{s}: {e}");
        }
        // Ctrl+J is allowed: it is the newline fallback.
        assert!(Keymap::from_overrides(&over(&[("newline", &["ctrl+j"])])).is_ok());
    }

    #[test]
    fn a_bad_spec_names_the_action_and_the_spec() {
        for s in ["", "ctrl+", "hyper+o", "ctrl+ctrl+o", "ctrl+foo", "ctrl++"] {
            let e = err(&[("cycle_model", &[s])]);
            assert!(e.starts_with("tui.keys.cycle_model: "), "{s}: {e}");
            assert!(e.contains(&format!("{s:?}")), "{s}: {e}");
        }
        let e = err(&[("flip", &["ctrl+o"])]);
        assert!(e.contains("unknown action \"flip\"") && e.contains("toggle_tools"), "{e}");
        let e = err(&[("toggle_tools", &[])]);
        assert!(e.contains("toggle_tools") && e.contains("at least one key"), "{e}");
    }

    #[test]
    fn a_key_on_two_actions_is_rejected_naming_both() {
        // Explicit twice.
        let e = err(&[("toggle_tools", &["ctrl+x"]), ("cycle_model", &["ctrl+x"])]);
        assert!(
            e.contains("Ctrl+X") && e.contains("`toggle_tools`") && e.contains("`cycle_model`"),
            "{e}"
        );
        // Onto another action's untouched default.
        let e = err(&[("cycle_model", &["ctrl+o"])]);
        assert!(e.contains("`toggle_tools`") && e.contains("`cycle_model`"), "{e}");
        // Swapping two defaults is fine.
        assert!(Keymap::from_overrides(&over(&[
            ("toggle_tools", &["ctrl+t"]),
            ("toggle_reasoning", &["ctrl+o"]),
        ]))
        .is_ok());
        // One action listing the same key twice.
        let e = err(&[("toggle_tools", &["ctrl+x", "CTRL+X"])]);
        assert!(e.contains("`toggle_tools`"), "{e}");
    }
}
