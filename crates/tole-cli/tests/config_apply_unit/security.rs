use crate::support::*;
use std::path::PathBuf;
use tole_cli::config::apply::*;
use tole_cli::config::Config;

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
    let lines = tole_cli::config::render_with(&c, tole_cli::config::PROJECT_TAG, &annotations(&st));
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
