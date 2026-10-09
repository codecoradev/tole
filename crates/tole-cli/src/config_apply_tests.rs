use super::*;
use crate::config::CONFIG_REL_PATH;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-apply-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

fn cfg(src: &str) -> Config {
    crate::config::parse(src, Path::new("c.toml")).unwrap()
}

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

// ------------------------------------------------------------- provider

const FULL_TOLE: [(&str, &str); 3] = [
    ("TOLE_BASE_URL", "https://env"),
    ("TOLE_MODEL", "env-model"),
    ("TOLE_API_KEY", "k"),
];

#[test]
fn provider_without_config_equals_from_env_semantics() {
    let none = resolve_provider(None, None, &env_of(&FULL_TOLE));
    let c = none.config.unwrap();
    assert_eq!(
        (c.base_url.as_str(), c.model.as_str()),
        ("https://env", "env-model")
    );
    // Incomplete env + no config = unusable, exactly like from_env().
    let r = resolve_provider(None, None, &env_of(&[("TOLE_API_KEY", "k")]));
    assert!(r.config.is_none());
    // OPENAI_* fallback when TOLE_* is incomplete.
    let r = resolve_provider(
        None,
        None,
        &env_of(&[
            ("TOLE_MODEL", "x"),
            ("OPENAI_BASE_URL", "https://o"),
            ("OPENAI_MODEL", "om"),
            ("OPENAI_API_KEY", "k"),
        ]),
    );
    assert_eq!(r.config.unwrap().model, "om");
}

#[test]
fn provider_env_beats_config() {
    let r = resolve_provider(Some("https://cfg"), Some("cfg-model"), &env_of(&FULL_TOLE));
    let (b, m) = (r.base_url.unwrap(), r.model.unwrap());
    assert_eq!(
        (b.value.as_str(), b.source, b.env),
        ("https://env", Source::Env, Some("TOLE_BASE_URL"))
    );
    assert_eq!(
        (m.value.as_str(), m.source, m.env),
        ("env-model", Source::Env, Some("TOLE_MODEL"))
    );
}

#[test]
fn provider_config_fills_missing_env_values() {
    // Key + base in env, model only in config.
    let r = resolve_provider(
        None,
        Some("cfg-model"),
        &env_of(&[("TOLE_BASE_URL", "https://env"), ("TOLE_API_KEY", "k")]),
    );
    let m = r.model.clone().unwrap();
    assert_eq!((m.value.as_str(), m.source), ("cfg-model", Source::Config));
    assert_eq!(r.base_url.unwrap().source, Source::Env);
    assert_eq!(r.config.unwrap().model, "cfg-model");
    // Key only in env; base + model from config.
    let r = resolve_provider(
        Some("https://cfg"),
        Some("cfg-model"),
        &env_of(&[("TOLE_API_KEY", "k")]),
    );
    let c = r.config.unwrap();
    assert_eq!(
        (c.base_url.as_str(), c.model.as_str()),
        ("https://cfg", "cfg-model")
    );
    // The API key never comes from the config: no key = unusable.
    let r = resolve_provider(Some("https://cfg"), Some("cfg-model"), &env_of(&[]));
    assert!(r.config.is_none());
    assert_eq!(r.model.unwrap().source, Source::Config);
}

#[test]
fn provider_complete_env_prefix_wins_over_config_filled_one() {
    // TOLE has only a key (config could fill it) but OPENAI is complete:
    // today's behavior (OPENAI) is kept.
    let r = resolve_provider(
        Some("https://cfg"),
        Some("cfg-model"),
        &env_of(&[
            ("TOLE_API_KEY", "k1"),
            ("OPENAI_BASE_URL", "https://o"),
            ("OPENAI_MODEL", "om"),
            ("OPENAI_API_KEY", "k2"),
        ]),
    );
    assert_eq!(r.config.unwrap().model, "om");
}

#[test]
fn provider_env_prefix_table_matches_core() {
    assert_eq!(tole_core::openai::ENV_PREFIXES, ["TOLE", "OPENAI"]);
    for (i, p) in tole_core::openai::ENV_PREFIXES.iter().enumerate() {
        assert_eq!(BASE_URL_ENV[i], format!("{p}_BASE_URL"));
        assert_eq!(MODEL_ENV[i], format!("{p}_MODEL"));
        assert_eq!(API_KEY_ENV[i], format!("{p}_API_KEY"));
    }
}

// ------------------------------- security-sensitive keys (part 3b)

