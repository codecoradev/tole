use crate::support::*;
use tole_cli::config::apply::*;
use tole_cli::config::Config;
use tole_cli::config::CONFIG_REL_PATH;

// -------------------------------------------------------- tui theme (#463)

/// The user layer carrying `user`, layered under `project` exactly as
/// startup does.
fn layered_cfg(project: &str, user: &str) -> Config {
    tole_cli::config::layered(&cfg(project), &cfg(user))
}

fn theme_of(c: &Config, env: &[(&str, &str)]) -> Eff<String> {
    Settings::resolve(&Flags::default(), c, &env_of(env)).tui_theme
}

#[test]
fn theme_precedence_env_then_user_config_then_no_color_then_default() {
    let c = layered_cfg("", "[tui]\ntheme = \"light\"\n");
    // env TOLE_THEME beats the user config (and NO_COLOR).
    let e = theme_of(&c, &[("TOLE_THEME", "mono"), ("NO_COLOR", "1")]);
    assert_eq!((e.value.as_str(), e.source), ("mono", Source::Env));
    assert_eq!(e.describe(), "(env TOLE_THEME)");
    // The user config beats NO_COLOR.
    let e = theme_of(&c, &[("NO_COLOR", "1")]);
    assert_eq!((e.value.as_str(), e.source), ("light", Source::UserConfig));
    // NO_COLOR beats the default.
    let none = layered_cfg("", "");
    let e = theme_of(&none, &[("NO_COLOR", "1")]);
    assert_eq!((e.value.as_str(), e.source), ("mono", Source::Env));
    assert_eq!(e.describe(), "(env NO_COLOR)");
    // An EMPTY NO_COLOR does not count; nor does a blank TOLE_THEME. Any
    // other non-empty value does (even "0").
    assert_eq!(theme_of(&none, &[("NO_COLOR", "")]).source, Source::Default);
    assert_eq!(theme_of(&none, &[("NO_COLOR", "0")]).value, "mono");
    let e = theme_of(&none, &[("TOLE_THEME", "  ")]);
    assert_eq!((e.value.as_str(), e.source), ("default", Source::Default));
}

#[test]
fn a_project_config_cannot_set_the_theme_or_colors() {
    let project = "[tui]\ntheme = \"mono\"\n[tui.colors]\napproval = \"black\"\n";
    // The file parses, but layering drops it: nothing reaches Settings.
    let c = layered_cfg(project, "");
    assert_eq!(c.tui, None);
    let st = Settings::resolve(&Flags::default(), &c, &env_of(&[]));
    assert_eq!(st.tui_theme.source, Source::Default);
    assert_eq!(st.tui_theme.value, "default");
    assert!(st.tui_colors.is_none());
    // With a user theme, the user wins outright (no key-by-key merge).
    let c = layered_cfg(project, "[tui]\ntheme = \"light\"\n");
    let st = Settings::resolve(&Flags::default(), &c, &env_of(&[]));
    assert_eq!(st.tui_theme.value, "light");
    assert!(st.tui_colors.is_none(), "project colors must not merge in");
}

#[cfg(feature = "tui")]
#[test]
fn an_invalid_theme_falls_back_to_default_with_a_note_naming_the_key() {
    use tole_tui::{parse_spec, Role, Theme};
    let theme = |user: &str, env: &[(&str, &str)]| {
        let c = layered_cfg("", user);
        Settings::resolve(&Flags::default(), &c, &env_of(env)).theme()
    };
    assert_eq!(theme("", &[]), (Theme::default(), None));
    assert_eq!(
        theme("[tui]\ntheme = \"light\"\n", &[]),
        (Theme::named("light").unwrap(), None)
    );
    // Unknown theme name in the user config.
    let (t, note) = theme("[tui]\ntheme = \"neon\"\n", &[]);
    assert_eq!(t, Theme::default());
    let note = note.unwrap();
    assert!(
        note.contains("tui.theme") && note.contains("neon"),
        "{note}"
    );
    assert!(!note.contains('\n'), "one line");
    // Unknown theme name in the env names the variable.
    let (_, note) = theme("", &[("TOLE_THEME", "neon")]);
    assert!(note.unwrap().contains("TOLE_THEME"));
    // A bad color names the key and the role; the theme is the default (the
    // valid overrides of the same table are not applied either).
    let user = "[tui]\ntheme = \"light\"\n[tui.colors]\nnote = \"red\"\ntool = \"ultraviolet\"\n";
    let (t, note) = theme(user, &[]);
    assert_eq!(t, Theme::default());
    let note = note.unwrap();
    assert!(
        note.contains("tui.colors") && note.contains("`tool`") && note.contains("ultraviolet"),
        "{note}"
    );
    // A background or conceal request is a refusal like any other.
    for spec in ["bg:red", "hidden"] {
        let user = format!("[tui.colors]\nnote = \"{spec}\"\n");
        let (t, note) = theme(&user, &[]);
        assert_eq!(t, Theme::default(), "{spec}");
        assert!(note.unwrap().contains("`note`"), "{spec}");
    }
    // Valid overrides apply on top of the chosen theme.
    let (t, note) = theme("[tui.colors]\ntool = \"#102030 bold\"\n", &[]);
    assert_eq!(note, None);
    assert_eq!(t.get(Role::Tool), parse_spec("#102030 bold").unwrap());
}

