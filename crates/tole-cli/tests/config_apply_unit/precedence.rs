use crate::support::*;
use tole_cli::config::apply::*;
use tole_cli::config::Config;

// ------------------------------------------------- pure precedence matrix

#[test]
fn flag_beats_env_beats_config_beats_default() {
    let r = |f, e, c| resolve_string_or(f, e, c, "dflt");
    assert_eq!(r(s("f"), s("e"), s("c")), ("f".into(), Source::Flag));
    assert_eq!(r(None, s("e"), s("c")), ("e".into(), Source::Env));
    assert_eq!(r(None, None, s("c")), ("c".into(), Source::Config));
    assert_eq!(r(None, None, None), ("dflt".into(), Source::Default));
    // Flag beats config even with no env; env beats config even with no flag.
    assert_eq!(r(s("f"), None, s("c")).1, Source::Flag);
}

#[test]
fn empty_or_blank_env_and_config_count_as_unset() {
    let r = |f, e, c| resolve_string(f, e, c);
    assert_eq!(r(None, s(""), s("c")), Some(("c".into(), Source::Config)));
    assert_eq!(r(None, s("  "), s("c")), Some(("c".into(), Source::Config)));
    assert_eq!(r(None, s(""), s("")), None);
    assert_eq!(r(None, None, s(" ")), None);
}

#[test]
fn generic_resolve_works_for_numbers() {
    assert_eq!(
        resolve(Some(1u64), Some(2), Some(3)),
        Some((1, Source::Flag))
    );
    assert_eq!(resolve(None, Some(2u64), Some(3)), Some((2, Source::Env)));
    assert_eq!(resolve(None, None, Some(3u64)), Some((3, Source::Config)));
    assert_eq!(resolve_or::<u64>(None, None, None, 9), (9, Source::Default));
}

#[test]
fn source_display() {
    assert_eq!(Source::Flag.to_string(), "flag");
    assert_eq!(Source::Profile.to_string(), "profile");
    assert_eq!(Source::Env.to_string(), "env");
    assert_eq!(Source::Config.to_string(), "config");
    assert_eq!(Source::Default.to_string(), "default");
}

// ------------------------------------------------- per-key settings matrix

#[test]
fn sessions_dir_precedence_and_default() {
    let c = cfg("sessions_dir = \"cfg-s\"\n");
    let none = env_of(&[]);
    let st = Settings::resolve(&Flags::default(), &c, &none);
    assert_eq!(st.sessions_dir.value, "cfg-s");
    assert_eq!(st.sessions_dir.source, Source::Config);
    let f = Flags {
        sessions_dir: s("flag-s"),
        ..Default::default()
    };
    let st = Settings::resolve(&f, &c, &none);
    assert_eq!(
        (st.sessions_dir.value.as_str(), st.sessions_dir.source),
        ("flag-s", Source::Flag)
    );
    let st = Settings::resolve(&Flags::default(), &Config::default(), &none);
    assert_eq!(
        (st.sessions_dir.value.as_str(), st.sessions_dir.source),
        (DEFAULT_SESSIONS_DIR, Source::Default)
    );
}

#[test]
fn workspace_precedence_has_no_env() {
    let c = cfg("workspace = \"cfg-w\"\n");
    // An env var of any plausible name must not matter.
    let env = env_of(&[("TOLE_WORKSPACE", "env-w")]);
    let st = Settings::resolve(&Flags::default(), &c, &env);
    assert_eq!(st.workspace.unwrap().source, Source::Config);
    let f = Flags {
        workspace: s("flag-w"),
        ..Default::default()
    };
    let st = Settings::resolve(&f, &c, &env);
    assert_eq!(st.workspace.unwrap().value, "flag-w");
    assert!(
        Settings::resolve(&Flags::default(), &Config::default(), &env)
            .workspace
            .is_none()
    );
}

#[test]
fn memory_precedence_flag_env_config() {
    let c = cfg("memory = \"uteke-cfg\"\n");
    let env = env_of(&[("TOLE_MEMORY", "uteke-env")]);
    let f = Flags {
        memory: s("uteke-flag"),
        ..Default::default()
    };
    assert_eq!(
        Settings::resolve(&f, &c, &env).memory.unwrap().value,
        "uteke-flag"
    );
    let m = Settings::resolve(&Flags::default(), &c, &env)
        .memory
        .unwrap();
    assert_eq!((m.value.as_str(), m.source), ("uteke-env", Source::Env));
    assert_eq!(m.env, Some("TOLE_MEMORY"));
    // Empty env is ignored; the config applies.
    let m = Settings::resolve(&Flags::default(), &c, &env_of(&[("TOLE_MEMORY", "")]))
        .memory
        .unwrap();
    assert_eq!((m.value.as_str(), m.source), ("uteke-cfg", Source::Config));
    // A blank flag does not shadow env (existing `--memory ""` rule).
    let f = Flags {
        memory: s("  "),
        ..Default::default()
    };
    assert_eq!(
        Settings::resolve(&f, &c, &env).memory.unwrap().value,
        "uteke-env"
    );
}

#[test]
fn system_prompt_precedence_flag_env_config() {
    let c = cfg("system_prompt = \"cfg\"\n");
    let env = env_of(&[("TOLE_SYSTEM_PROMPT", "env")]);
    let f = Flags {
        system: s("flag"),
        ..Default::default()
    };
    assert_eq!(
        Settings::resolve(&f, &c, &env).system_prompt.unwrap().value,
        "flag"
    );
    assert_eq!(
        Settings::resolve(&Flags::default(), &c, &env)
            .system_prompt
            .unwrap()
            .value,
        "env"
    );
    let e = env_of(&[("TOLE_SYSTEM_PROMPT", " ")]);
    let p = Settings::resolve(&Flags::default(), &c, &e)
        .system_prompt
        .unwrap();
    assert_eq!((p.value.as_str(), p.source), ("cfg", Source::Config));
    // An explicit empty --system stays an explicit (empty) prompt.
    let f = Flags {
        system: s(""),
        ..Default::default()
    };
    let p = Settings::resolve(&f, &c, &env).system_prompt.unwrap();
    assert_eq!((p.value.as_str(), p.source), ("", Source::Flag));
}

#[test]
fn mission_budgets_flag_then_config_no_env() {
    let c = cfg("[mission]\nmax_steps = 10\nmax_minutes = 20\nmax_tokens = 30\n");
    let env = env_of(&[("TOLE_MAX_STEPS", "99")]);
    let st = Settings::resolve(&Flags::default(), &c, &env);
    assert_eq!(st.max_steps.unwrap().value, 10);
    assert_eq!(st.max_minutes.unwrap().value, 20);
    assert_eq!(st.max_tokens.as_ref().unwrap().source, Source::Config);
    let f = Flags {
        max_steps: Some(1),
        max_tokens: Some(3),
        ..Default::default()
    };
    let st = Settings::resolve(&f, &c, &env);
    assert_eq!(
        (
            st.max_steps.as_ref().unwrap().value,
            st.max_steps.unwrap().source
        ),
        (1, Source::Flag)
    );
    assert_eq!(st.max_minutes.unwrap().value, 20);
    assert_eq!(st.max_tokens.unwrap().value, 3);
    // Nothing anywhere: no value (the budget tier default applies later).
    let st = Settings::resolve(&Flags::default(), &Config::default(), &env);
    assert!(st.max_steps.is_none() && st.max_minutes.is_none() && st.max_tokens.is_none());
}