const SECURITY_SET: &str = r#"
plan_mode = true
trust = ["internal"]
allow = ["write_*"]
mcp_server = ["a=b"]
no_auto_mcp = true
on_pretool = ["p"]
on_posttool = ["q"]
on_turnend = ["r"]
skill = ["s/SKILL.md"]
no_skills = true
[mission]
verify = "cargo test"
verify_timeout = 5
"#;

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The pure matrix behind every list key: the highest NON-EMPTY layer
/// replaces the whole list; nothing is ever merged across layers.
#[test]
fn resolve_list_matrix_replaces_wholesale() {
    let f = v(&["f1", "f2"]);
    let e = v(&["e1"]);
    let c = v(&["c1", "c2", "c3"]);
    let r = |f: &[String], e: &[String], c: Option<&[String]>| resolve_list(f, e, c);
    // flag > env > config
    assert_eq!(r(&f, &e, Some(&c)), Some((f.clone(), Source::Flag)));
    assert_eq!(r(&[], &e, Some(&c)), Some((e.clone(), Source::Env)));
    assert_eq!(r(&[], &[], Some(&c)), Some((c.clone(), Source::Config)));
    // flag over config with no env; env over config with no flag
    assert_eq!(r(&f, &[], Some(&c)), Some((f.clone(), Source::Flag)));
    assert_eq!(r(&f, &e, None), Some((f.clone(), Source::Flag)));
    // NEVER merged: a one-entry flag list does not pick up config entries.
    let one = v(&["only"]);
    let (got, _) = r(&one, &e, Some(&c)).unwrap();
    assert_eq!(got, one);
    // An empty config list is "not given".
    assert_eq!(r(&[], &[], Some(&[])), None);
    assert_eq!(r(&[], &[], None), None);
    // An empty flag list is "not given" (clap cannot tell the difference).
    assert_eq!(r(&[], &e, Some(&c)).unwrap().1, Source::Env);
    // Generic over the element type (skill paths).
    let paths = [PathBuf::from("a")];
    assert_eq!(
        resolve_list(&paths, &[], Some(&[PathBuf::from("b")])),
        Some((vec![PathBuf::from("a")], Source::Flag))
    );
}

/// The boolean rule: `flag || config`. No flag can force `false`.
#[test]
fn resolve_bool_flags_can_only_turn_on() {
    assert_eq!(resolve_bool(false, None), (false, Source::Default));
    assert_eq!(resolve_bool(true, None), (true, Source::Flag));
    assert_eq!(resolve_bool(false, Some(true)), (true, Source::Config));
    assert_eq!(resolve_bool(false, Some(false)), (false, Source::Config));
    // The flag wins the label; the value is true either way.
    assert_eq!(resolve_bool(true, Some(true)), (true, Source::Flag));
    assert_eq!(resolve_bool(true, Some(false)), (true, Source::Flag));
    // Exhaustive: effective == flag || config for every combination.
    for flag in [false, true] {
        for c in [None, Some(false), Some(true)] {
            assert_eq!(
                resolve_bool(flag, c).0,
                flag || c == Some(true),
                "flag={flag} cfg={c:?}"
            );
        }
    }
}

#[test]
fn split_env_list_splits_on_commas_and_whitespace() {
    assert_eq!(
        split_env_list("internal, read_only"),
        v(&["internal", "read_only"])
    );
    assert_eq!(split_env_list(" a  b,,c "), v(&["a", "b", "c"]));
    assert!(split_env_list("  ,  ").is_empty());
}

#[test]
fn security_keys_are_applied_from_config() {
    let env = env_of(&[]);
    let st = Settings::resolve(&Flags::default(), &cfg(SECURITY_SET), &env);
    let list = |e: &ListEff<String>| e.as_ref().map(|e| (e.value.clone(), e.source));
    assert_eq!(list(&st.trust), Some((v(&["internal"]), Source::Config)));
    assert_eq!(list(&st.allow), Some((v(&["write_*"]), Source::Config)));
    assert_eq!(list(&st.mcp_server), Some((v(&["a=b"]), Source::Config)));
    assert_eq!(list(&st.on_pretool), Some((v(&["p"]), Source::Config)));
    assert_eq!(list(&st.on_posttool), Some((v(&["q"]), Source::Config)));
    assert_eq!(list(&st.on_turnend), Some((v(&["r"]), Source::Config)));
    let skill = st.skill.unwrap();
    assert_eq!(
        (skill.value, skill.source),
        (vec![PathBuf::from("s/SKILL.md")], Source::Config)
    );
    for b in [&st.plan_mode, &st.no_auto_mcp, &st.no_skills] {
        assert_eq!((b.value, b.source), (true, Source::Config));
    }
    assert_eq!(st.verify.unwrap().value, "cargo test");
    assert_eq!(
        (st.verify_timeout.value, st.verify_timeout.source),
        (5, Source::Config)
    );
}

