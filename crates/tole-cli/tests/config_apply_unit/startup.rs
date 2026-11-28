use crate::support::*;
use std::path::{Path, PathBuf};
use tole_cli::config::apply::*;
use tole_cli::config::Config;
use tole_cli::config::CONFIG_REL_PATH;
use tole_cli::config_trust::ConfigTrustIo;

// ------------------------------------------------------------- startup

#[test]
fn no_config_is_a_silent_noop_and_never_resolves_the_store() {
    let d = tmpdir();
    let mut io = Fake::new(true, true);
    let r = load_project_config(&d, None, &no_store, &mut io).unwrap();
    assert!(r.is_none());
    assert!(io.shown.is_empty(), "{}", io.shown);
    assert_eq!(io.confirms, 0);
}

#[test]
fn untrusted_non_interactive_fails_closed_with_the_instruction() {
    let (d, home) = project("model = \"m\"\n");
    let store = store_of(&home);
    let mut io = Fake::new(false, true);
    let e = load_project_config(&d, None, &|| Ok(store.clone()), &mut io).unwrap_err();
    let msg = format!("{e:#}");
    assert!(msg.contains("is not trusted"), "{msg}");
    assert!(msg.contains("Run: tole config trust"), "{msg}");
    assert_eq!(io.confirms, 0);
    assert!(!io.shown.contains("using config"), "{}", io.shown);
}

#[test]
fn trusted_config_loads_and_prints_one_using_line() {
    let (d, home) = project("model = \"m\"\n");
    let store = store_of(&home);
    tole_cli::config_trust::record(&store, &d, "model = \"m\"\n").unwrap();
    let mut io = Fake::new(false, false);
    let l = load_project_config(&d, None, &|| Ok(store.clone()), &mut io)
        .unwrap()
        .unwrap();
    assert_eq!(l.config.model.as_deref(), Some("m"));
    assert_eq!(io.shown.lines().count(), 1, "{}", io.shown);
    assert!(
        io.shown.starts_with("tole: using config ") && io.shown.contains("config.toml"),
        "{}",
        io.shown
    );
}

#[test]
fn explicit_config_needs_no_store() {
    let d = tmpdir();
    let f = d.join("x.toml");
    std::fs::write(&f, "model = \"m\"\n").unwrap();
    let mut io = Fake::new(false, false);
    let l = load_project_config(&d, Some(&f), &no_store, &mut io)
        .unwrap()
        .unwrap();
    assert_eq!(l.config.model.as_deref(), Some("m"));
}

#[test]
fn changed_file_fails_closed_again() {
    let (d, home) = project("model = \"m\"\n");
    let store = store_of(&home);
    tole_cli::config_trust::record(&store, &d, "model = \"m\"\n").unwrap();
    std::fs::write(d.join(CONFIG_REL_PATH), "model = \"evil\"\n").unwrap();
    let mut io = Fake::new(false, false);
    assert!(load_project_config(&d, None, &|| Ok(store.clone()), &mut io).is_err());
}

#[test]
fn invalid_trusted_content_is_a_parse_error() {
    let (d, home) = project("modle = \"m\"\n");
    let store = store_of(&home);
    tole_cli::config_trust::record(&store, &d, "modle = \"m\"\n").unwrap();
    let mut io = Fake::new(false, false);
    let e = load_project_config(&d, None, &|| Ok(store.clone()), &mut io).unwrap_err();
    assert!(format!("{e:#}").contains("modle"), "{e:#}");
    assert!(!io.shown.contains("using config"));
}

#[test]
fn interactive_yes_persists_and_loads() {
    let (d, home) = project("model = \"m\"\n");
    let store = store_of(&home);
    let mut io = Fake::new(true, true);
    let l = load_project_config(&d, None, &|| Ok(store.clone()), &mut io).unwrap();
    assert!(l.is_some());
    assert_eq!(io.confirms, 1);
    let mut io2 = Fake::new(false, false);
    assert!(
        load_project_config(&d, None, &|| Ok(store.clone()), &mut io2)
            .unwrap()
            .is_some()
    );
}

#[test]
fn startup_io_never_prompts_when_not_allowed() {
    // allow_prompt=false forces non-interactive regardless of the tty.
    let io = StartupIo::new(false);
    assert!(!io.is_interactive());
}

#[test]
fn fill_carries_only_low_risk_provider_values() {
    let c = cfg("model = \"m\"\nbase_url = \"u\"\nsystem_prompt = \"p\"\nmemory = \"uteke\"\n");
    assert_eq!(
        Settings::fill_from(&c),
        ProviderFill {
            base_url: s("u"),
            model: s("m"),
            system_prompt: s("p"),
            profile_model: None,
            profile_system_prompt: None,
            profile_name: None,
        }
    );
    assert_eq!(
        Settings::fill_from(&Config::default()),
        ProviderFill::default()
    );
}

