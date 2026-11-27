//! Themes and keybindings on the real binary.

use crate::mock::spawn_mock;
use crate::pty::{printed, workdir, Pty};
use std::path::Path;
/// Every SGR parameter in `seen` that selects a foreground or background
/// color (`30-38`, `40-48`, `90-97`, `100-107`); `39`/`49` only reset.
fn sgr_colors(seen: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = seen;
    while let Some(i) = rest.find("\x1b[") {
        rest = &rest[i + 2..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == ';' || c == ':'))
            .unwrap_or(rest.len());
        if rest[end..].starts_with('m') {
            for p in rest[..end].split([';', ':']) {
                match p.parse::<u16>() {
                    Ok(38 | 48) => {
                        found.push(rest[..end].to_string());
                        break; // extended color: the rest are its operands
                    }
                    Ok(30..=37 | 40..=47 | 90..=97 | 100..=107) => found.push(p.to_string()),
                    _ => {}
                }
            }
        }
    }
    found
}

/// One approval prompt, denied, then quit; returns everything printed.
fn approval_screen(envs: &[(&str, &str)]) -> String {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn_full(&dir, &url, &[], envs);
    t.expect("effort:");
    t.send("write it\r");
    t.expect("create the file out.txt");
    t.send("n");
    t.expect("approval denied");
    t.idle();
    t.send("/");
    t.expect("/thinking");
    t.send("\x7f/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    String::from_utf8_lossy(&o).into_owned()
}

/// #463 themes: `TOLE_THEME=mono` (and `NO_COLOR`) leave no color sequence on
/// the screen, the default theme still colors it, and an unknown theme falls
/// back to the default with one note.
#[test]
fn tui_mono_theme_emits_no_color_sequences_and_default_still_does() {
    let default = approval_screen(&[]);
    assert!(
        !sgr_colors(&default).is_empty(),
        "the default theme must color the screen: {default:?}"
    );
    for envs in [
        &[("TOLE_THEME", "mono")][..],
        &[("NO_COLOR", "1")][..],
        &[("NO_COLOR", "1"), ("TOLE_THEME", "MONO")][..],
    ] {
        let mono = approval_screen(envs);
        assert_eq!(sgr_colors(&mono), Vec::<String>::new(), "{envs:?}");
        assert!(!mono.contains("\x1b[38;"), "{envs:?}");
        // Still readable: the approval prompt is on screen, and bold.
        assert!(mono.contains("create the file out.txt"), "{mono:?}");
        assert!(
            mono.contains("\x1b[1m"),
            "bold carries the prompt: {mono:?}"
        );
    }
    // An explicit theme is selected over NO_COLOR, but crossterm (the
    // terminal backend) itself drops color escapes while NO_COLOR is set, so
    // the screen stays color-free and the prompt stays bold.
    let both = approval_screen(&[("NO_COLOR", "1"), ("TOLE_THEME", "light")]);
    assert_eq!(sgr_colors(&both), Vec::<String>::new(), "{both:?}");
    assert!(both.contains("\x1b[1m"), "{both:?}");
    // Without NO_COLOR the light theme colors the screen.
    let light = approval_screen(&[("TOLE_THEME", "light")]);
    assert!(!sgr_colors(&light).is_empty(), "{light:?}");
}

#[test]
fn tui_unknown_theme_shows_one_note_and_uses_the_default() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn_full(&dir, &url, &[], &[("TOLE_THEME", "neon")]);
    t.expect("TOLE_THEME = \"neon\" is not a theme");
    t.expect("effort:");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    let seen = String::from_utf8_lossy(&o).into_owned();
    assert_eq!(seen.matches("is not a theme").count(), 1, "{seen:?}");
    assert!(!sgr_colors(&seen).is_empty(), "default theme colors");
}

/// The user config (`$CODECORA_HOME/tole/config.toml`; the pty fixture sets
/// `CODECORA_HOME` to `<cwd>.home`, outside the workspace, #684).
fn write_user_config(dir: &Path, body: &str) {
    let home = dir.with_extension("home").join("tole");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), body).unwrap();
}

/// #463 keybindings: with a user config that rebinds two actions the new keys
/// act, the old key does not, and the footer hint names the new key.
#[test]
fn tui_user_keybinding_acts_on_the_new_key_and_not_the_old_one() {
    let (url, dir) = (spawn_mock(), workdir());
    write_user_config(
        &dir,
        "[tui.keys]\ntoggle_tools = \"alt+o\"\nmodel_picker = \"f4\"\n",
    );
    let mut t = Pty::spawn(&dir, &url);
    // The hint is built from the effective binding, and the idle marker the
    // other tests rely on is unchanged (cycle_thinking keeps its default).
    t.expect("F4 model");
    t.idle();

    // The old keys do nothing: Ctrl+L must not open the picker (it would
    // swallow the next keys) and Ctrl+O must not toggle: two Alt+O presses
    // give exactly two notes, "expanded" then "collapsed".
    t.send("\x0c\x0f");
    t.send("\x1bo"); // Alt+O
    t.expect("(tool output expanded)");
    t.send("\x1bo");
    t.expect("(tool output collapsed)");
    let seen = printed(&t);
    assert_eq!(seen.matches("(tool output").count(), 2, "{seen:?}");
    assert!(!seen.contains("model>"), "Ctrl+L opened the picker");

    // F4 (xterm: ESC O S) opens the picker.
    t.send("\x1bOS");
    t.expect("model>");
    t.send("\x1b");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
}

/// An invalid keymap never aborts the TUI: it starts with the default keys
/// and shows ONE note naming the key and the problem.
#[test]
fn tui_invalid_keymap_falls_back_to_default_with_one_note() {
    let (url, dir) = (spawn_mock(), workdir());
    write_user_config(
        &dir,
        "[tui.keys]\ntoggle_tools = \"o\"\nmodel_picker = \"f4\"\n",
    );
    let mut t = Pty::spawn(&dir, &url);
    t.expect("tui.keys.toggle_tools");
    t.expect("using the default keys");
    t.idle();
    // The whole keymap is the default: Ctrl+O toggles, and the valid
    // `model_picker = f4` of the same table was not applied either.
    t.send("\x0f");
    t.expect("(tool output expanded)");
    t.send("\x1bOS\x0f"); // F4 is not bound: it must not open the picker
    t.expect("(tool output collapsed)");
    assert!(!printed(&t).contains("model>"), "F4 opened the picker");
    t.send("\x0c");
    t.expect("model>");
    t.send("\x1b");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let seen = printed(&t);
    assert_eq!(seen.matches("tui.keys.toggle_tools").count(), 1, "{seen:?}");
    assert!(seen.contains("Ctrl+L model"), "{seen:?}");
}