#[test]
fn no_config_keeps_every_security_setting_at_its_default() {
    let st = Settings::resolve(&Flags::default(), &Config::default(), &env_of(&[]));
    assert!(st.trust.is_none() && st.allow.is_none() && st.mcp_server.is_none());
    assert!(st.on_pretool.is_none() && st.on_posttool.is_none() && st.on_turnend.is_none());
    assert!(st.skill.is_none() && st.verify.is_none());
    for b in [&st.plan_mode, &st.no_auto_mcp, &st.no_skills] {
        assert_eq!((b.value, b.source), (false, Source::Default));
    }
    assert_eq!(
        (st.verify_timeout.value, st.verify_timeout.source),
        (DEFAULT_VERIFY_TIMEOUT_SECS, Source::Default)
    );
    assert_eq!(DEFAULT_VERIFY_TIMEOUT_SECS, 300);
}

#[test]
fn a_flag_list_replaces_the_config_list_wholesale_for_every_list_key() {
    let c = cfg(SECURITY_SET);
    let f = Flags {
        trust: v(&["read_only"]),
        allow: v(&["edit_file"]),
        mcp_server: v(&["x=y"]),
        on_pretool: v(&["fp"]),
        on_posttool: v(&["fq"]),
        on_turnend: v(&["fr"]),
        skill: vec![PathBuf::from("f/SKILL.md")],
        ..Default::default()
    };
    let st = Settings::resolve(&f, &c, &env_of(&[]));
    let flagged = |e: &ListEff<String>, want: &[&str]| {
        let e = e.as_ref().unwrap();
        assert_eq!((e.value.clone(), e.source), (v(want), Source::Flag));
    };
    flagged(&st.trust, &["read_only"]);
    flagged(&st.allow, &["edit_file"]);
    flagged(&st.mcp_server, &["x=y"]);
    flagged(&st.on_pretool, &["fp"]);
    flagged(&st.on_posttool, &["fq"]);
    flagged(&st.on_turnend, &["fr"]);
    let skill = st.skill.unwrap();
    assert_eq!(
        (skill.value, skill.source),
        (vec![PathBuf::from("f/SKILL.md")], Source::Flag)
    );
    // Per key: an unrelated list the flag did not give still comes from config.
    let only_allow = Flags {
        allow: v(&["edit_file"]),
        ..Default::default()
    };
    let st = Settings::resolve(&only_allow, &c, &env_of(&[]));
    assert_eq!(st.on_pretool.unwrap().source, Source::Config);
    assert_eq!(st.allow.unwrap().source, Source::Flag);
}

#[test]
fn trust_precedence_is_flag_then_tole_trust_env_then_config() {
    let c = cfg("trust = [\"internal\"]\n");
    let env = env_of(&[("TOLE_TRUST", "read_only, none")]);
    let f = Flags {
        trust: v(&["none"]),
        ..Default::default()
    };
    let t = Settings::resolve(&f, &c, &env).trust.unwrap();
    assert_eq!((t.value, t.source), (v(&["none"]), Source::Flag));
    let t = Settings::resolve(&Flags::default(), &c, &env)
        .trust
        .unwrap();
    assert_eq!(
        (t.value, t.source, t.env),
        (v(&["read_only", "none"]), Source::Env, Some("TOLE_TRUST"))
    );
    // An empty TOLE_TRUST is "unset": the config applies.
    let t = Settings::resolve(&Flags::default(), &c, &env_of(&[("TOLE_TRUST", " ")]))
        .trust
        .unwrap();
    assert_eq!((t.value, t.source), (v(&["internal"]), Source::Config));
}

