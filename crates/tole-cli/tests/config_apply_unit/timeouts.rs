use crate::support::*;
use std::path::Path;
use tole_cli::config::apply::*;
use tole_cli::config::Config;
use tole_cli::config::CONFIG_REL_PATH;

// ------------------------------------------- discussion timeout (#786)

fn timeout_of(c: &Config, env: &[(&str, &str)]) -> Settings {
    Settings::resolve(&Flags::default(), c, &env_of(env))
}

#[test]
fn discussion_timeout_env_beats_user_config_beats_default() {
    let user = cfg("[discussions]\nrequest_timeout_secs = 300\n");
    let c = tole_cli::config::layered(&Config::default(), &user);
    let st = timeout_of(&c, &[("TOLE_DISCUSSION_TIMEOUT_SECS", " 600 ")]);
    assert_eq!(st.request_timeout.value, 600);
    assert_eq!(st.request_timeout.source, Source::Env);
    assert_eq!(
        st.request_timeout.describe(),
        "(env TOLE_DISCUSSION_TIMEOUT_SECS)"
    );
    let st = timeout_of(&c, &[]);
    assert_eq!(st.request_timeout.value, 300);
    assert_eq!(st.request_timeout.source, Source::UserConfig);
    let st = timeout_of(&Config::default(), &[]);
    assert_eq!(st.request_timeout.value, 120, "the unchanged default");
    assert_eq!(st.request_timeout.source, Source::Default);
    assert_eq!(st.discussion_timeout_error(), None);
    // A blank env counts as unset.
    let st = timeout_of(&c, &[("TOLE_DISCUSSION_TIMEOUT_SECS", "  ")]);
    assert_eq!(st.request_timeout.value, 300);
    assert_eq!(st.discussion_timeout_error(), None);
}

#[test]
fn an_invalid_discussion_timeout_env_is_an_error_naming_the_variable() {
    for bad in ["abc", "0", "-5", "1.5", "3601", "99999999999999999999"] {
        let st = timeout_of(&Config::default(), &[("TOLE_DISCUSSION_TIMEOUT_SECS", bad)]);
        let msg = st
            .discussion_timeout_error()
            .unwrap_or_else(|| panic!("{bad:?} must be an error"));
        assert!(
            msg.contains("TOLE_DISCUSSION_TIMEOUT_SECS")
                && msg.contains(bad)
                && msg.contains("1 to 3600"),
            "{msg}"
        );
    }
    // The bounds themselves are valid.
    for good in ["1", "3600"] {
        let st = timeout_of(
            &Config::default(),
            &[("TOLE_DISCUSSION_TIMEOUT_SECS", good)],
        );
        assert_eq!(st.discussion_timeout_error(), None, "{good}");
        assert_eq!(st.request_timeout.value, good.parse::<u64>().unwrap());
    }
    // The env may go below the config floor of 10 s: the test hook (#813).
    let st = timeout_of(&Config::default(), &[("TOLE_DISCUSSION_TIMEOUT_SECS", "2")]);
    assert_eq!(st.discussion_timeout_error(), None);
    assert_eq!(st.request_timeout.value, 2);
}

#[test]
fn the_discussion_timeout_config_is_bounded() {
    // The user-config floor is 10 s (#813): 1..=9 are errors naming the key
    // and the floor; the env hook still goes down to 1 s (tested above).
    for bad in [0u64, 1, 5, 9, 3601] {
        let e = tole_cli::config::parse(
            &format!("[discussions]\nrequest_timeout_secs = {bad}\n"),
            Path::new("c.toml"),
        )
        .unwrap_err();
        assert!(
            e.message.contains("discussions.request_timeout_secs")
                && e.message.contains("10 to 3600")
                && e.message.contains("floor is 10 s"),
            "{e}"
        );
    }
    for good in [10u64, 120, 3600] {
        assert!(tole_cli::config::parse(
            &format!("[discussions]\nrequest_timeout_secs = {good}\n"),
            Path::new("c.toml")
        )
        .is_ok());
    }
    assert!(tole_cli::config::parse("[discussions]\ntimeout = 30\n", Path::new("c.toml")).is_err());
}