#[test]
fn config_check_shows_the_effective_theme_and_ignores_a_project_theme() {
    let user = tmpdir();
    let up = user.join("config.toml");
    std::fs::write(&up, "[tui]\ntheme = \"light\"\n").unwrap();
    let dir = tmpdir();
    let p = dir.join(CONFIG_REL_PATH);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "[tui]\ntheme = \"mono\"\n").unwrap();
    let out = check(&dir, None, Some(&up), &Flags::default(), &env_of(&[])).unwrap();
    #[cfg(feature = "tui")]
    assert!(
        out.contains("tui.theme = \"light\"  [user config]  effective: \"light\"  (user config)"),
        "{out}"
    );
    // A build without the TUI cannot validate a theme: it says so and never
    // reports an effective value or a verdict.
    #[cfg(not(feature = "tui"))]
    assert!(
        out.contains("tui.theme = \"light\"  [user config]  (not validated: this build has no TUI")
            && !out.contains("effective: \"light\""),
        "{out}"
    );
    assert!(
        out.contains("tui.theme = \"mono\"  [config]  IGNORED: a project config cannot set"),
        "{out}"
    );
    #[cfg(feature = "tui")]
    {
        let out = check(
            &dir,
            None,
            Some(&up),
            &Flags::default(),
            &env_of(&[("TOLE_THEME", "mono")]),
        )
        .unwrap();
        assert!(
            out.contains("effective: \"mono\"  (env TOLE_THEME)"),
            "{out}"
        );
    }
    // An invalid value is flagged in the check output.
    std::fs::write(&up, "[tui]\ntheme = \"neon\"\n").unwrap();
    let out = check(&dir, None, Some(&up), &Flags::default(), &env_of(&[])).unwrap();
    #[cfg(feature = "tui")]
    assert!(out.contains("INVALID: tui.theme = \"neon\""), "{out}");
    #[cfg(not(feature = "tui"))]
    assert!(
        out.contains("not validated") && !out.contains("INVALID"),
        "{out}"
    );
}

#[test]
fn config_check_shows_effective_effort_and_source() {
    let dir = tmpdir();
    let p = dir.join(CONFIG_REL_PATH);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "reasoning_effort = \"medium\"\n").unwrap();
    let out = check(
        &dir,
        None,
        None,
        &Flags::default(),
        &env_of(&[("TOLE_REASONING_EFFORT", "low")]),
    )
    .unwrap();
    assert!(
        out.contains("reasoning_effort = \"medium\"  [config]  effective: \"low\"  (env TOLE_REASONING_EFFORT)"),
        "{out}"
    );
}

// -------------------------------------------------------- tui keys (#463)

fn keys_of(c: &Config) -> Option<std::collections::BTreeMap<String, Vec<String>>> {
    Settings::resolve(&Flags::default(), c, &env_of(&[]))
        .tui_keys
        .map(|e| e.value)
}