#[test]
fn booleans_are_flag_or_config_and_never_forced_off() {
    let c = cfg("plan_mode = true\nno_skills = true\nno_auto_mcp = false\n");
    // No flag: config true applies and cannot be switched off from the
    // command line (there is no flag that sets false).
    let st = Settings::resolve(&Flags::default(), &c, &env_of(&[]));
    assert_eq!(
        (st.plan_mode.value, st.plan_mode.source),
        (true, Source::Config)
    );
    assert_eq!(
        (st.no_skills.value, st.no_skills.source),
        (true, Source::Config)
    );
    assert_eq!(
        (st.no_auto_mcp.value, st.no_auto_mcp.source),
        (false, Source::Config)
    );
    // The flag turns a config-false on and relabels a config-true.
    let f = Flags {
        plan_mode: true,
        no_auto_mcp: true,
        ..Default::default()
    };
    let st = Settings::resolve(&f, &c, &env_of(&[]));
    assert_eq!(
        (st.plan_mode.value, st.plan_mode.source),
        (true, Source::Flag)
    );
    assert_eq!(
        (st.no_auto_mcp.value, st.no_auto_mcp.source),
        (true, Source::Flag)
    );
}

#[test]
fn mission_verify_and_timeout_are_flag_then_config_then_default() {
    let c = cfg("[mission]\nverify = \"cfg-v\"\nverify_timeout = 10\n");
    let f = Flags {
        verify: s("flag-v"),
        verify_timeout: Some(20),
        ..Default::default()
    };
    let st = Settings::resolve(&f, &c, &env_of(&[]));
    assert_eq!(st.verify.unwrap().value, "flag-v");
    assert_eq!(
        (st.verify_timeout.value, st.verify_timeout.source),
        (20, Source::Flag)
    );
    let st = Settings::resolve(&Flags::default(), &c, &env_of(&[]));
    assert_eq!(st.verify.unwrap().source, Source::Config);
    assert_eq!(
        (st.verify_timeout.value, st.verify_timeout.source),
        (10, Source::Config)
    );
    // A blank config `verify` is unset.
    let st = Settings::resolve(
        &Flags::default(),
        &cfg("[mission]\nverify = \" \"\n"),
        &env_of(&[]),
    );
    assert!(st.verify.is_none());
}

#[test]
fn check_shows_effective_value_and_source_for_every_key() {
    let env = env_of(&[("TOLE_MODEL", "env-model")]);
    let src = format!(
        "model = \"m\"\nbase_url = \"https://b\"\nsessions_dir = \"sd\"\nworkspace = \"w\"\n\
         memory = \"uteke\"\nsystem_prompt = \"sp\"\n{}",
        SECURITY_SET.replacen(
            "[mission]\n",
            "[mission]\nmax_steps = 1\nmax_minutes = 2\nmax_tokens = 3\n",
            1
        )
    );
    let c = cfg(&src);
    let flags = Flags {
        allow: v(&["edit_file"]),
        plan_mode: true,
        ..Default::default()
    };
    let st = Settings::resolve(&flags, &c, &env);
    let lines = crate::config::render_with(&c, &annotations(&st));
    let joined = lines.join("\n");
    assert!(!joined.contains("not applied"), "{joined}");
    for k in [
        "model",
        "base_url",
        "sessions_dir",
        "workspace",
        "system_prompt",
        "mission.max_steps",
        "mission.max_minutes",
        "mission.max_tokens",
        "plan_mode",
        "trust",
        "allow",
        "mcp_server",
        "no_auto_mcp",
        "on_pretool",
        "on_posttool",
        "on_turnend",
        "skill",
        "no_skills",
        "mission.verify",
        "mission.verify_timeout",
    ] {
        let line = lines
            .iter()
            .find(|l| l.starts_with(&format!("{k} = ")))
            .unwrap_or_else(|| panic!("{k} missing in\n{joined}"));
        // Keys whose feature is compiled out are shown as unsupported by
        // this build (startup refuses them) instead of an effective value.
        let gated_off = (cfg!(not(feature = "mcp")) && matches!(k, "mcp_server" | "no_auto_mcp"))
            || (cfg!(not(feature = "shell-tools"))
                && matches!(k, "on_pretool" | "on_posttool" | "on_turnend"));
        if gated_off {
            assert!(line.contains("not supported by this build"), "{line}");
        } else {
            assert!(line.contains("effective:"), "{line}");
        }
    }
    let line = |k: &str| {
        lines
            .iter()
            .find(|l| l.starts_with(&format!("{k} = ")))
            .unwrap()
            .clone()
    };
    // List key: the flag list replaced the config list; source shown.
    assert!(
        line("allow").contains("effective: [\"edit_file\"]  (flag)"),
        "{}",
        line("allow")
    );
    assert!(
        line("trust").contains("effective: [\"internal\"]  (config)"),
        "{}",
        line("trust")
    );
    // Boolean keys: effective + source + the only-on rule.
    assert!(line("plan_mode").contains("effective: true  (flag)"));
    assert!(line("no_skills").contains("effective: true  (config)"));
    assert!(line("no_skills").contains("flags can only turn this on"));
    assert!(line("mission.verify_timeout").contains("effective: 5  (config)"));
    assert!(joined.contains("model = \"m\"  [config]  effective: \"env-model\"  (env TOLE_MODEL)"));
    assert!(joined.contains("sessions_dir = \"sd\"  [config]  effective: \"sd\"  (config)"));
}