#[test]
fn a_project_config_cannot_set_the_discussion_timeout() {
    let project = cfg("[discussions]\nrequest_timeout_secs = 3600\n");
    let merged = tole_cli::config::layered(&project, &Config::default());
    assert_eq!(merged.discussions, None);
    let st = timeout_of(&merged, &[]);
    assert_eq!(
        (st.request_timeout.value, st.request_timeout.source),
        (120, Source::Default)
    );
    // With a user value, the user's wins over a project's larger one.
    let user = cfg("[discussions]\nrequest_timeout_secs = 200\n");
    let st = timeout_of(&tole_cli::config::layered(&project, &user), &[]);
    assert_eq!(
        (st.request_timeout.value, st.request_timeout.source),
        (200, Source::UserConfig)
    );
}

#[test]
fn config_check_reports_the_discussion_timeout_and_ignores_a_project_one() {
    let user = tmpdir();
    let up = user.join("config.toml");
    std::fs::write(&up, "[discussions]\nrequest_timeout_secs = 600\n").unwrap();
    let dir = tmpdir();
    let p = dir.join(CONFIG_REL_PATH);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "[discussions]\nrequest_timeout_secs = 3000\n").unwrap();
    let out = check(&dir, None, Some(&up), &Flags::default(), &env_of(&[])).unwrap();
    assert!(
        out.contains(
            "discussions.request_timeout_secs = 600  [user config]  effective: 600  (user config)"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "discussions.request_timeout_secs = 3000  [config]  IGNORED: a project config cannot set"
        ),
        "{out}"
    );
    let env = env_of(&[("TOLE_DISCUSSION_TIMEOUT_SECS", "900")]);
    let out = check(&dir, None, Some(&up), &Flags::default(), &env).unwrap();
    assert!(
        out.contains("effective: 900  (env TOLE_DISCUSSION_TIMEOUT_SECS)"),
        "{out}"
    );
    let env = env_of(&[("TOLE_DISCUSSION_TIMEOUT_SECS", "soon")]);
    let out = check(&dir, None, Some(&up), &Flags::default(), &env).unwrap();
    assert!(
        out.contains("INVALID: TOLE_DISCUSSION_TIMEOUT_SECS must be"),
        "{out}"
    );
}

// ------------------------------------------- chat request timeout (#1236)

#[test]
fn chat_timeout_defaults_to_900_and_env_beats_user_config_beats_default() {
    let st = timeout_of(&Config::default(), &[]);
    assert_eq!(
        (st.chat_timeout.value, st.chat_timeout.source),
        (900, Source::Default),
        "AC1: the default"
    );
    assert_eq!(st.chat_timeout_error(), None);
    let user = cfg("[provider]\nrequest_timeout_secs = 300\n");
    let c = tole_cli::config::layered(&Config::default(), &user);
    let st = timeout_of(&c, &[]);
    assert_eq!(
        (st.chat_timeout.value, st.chat_timeout.source),
        (300, Source::UserConfig)
    );
    let st = timeout_of(&c, &[("TOLE_REQUEST_TIMEOUT_SECS", " 1200 ")]);
    assert_eq!(st.chat_timeout.value, 1200);
    assert_eq!(
        st.chat_timeout.describe(),
        "(env TOLE_REQUEST_TIMEOUT_SECS)"
    );
    // A blank env counts as unset.
    let st = timeout_of(&c, &[("TOLE_REQUEST_TIMEOUT_SECS", "  ")]);
    assert_eq!(st.chat_timeout.value, 300);
    // The discussion knob is a separate setting.
    let st = timeout_of(&c, &[("TOLE_DISCUSSION_TIMEOUT_SECS", "60")]);
    assert_eq!((st.chat_timeout.value, st.request_timeout.value), (300, 60));
}