/// #584: a selected profile sits between the per-key flags and the env.
#[test]
fn a_delegated_child_takes_allow_and_trust_only_from_its_flags() {
    let user = cfg(
        "allow = [\"cfg_*\"]\ntrust = [\"workspace\"]\n[profiles.coder]\nallow = [\"write_*\"]\n",
    );
    let profile = tole_cli::profile::Profile::resolve(&user, "coder", None, None).unwrap();
    let env = env_of(&[
        ("TOLE_TRUST", "internal"),
        ("TOLE_DELEGATION_LOG", "/a/delegation.jsonl"),
        ("TOLE_DELEGATION_NODE", "g-1.1"),
    ]);
    let flags = Flags {
        profile: Some(profile.clone()),
        ..Flags::default()
    };
    let st = Settings::resolve(&flags, &user, &env);
    assert_eq!(st.allow, None, "no profile, config or env allow");
    assert_eq!(st.trust, None, "no config or env trust");
    let flags = Flags {
        profile: Some(profile),
        allow: vec!["write_file".into()],
        ..Flags::default()
    };
    let st = Settings::resolve(&flags, &user, &env);
    assert_eq!(st.allow.unwrap().value, vec!["write_file".to_string()]);
    // Not delegated: the profile's allow applies as before.
    let plain = env_of(&[("TOLE_TRUST", "internal")]);
    let flags = Flags {
        profile: Some(tole_cli::profile::Profile::resolve(&user, "coder", None, None).unwrap()),
        ..Flags::default()
    };
    let st = Settings::resolve(&flags, &user, &plain);
    assert_eq!(st.allow.unwrap().value, vec!["write_*".to_string()]);
    assert!(st.trust.is_some());
}

#[test]
fn profile_layer_is_below_flags_and_above_env_and_config() {
    let user = cfg(
        "model = \"cfg-model\"\nsystem_prompt = \"cfg prompt\"\nallow = [\"cfg_*\"]\n\
         skill = [\"/cfg/SKILL.md\"]\n\
         [profiles.coder]\nallow = [\"write_*\"]\nskills = [\"/p/SKILL.md\"]\n\
         [profiles.coder.models]\nstandard = \"profile-model\"\n",
    );
    let mut profile = tole_cli::profile::Profile::resolve(&user, "coder", None, None).unwrap();
    profile.system_prompt = s("persona");
    let env = env_of(&[
        ("TOLE_BASE_URL", "https://x/v1"),
        ("TOLE_MODEL", "env-model"),
        ("TOLE_API_KEY", "sk-env"),
        ("TOLE_SYSTEM_PROMPT", "env prompt"),
        ("TOLE_SKILL", "/env/SKILL.md"),
    ]);
    let flags = Flags {
        profile: Some(profile),
        ..Flags::default()
    };
    let st = Settings::resolve(&flags, &user, &env);
    let m = st.provider.model.as_ref().unwrap();
    assert_eq!(
        (m.value.as_str(), m.source),
        ("profile-model", Source::Profile)
    );
    assert_eq!(st.provider.config.as_ref().unwrap().model, "profile-model");
    let p = st.system_prompt.as_ref().unwrap();
    assert_eq!((p.value.as_str(), p.source), ("persona", Source::Profile));
    let a = st.allow.as_ref().unwrap();
    assert_eq!(
        (a.value.clone(), a.source),
        (vec!["write_*".to_string()], Source::Profile)
    );
    let k = st.skill.as_ref().unwrap();
    assert_eq!(
        (k.value.clone(), k.source),
        (vec![PathBuf::from("/p/SKILL.md")], Source::Profile)
    );
    let r = st.memory_room.as_ref().unwrap();
    assert_eq!(
        (r.value.as_str(), r.source),
        ("profile-coder", Source::Profile)
    );
    // Explicit per-key flags still win over the profile.
    let flags = Flags {
        system: s("flag prompt"),
        allow: vec!["flag_*".into()],
        skill: vec![PathBuf::from("/flag/SKILL.md")],
        ..flags
    };
    let st = Settings::resolve(&flags, &user, &env);
    assert_eq!(st.system_prompt.unwrap().source, Source::Flag);
    assert_eq!(st.allow.unwrap().value, vec!["flag_*".to_string()]);
    assert_eq!(st.skill.unwrap().source, Source::Flag);
    // Without a profile nothing changes: env model, env prompt, config allow.
    let st = Settings::resolve(&Flags::default(), &user, &env);
    assert_eq!(st.provider.model.unwrap().value, "env-model");
    assert_eq!(st.system_prompt.unwrap().value, "env prompt");
    assert_eq!(st.allow.unwrap().source, Source::Config);
    assert_eq!(st.memory_room, None);
}

#[test]
fn fill_with_profile_carries_the_profile_model_and_persona() {
    let c = cfg("model = \"m\"\n[profiles.p.models]\nstandard = \"pm\"\n");
    let mut prof = tole_cli::profile::Profile::resolve(&c, "p", None, None).unwrap();
    prof.system_prompt = s("persona");
    let f = Settings::fill_with_profile(&c, Some(&prof));
    assert_eq!(
        (
            f.model,
            f.profile_model,
            f.profile_system_prompt,
            f.profile_name
        ),
        (s("m"), s("pm"), s("persona"), s("p"))
    );
    assert_eq!(
        Settings::fill_with_profile(&c, None),
        Settings::fill_from(&c)
    );
}