#[test]
fn unsupported_keys_are_loud_per_missing_feature() {
    let c = cfg(SECURITY_SET);
    // Full build: nothing unsupported.
    assert_eq!(unsupported_keys(&c, true, true), None);
    // No mcp: mcp_server named first.
    let m = unsupported_keys(&c, false, true).unwrap();
    assert!(
        m.contains("`mcp_server`") && m.contains("`mcp` feature"),
        "{m}"
    );
    assert!(m.contains("--no-config"), "{m}");
    let m = unsupported_keys(&cfg("no_auto_mcp = true\n"), false, true).unwrap();
    assert!(m.contains("`no_auto_mcp`"), "{m}");
    // No shell-tools: hooks / memory named.
    for (src, key) in [
        ("on_pretool = [\"p\"]\n", "on_pretool"),
        ("on_posttool = [\"p\"]\n", "on_posttool"),
        ("on_turnend = [\"p\"]\n", "on_turnend"),
        ("memory = \"uteke\"\n", "memory"),
    ] {
        let m = unsupported_keys(&cfg(src), true, false).unwrap();
        assert!(
            m.contains(&format!("`{key}`")) && m.contains("`shell-tools`"),
            "{m}"
        );
    }
    // Keys that need no feature are fine in the bare profile.
    let bare_ok = cfg("plan_mode = true\ntrust = [\"internal\"]\nallow = [\"x\"]\nskill = [\"s\"]\nno_skills = true\n[mission]\nverify = \"v\"\n");
    assert_eq!(unsupported_keys(&bare_ok, false, false), None);
    assert_eq!(unsupported_keys(&Config::default(), false, false), None);
}

// ------------------------------------------------------------- startup

struct Fake {
    interactive: bool,
    answer: bool,
    shown: String,
    confirms: usize,
}

impl Fake {
    fn new(interactive: bool, answer: bool) -> Fake {
        Fake {
            interactive,
            answer,
            shown: String::new(),
            confirms: 0,
        }
    }
}

impl ConfigTrustIo for Fake {
    fn is_interactive(&self) -> bool {
        self.interactive
    }
    fn inform(&mut self, text: &str) {
        self.shown.push_str(text);
        self.shown.push('\n');
    }
    fn warn(&mut self, text: &str) {
        self.inform(text)
    }
    fn confirm(&mut self, p: &str) -> bool {
        self.confirms += 1;
        self.shown.push_str(p);
        self.answer
    }
}

fn project(content: &str) -> (PathBuf, PathBuf) {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(CONFIG_REL_PATH), content).unwrap();
    let store = tmpdir()
        .join("home")
        .join("tole")
        .join("trusted-configs.json");
    (d, store)
}

fn no_store() -> Result<PathBuf> {
    panic!("the trust store must not be resolved here")
}

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
    let (d, store) = project("model = \"m\"\n");
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
    let (d, store) = project("model = \"m\"\n");
    crate::config_trust::record(&store, &d, "model = \"m\"\n").unwrap();
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
    let (d, store) = project("model = \"m\"\n");
    crate::config_trust::record(&store, &d, "model = \"m\"\n").unwrap();
    std::fs::write(d.join(CONFIG_REL_PATH), "model = \"evil\"\n").unwrap();
    let mut io = Fake::new(false, false);
    assert!(load_project_config(&d, None, &|| Ok(store.clone()), &mut io).is_err());
}

#[test]
fn invalid_trusted_content_is_a_parse_error() {
    let (d, store) = project("modle = \"m\"\n");
    crate::config_trust::record(&store, &d, "modle = \"m\"\n").unwrap();
    let mut io = Fake::new(false, false);
    let e = load_project_config(&d, None, &|| Ok(store.clone()), &mut io).unwrap_err();
    assert!(format!("{e:#}").contains("modle"), "{e:#}");
    assert!(!io.shown.contains("using config"));
}

#[test]
fn interactive_yes_persists_and_loads() {
    let (d, store) = project("model = \"m\"\n");
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
            system_prompt: s("p")
        }
    );
    assert_eq!(
        Settings::fill_from(&Config::default()),
        ProviderFill::default()
    );
}