#[test]
fn an_invalid_chat_timeout_env_is_an_error_naming_the_variable_and_range() {
    for bad in [
        "abc",
        "0",
        "-5",
        "1.5",
        "29",
        "3601",
        "99999999999999999999",
    ] {
        let st = timeout_of(&Config::default(), &[("TOLE_REQUEST_TIMEOUT_SECS", bad)]);
        let msg = st
            .chat_timeout_error()
            .unwrap_or_else(|| panic!("{bad:?} must be an error"));
        assert!(
            msg.contains("TOLE_REQUEST_TIMEOUT_SECS")
                && msg.contains(bad)
                && msg.contains("30 to 3600"),
            "{msg}"
        );
    }
    for good in ["30", "3600"] {
        let st = timeout_of(&Config::default(), &[("TOLE_REQUEST_TIMEOUT_SECS", good)]);
        assert_eq!(st.chat_timeout_error(), None, "{good}");
        assert_eq!(st.chat_timeout.value, good.parse::<u64>().unwrap());
    }
}

#[test]
fn the_chat_timeout_config_is_bounded_and_strict() {
    for bad in [0u64, 1, 29, 3601] {
        let e = tole_cli::config::parse(
            &format!("[provider]\nrequest_timeout_secs = {bad}\n"),
            Path::new("c.toml"),
        )
        .unwrap_err();
        assert!(
            e.message.contains("provider.request_timeout_secs") && e.message.contains("30 to 3600"),
            "{e}"
        );
    }
    for good in [30u64, 900, 3600] {
        assert!(tole_cli::config::parse(
            &format!("[provider]\nrequest_timeout_secs = {good}\n"),
            Path::new("c.toml")
        )
        .is_ok());
    }
    assert!(tole_cli::config::parse("[provider]\ntimeout = 60\n", Path::new("c.toml")).is_err());
}

#[test]
fn a_project_config_cannot_set_the_chat_timeout() {
    let project = cfg("[provider]\nrequest_timeout_secs = 3600\n");
    let merged = tole_cli::config::layered(&project, &Config::default());
    assert_eq!(merged.provider, None);
    let st = timeout_of(&merged, &[]);
    assert_eq!(
        (st.chat_timeout.value, st.chat_timeout.source),
        (900, Source::Default)
    );
    let user = cfg("[provider]\nrequest_timeout_secs = 200\n");
    let st = timeout_of(&tole_cli::config::layered(&project, &user), &[]);
    assert_eq!(
        (st.chat_timeout.value, st.chat_timeout.source),
        (200, Source::UserConfig)
    );
}

#[test]
fn the_chat_timeout_doctor_line_names_the_value_and_its_source() {
    // Nothing installed in a unit test: the default and its source.
    assert_eq!(
        chat_timeout_line(),
        "provider: request timeout 900s (default)"
    );
    assert_eq!(chat_timeout(), std::time::Duration::from_secs(900));
}

#[test]
fn config_check_reports_the_chat_timeout_and_ignores_a_project_one() {
    let user = tmpdir();
    let up = user.join("config.toml");
    std::fs::write(&up, "[provider]\nrequest_timeout_secs = 600\n").unwrap();
    let dir = tmpdir();
    let p = dir.join(CONFIG_REL_PATH);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "[provider]\nrequest_timeout_secs = 3000\n").unwrap();
    let out = check(&dir, None, Some(&up), &Flags::default(), &env_of(&[])).unwrap();
    assert!(
        out.contains(
            "provider.request_timeout_secs = 600  [user config]  effective: 600  (user config)"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "provider.request_timeout_secs = 3000  [config]  IGNORED: a project config cannot set"
        ),
        "{out}"
    );
    let env = env_of(&[("TOLE_REQUEST_TIMEOUT_SECS", "1500")]);
    let out = check(&dir, None, Some(&up), &Flags::default(), &env).unwrap();
    assert!(
        out.contains("effective: 1500  (env TOLE_REQUEST_TIMEOUT_SECS)"),
        "{out}"
    );
    let env = env_of(&[("TOLE_REQUEST_TIMEOUT_SECS", "soon")]);
    let out = check(&dir, None, Some(&up), &Flags::default(), &env).unwrap();
    assert!(
        out.contains("INVALID: TOLE_REQUEST_TIMEOUT_SECS must be"),
        "{out}"
    );
}

// ---- #864: a user config that is not there, or cannot be inspected