#[test]
fn skill_precedence_flag_env_config() {
    // #434: TOLE_SKILL is the env layer of the `skill` list (PATH-style).
    let c = cfg("skill = [\"cfg/SKILL.md\"]\n");
    let env = env_of(&[("TOLE_SKILL", "/a/SKILL.md::/b/SKILL.md")]);
    let skill = |f: &Flags, e: &dyn Fn(&str) -> Option<String>| {
        Settings::resolve(f, &c, e)
            .skill
            .map(|s| (s.value, s.source))
    };
    assert_eq!(
        skill(&Flags::default(), &env),
        Some((
            vec![PathBuf::from("/a/SKILL.md"), PathBuf::from("/b/SKILL.md")],
            Source::Env
        )),
        "empty entries are dropped"
    );
    let f = Flags {
        skill: vec![PathBuf::from("flag/SKILL.md")],
        ..Flags::default()
    };
    assert_eq!(
        skill(&f, &env),
        Some((vec![PathBuf::from("flag/SKILL.md")], Source::Flag))
    );
    assert_eq!(
        skill(&Flags::default(), &env_of(&[("TOLE_SKILL", "")])),
        Some((vec![PathBuf::from("cfg/SKILL.md")], Source::Config)),
        "a blank TOLE_SKILL leaves the config value"
    );
}

#[test]
fn user_layer_keys_report_their_own_source() {
    let user = tole_cli::config::parse(
        "skill = [\"g\"]\nplan_mode = true\nmodel = \"u\"\n[mission]\nverify_timeout = 9\n",
        Path::new("u.toml"),
    )
    .unwrap();
    let project = tole_cli::config::parse("model = \"p\"\n", Path::new("p.toml")).unwrap();
    let cfg = tole_cli::config::layered(&project, &user);
    let env = |_: &str| None;
    let s = Settings::resolve(&Flags::default(), &cfg, &env);
    assert_eq!(s.skill.as_ref().unwrap().source, Source::UserConfig);
    assert_eq!(s.plan_mode.source, Source::UserConfig);
    assert_eq!(s.verify_timeout.source, Source::UserConfig);
    assert_eq!(s.provider.model.as_ref().unwrap().source, Source::Config);
    assert_eq!(s.provider.model.as_ref().unwrap().value, "p");
    assert_eq!(s.skill.as_ref().unwrap().describe(), "(user config)");
    // A flag still wins over the user layer.
    let flags = Flags {
        skill: vec![PathBuf::from("f")],
        ..Flags::default()
    };
    let s = Settings::resolve(&flags, &cfg, &env);
    assert_eq!(s.skill.unwrap().source, Source::Flag);
}

#[test]
fn unsupported_key_names_the_layer_it_came_from() {
    let user = tole_cli::config::parse("on_turnend = [\"x\"]\n", Path::new("u.toml")).unwrap();
    let cfg = tole_cli::config::layered(&Config::default(), &user);
    let msg = unsupported_keys(&cfg, true, false).unwrap();
    assert!(msg.starts_with("user config key `on_turnend`"), "{msg}");
    let cfg = tole_cli::config::layered(&user, &Config::default());
    let msg = unsupported_keys(&cfg, true, false).unwrap();
    assert!(msg.starts_with("project config key `on_turnend`"), "{msg}");
}

#[test]
fn compaction_settings_resolve_flag_env_config_default() {
    let none = |_: &str| None;
    let s = Settings::resolve(&Flags::default(), &Config::default(), &none);
    assert_eq!(s.context_window, None, "detect from /models");
    assert_eq!(
        (s.compact_at.value, s.compact_at.source),
        (tole_core::compaction::Policy::DEFAULT_AT, Source::Default)
    );
    assert!(!s.no_compaction.value);
    let cfg = tole_cli::config::parse(
        "context_window = 64000\ncompact_at = 0.7\nno_compaction = true\n",
        Path::new("c.toml"),
    )
    .unwrap();
    let s = Settings::resolve(&Flags::default(), &cfg, &none);
    assert_eq!(s.context_window.as_ref().unwrap().value, 64000);
    assert_eq!(
        (s.compact_at.value, s.compact_at.source),
        (0.7, Source::Config)
    );
    assert!(s.no_compaction.value);
    // The env window beats the config; the flag fraction beats the config.
    let env = |k: &str| (k == CONTEXT_WINDOW_ENV).then(|| "200000".to_string());
    let flags = Flags {
        compact_at: Some(0.9),
        ..Flags::default()
    };
    let s = Settings::resolve(&flags, &cfg, &env);
    let w = s.context_window.unwrap();
    assert_eq!((w.value, w.source), (200000, Source::Env));
    assert_eq!(
        (s.compact_at.value, s.compact_at.source),
        (0.9, Source::Flag)
    );
    // A malformed env window is ignored, not fatal.
    let bad = |k: &str| (k == CONTEXT_WINDOW_ENV).then(|| "lots".to_string());
    let s = Settings::resolve(&Flags::default(), &cfg, &bad);
    assert_eq!(s.context_window.unwrap().source, Source::Config);
}