#[test]
fn a_project_config_cannot_set_keys_and_the_user_layer_can() {
    let project = "[tui.keys]\ntoggle_tools = \"f5\"\n";
    // Layering drops the project table: no keys reach Settings.
    let c = layered_cfg(project, "");
    assert_eq!(c.tui, None);
    assert_eq!(keys_of(&c), None);
    // The user layer supplies them, as a string or a list, and only it.
    let c = layered_cfg(
        project,
        "[tui.keys]\ntoggle_tools = [\"alt+o\", \"f6\"]\nnewline = \"alt+n\"\n",
    );
    let k = keys_of(&c).unwrap();
    assert_eq!(k["toggle_tools"], vec!["alt+o", "f6"]);
    assert_eq!(k["newline"], vec!["alt+n"]);
    let st = Settings::resolve(&Flags::default(), &c, &env_of(&[]));
    assert_eq!(st.tui_keys.unwrap().source, Source::UserConfig);
}

#[cfg(feature = "tui")]
#[test]
fn an_invalid_keymap_falls_back_to_default_with_one_note_naming_the_key() {
    use tole_tui::Keymap;
    let keymap = |user: &str| {
        let c = layered_cfg("", user);
        Settings::resolve(&Flags::default(), &c, &env_of(&[])).keymap()
    };
    assert_eq!(keymap(""), (Keymap::default(), None));
    let (k, note) = keymap("[tui.keys]\ntoggle_tools = \"alt+o\"\n");
    assert_eq!(note, None);
    assert_ne!(k, Keymap::default());
    for (user, needles) in [
        (
            "[tui.keys]\nnope = \"ctrl+o\"\n",
            &["unknown action", "nope"][..],
        ),
        (
            "[tui.keys]\ncycle_model = \"ctrl+q\"\n",
            &["cycle_model", "ctrl+q"][..],
        ),
        (
            "[tui.keys]\ncycle_model = \"y\"\n",
            &["cycle_model", "bare character"][..],
        ),
        (
            "[tui.keys]\ncycle_model = \"esc\"\n",
            &["cycle_model", "protected"][..],
        ),
        (
            "[tui.keys]\ncycle_model = \"ctrl+x\"\ntoggle_tools = \"ctrl+x\"\n",
            &["cycle_model", "toggle_tools", "Ctrl+X"][..],
        ),
        // A valid binding next to a bad one: the WHOLE keymap is the default.
        (
            "[tui.keys]\nnewline = \"alt+n\"\ncycle_model = \"ctrl+i\"\n",
            &["cycle_model", "Tab"][..],
        ),
    ] {
        let (k, note) = keymap(user);
        assert_eq!(k, Keymap::default(), "{user}");
        let note = note.unwrap();
        assert!(note.starts_with("keys: ") && !note.contains('\n'), "{note}");
        assert!(note.ends_with("using the default keys"), "{note}");
        for n in needles {
            assert!(note.contains(n), "{user}: {note}");
        }
    }
}

#[test]
fn config_check_shows_keys_ignores_a_project_table_and_flags_a_bad_one() {
    let user = tmpdir();
    let up = user.join("config.toml");
    std::fs::write(&up, "[tui.keys]\ntoggle_tools = [\"alt+o\", \"f6\"]\n").unwrap();
    let dir = tmpdir();
    let p = dir.join(CONFIG_REL_PATH);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "[tui.keys]\nnewline = \"f5\"\n").unwrap();
    let out = check(&dir, None, Some(&up), &Flags::default(), &env_of(&[])).unwrap();
    #[cfg(feature = "tui")]
    assert!(
        out.contains("tui.keys = [1 entries]  [user config]  effective: 1 action(s) rebound")
            && out.contains("(user config)")
            && out.contains("  - toggle_tools = [\"alt+o\", \"f6\"]"),
        "{out}"
    );
    #[cfg(not(feature = "tui"))]
    assert!(
        out.contains(
            "tui.keys = [1 entries]  [user config]  (not validated: this build has no TUI"
        ) && !out.contains("effective: 1 action"),
        "{out}"
    );
    assert!(
        out.contains("tui.keys = [1 entries]  [config]  IGNORED: a project config cannot set"),
        "{out}"
    );
    std::fs::write(&up, "[tui.keys]\ntoggle_tools = \"o\"\n").unwrap();
    let out = check(&dir, None, Some(&up), &Flags::default(), &env_of(&[])).unwrap();
    #[cfg(feature = "tui")]
    assert!(
        out.contains("INVALID: tui.keys.toggle_tools: \"o\": a bare character")
            && out.contains("the default keys are used"),
        "{out}"
    );
    #[cfg(not(feature = "tui"))]
    assert!(
        out.contains("not validated") && !out.contains("INVALID"),
        "{out}"
    );
}