#[test]
fn a_missing_user_config_file_is_none_and_an_uninspectable_path_is_an_error() {
    let d = tmpdir();
    let mut io = Fake::new(false, false);
    // Absent: "no user config".
    assert!(load_user_config(Some(&d.join("absent.toml")), &mut io)
        .unwrap()
        .is_none());
    assert!(load_user_config(None, &mut io).unwrap().is_none());
    // A parent that is a FILE: `exists()` says false, `try_exists` is an error.
    let file = d.join("a-file");
    std::fs::write(&file, "x").unwrap();
    let bad = file.join("config.toml");
    assert!(!bad.exists(), "the old check read this as absent");
    let e = load_user_config(Some(&bad), &mut io)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("cannot inspect the user config") && e.contains("a-file"),
        "{e}"
    );
    assert!(io.shown.is_empty(), "nothing announced: {}", io.shown);
}

fn ctx(root: bool, session: bool, unattended: bool) -> AbsentCtx {
    AbsentCtx {
        root_overridden: root,
        under_session: session,
        unattended_command: unattended,
    }
}

#[test]
fn the_absent_notice_is_for_the_unusual_cases_only() {
    let p = Path::new("/h/.codecora/tole/config.toml");
    // The default path, an attended owner or a cron tick: silent.
    assert_eq!(
        absent_user_config_notice(Some(p), &ctx(false, false, false)),
        None
    );
    // Each unusual case alone is announced, naming the path and the effect.
    for c in [
        ctx(true, false, false),
        ctx(false, true, false),
        ctx(false, false, true),
    ] {
        let n = absent_user_config_notice(Some(p), &c).unwrap();
        assert!(
            n.starts_with("tole: no user config at /h/.codecora/tole/config.toml")
                && n.contains("sandbox, approvals and secret release use their defaults"),
            "{n}"
        );
    }
    // An unresolvable data directory is always announced.
    let n = absent_user_config_notice(None, &ctx(false, false, false)).unwrap();
    assert!(n.contains("cannot resolve the tole data directory"), "{n}");
    // The path is terminal-safe.
    let n = absent_user_config_notice(
        Some(Path::new("/x/\u{1b}[31m/c.toml")),
        &ctx(true, false, false),
    )
    .unwrap();
    assert!(!n.contains('\u{1b}'), "{n:?}");
}

#[test]
fn a_path_in_a_user_config_message_is_one_short_line() {
    use tole_cli::config::one_line_path;
    // A newline, a carriage return and an escape sequence cannot forge a line
    // or drive the terminal.
    let nasty = Path::new("/x/a\nb\rc\u{1b}[31md/config.toml");
    let s = one_line_path(nasty);
    assert!(
        !s.contains('\n') && !s.contains('\r') && !s.contains('\u{1b}'),
        "{s:?}"
    );
    assert_eq!(s, "/x/a\\nb\\rc\\u{1b}[31md/config.toml");
    // An over-long path is cut with an ellipsis.
    let long = format!("/{}", "a".repeat(500));
    let s = one_line_path(Path::new(&long));
    assert!(
        s.chars().count() <= 201 && s.ends_with('\u{2026}'),
        "{}",
        s.len()
    );
    // An ordinary path is untouched.
    assert_eq!(
        one_line_path(Path::new("/h/.codecora/tole/config.toml")),
        "/h/.codecora/tole/config.toml"
    );
}

#[test]
fn the_messages_carry_a_single_line_path_and_the_error_names_the_way_out() {
    let d = tmpdir();
    let file = d.join("evil\nforged: line");
    std::fs::write(&file, "x").unwrap();
    let bad = file.join("config.toml");
    let mut io = Fake::new(false, false);
    let e = load_user_config(Some(&bad), &mut io)
        .unwrap_err()
        .to_string();
    assert_eq!(e.lines().count(), 1, "{e:?}");
    assert!(
        e.contains("--no-config") && e.contains("CODECORA_HOME") && e.contains("evil\\nforged"),
        "{e}"
    );
    let n = absent_user_config_notice(Some(&bad), &ctx(true, false, false)).unwrap();
    assert_eq!(n.lines().count(), 1, "{n:?}");
}
