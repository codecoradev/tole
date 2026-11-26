//! Tests: file format, validation, discovery, layering, secrets.

use super::*;

#[test]
fn full_file_round_trips() {
    let c = parse(FULL, &p()).unwrap();
    assert_eq!(c.model.as_deref(), Some("m"));
    assert_eq!(c.plan_mode, Some(true));
    assert_eq!(c.no_skills, Some(false));
    assert_eq!(c.trust.as_deref().unwrap(), ["internal", "none"]);
    assert_eq!(c.skill.as_deref().unwrap(), ["skills/x/SKILL.md"]);
    let m = c.mission.unwrap();
    assert_eq!(
        (m.max_steps, m.max_minutes, m.max_tokens, m.verify_timeout),
        (Some(10), Some(5), Some(1000), Some(60))
    );
    assert_eq!(m.verify.as_deref(), Some("cargo test"));
    let lines = render(&parse(FULL, &p()).unwrap());
    assert!(lines.contains(&"model = \"m\"  [config]".to_string()));
    assert!(lines.contains(&"  - \"internal\"".to_string()));
    assert!(lines.contains(&"mission.max_tokens = 1000  [config]".to_string()));
}

#[test]
fn review_gate_is_validated_and_a_project_can_only_tighten_it() {
    assert_eq!(
        parse("review_gate = \"warn\"\n", &p())
            .unwrap()
            .review_gate
            .as_deref(),
        Some("warn")
    );
    let e = parse("review_gate = \"maybe\"\n", &p()).unwrap_err();
    assert!(e.message.contains("review_gate must be"), "{e}");
    let with = |v: &str| parse(&format!("review_gate = \"{v}\"\n"), &p()).unwrap();
    let eff = |proj: Option<&str>, user: Option<&str>| {
        let proj = proj.map(with).unwrap_or_default();
        let user = user.map(with).unwrap_or_default();
        let m = layered(&proj, &user);
        (m.review_gate.clone(), m.user_keys.contains("review_gate"))
    };
    let s = |v: &str| Some(v.to_string());
    // A project that loosens what the user chose loses to the user.
    assert_eq!(eff(Some("off"), Some("block")), (s("block"), true));
    assert_eq!(eff(Some("warn"), Some("block")), (s("block"), true));
    // A project that tightens wins; equal is the project's.
    assert_eq!(eff(Some("block"), Some("off")), (s("block"), false));
    assert_eq!(eff(Some("warn"), Some("warn")), (s("warn"), false));
    // One layer only: used as given (the project file is trust-gated).
    assert_eq!(eff(None, Some("off")), (s("off"), true));
    assert_eq!(eff(Some("off"), None), (s("off"), false));
    assert_eq!(eff(None, None), (None, false));
}

#[test]
fn reasoning_effort_is_validated_and_layered() {
    let c = parse("reasoning_effort = \"high\"\n", &p()).unwrap();
    assert_eq!(c.reasoning_effort.as_deref(), Some("high"));
    let lines = render(&c);
    assert!(lines.contains(&"reasoning_effort = \"high\"  [config]".to_string()));
    let e = parse("reasoning_effort = \"max\"\n", &p()).unwrap_err();
    assert!(e.message.contains("minimal | low | medium | high"), "{e}");
    let user = parse("reasoning_effort = \"low\"\n", &p()).unwrap();
    let none = Config::default();
    let m = layered(&none, &user);
    assert_eq!(m.reasoning_effort.as_deref(), Some("low"));
    assert!(m.user_keys.contains("reasoning_effort"));
    assert_eq!(
        layered(&c, &user).reasoning_effort.as_deref(),
        Some("high"),
        "project wins key by key"
    );
}

#[test]
fn compaction_keys_are_range_checked() {
    let c = parse(FULL, &p()).unwrap();
    assert_eq!(
        (c.context_window, c.compact_at, c.no_compaction),
        (Some(131072), Some(0.75), Some(false))
    );
    for bad in [
        "compact_at = 0.0\n",
        "compact_at = 1.5\n",
        "compact_at = -0.2\n",
    ] {
        let e = parse(bad, &p()).unwrap_err();
        assert!(e.message.contains("compact_at"), "{bad} -> {e}");
    }
    assert!(parse("compact_at = 1.0\n", &p()).is_ok());
    assert!(parse("context_window = 0\n", &p()).is_err());
    assert!(parse("context_window = -1\n", &p()).is_err());
}

#[test]
fn empty_file_is_valid() {
    assert_eq!(parse("", &p()).unwrap(), Config::default());
    assert!(render(&Config::default()).is_empty());
}

#[test]
fn unknown_key_is_rejected_naming_it() {
    let e = parse("modle = \"x\"\n", &p()).unwrap_err();
    assert!(e.message.contains("modle"), "{e}");
    assert_eq!(e.line_col.map(|l| l.0), Some(1));
    let e = parse("[mission]\nmax_step = 1\n", &p()).unwrap_err();
    assert!(e.message.contains("max_step"), "{e}");
}

#[test]
fn forbidden_secret_keys_are_hard_errors() {
    for src in [
        "api_key = \"x\"\n",
        "serve_token = \"x\"\n",
        "token = \"x\"\n",
        "my_secret = \"x\"\n",
        "db_password = \"x\"\n",
        "API_KEY = \"x\"\n",
        "Api-Key = \"x\"\n",
        "[mission]\ntoken = \"x\"\n",
        "[mission]\nverify_secret = \"x\"\n",
        "[a.b]\nPassword = \"x\"\n",
        "x = [{ ok = 1, auth_token = \"x\" }]\n",
    ] {
        let e = parse(src, &p()).unwrap_err();
        assert!(
            e.message
                .contains("secrets must never live in a config file"),
            "{src:?} -> {e}"
        );
        assert!(e.line_col.is_some(), "{src:?} -> {e}");
    }
    let e = parse("[mission]\nverify_secret = \"x\"\n", &p()).unwrap_err();
    assert!(e.message.contains("verify_secret"));
}

#[test]
fn mission_max_tokens_is_not_mistaken_for_a_secret() {
    assert!(parse("[mission]\nmax_tokens = 5\n", &p()).is_ok());
    // Only in its exact position: top-level it is a secret-like name.
    assert!(parse("max_tokens = 5\n", &p()).is_err());
}

#[test]
fn wrong_type_is_rejected_with_line_col() {
    let e = parse("model = \"m\"\nplan_mode = \"yes\"\n", &p()).unwrap_err();
    assert_eq!(e.line_col.map(|l| l.0), Some(2), "{e}");
    assert!(e.to_string().starts_with("cfg.toml:2:"), "{e}");
    assert!(parse("[mission]\nmax_steps = -1\n", &p()).is_err());
    assert!(parse("[mission]\nmax_steps = \"3\"\n", &p()).is_err());
    assert!(parse("allow = \"read_file\"\n", &p()).is_err());
}

#[test]
fn trust_presets_use_the_shared_rule() {
    assert!(parse("trust = [\"internal\", \"read_only\", \"none\"]\n", &p()).is_ok());
    assert!(parse("trust = [\"INTERNAL\"]\n", &p()).is_ok());
    let e = parse("trust = [\"internal\", \"internalx\"]\n", &p()).unwrap_err();
    assert!(e.message.contains("unknown trust preset"), "{e}");
    assert!(e.message.contains("internalx"), "{e}");
    assert_eq!(e.line_col.map(|l| l.0), Some(1));
    // Same function as the flag: identical message.
    let flag = crate::trust::expand_trust(&["internalx".into()]).unwrap_err();
    assert_eq!(e.message, flag.to_string());
}

#[test]
fn malformed_toml_reports_line_and_col() {
    let e = parse("model = \"m\"\nallow = [\n", &p()).unwrap_err();
    assert!(e.line_col.is_some(), "{e}");
    let e = parse("model = \"m\"\nbad line here\n", &p()).unwrap_err();
    let s = e.to_string();
    assert!(s.starts_with("cfg.toml:2:"), "{s}");
}

#[test]
fn oversized_file_is_rejected_without_full_read() {
    let d = tmpdir();
    let f = d.join("big.toml");
    let big = format!("system_prompt = \"{}\"\n", "a".repeat(70 * 1024));
    std::fs::write(&f, big).unwrap();
    let e = load(&f).unwrap_err();
    assert!(e.message.contains("larger than the 64 KiB limit"), "{e}");
    // Exactly at the cap is allowed by the size gate.
    let ok = d.join("ok.toml");
    let prefix = "system_prompt = \"";
    let pad = MAX_CONFIG_BYTES as usize - prefix.len() - 2;
    std::fs::write(&ok, format!("{prefix}{}\"\n", "b".repeat(pad))).unwrap();
    assert_eq!(
        std::fs::metadata(&ok).unwrap().len(),
        MAX_CONFIG_BYTES,
        "fixture must sit exactly at the cap"
    );
    assert!(load(&ok).is_ok());
    // The bounded reader never pulls more than cap+1 bytes.
    let huge = d.join("huge.toml");
    let f = std::fs::File::create(&huge).unwrap();
    f.set_len(64 * 1024 * 1024).unwrap();
    let t = std::time::Instant::now();
    assert!(load(&huge).is_err());
    assert!(t.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn non_utf8_is_rejected() {
    let d = tmpdir();
    let f = d.join("bin.toml");
    std::fs::write(&f, [0xff, 0xfe, 0x00]).unwrap();
    assert!(load(&f).unwrap_err().message.contains("UTF-8"));
}

#[test]
fn discovery_is_cwd_only_and_explicit_wins() {
    let d = tmpdir();
    assert_eq!(discover(&d, None), None);
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(".tole/config.toml"), "").unwrap();
    assert_eq!(discover(&d, None), Some(d.join(".tole/config.toml")));
    // No parent walk.
    let child = d.join("sub");
    std::fs::create_dir_all(&child).unwrap();
    assert_eq!(discover(&child, None), None);
    let other = d.join("other.toml");
    assert_eq!(discover(&d, Some(&other)), Some(other));
}

#[test]
fn check_reports_absent_valid_and_missing_explicit() {
    let d = tmpdir();
    let none = |_: &Config| Annotations::new();
    let out = check(&d, None, None, &none).unwrap();
    assert!(out.contains("\nno .tole/config.toml in "), "{out}");
    let f = d.join("c.toml");
    std::fs::write(&f, "model = \"m\"\n").unwrap();
    let out = check(&d, Some(&f), None, &none).unwrap();
    assert!(out.contains("model = \"m\"  [config]"), "{out}");
    assert!(check(&d, Some(&d.join("nope.toml")), None, &none).is_err());
}

#[test]
fn check_shows_both_layers() {
    let d = tmpdir();
    let none = |_: &Config| Annotations::new();
    let user = d.join("user.toml");
    let out = check(&d, None, Some(&user), &none).unwrap();
    assert!(out.starts_with("no user config at "), "{out}");
    std::fs::write(&user, "model = \"u\"\nskill = [\"g/SKILL.md\"]\n").unwrap();
    let proj = d.join("p.toml");
    std::fs::write(&proj, "model = \"p\"\n").unwrap();
    let out = check(&d, Some(&proj), Some(&user), &|c| {
        // The annotation sees the layered config.
        assert_eq!(c.model.as_deref(), Some("p"));
        assert!(c.user_keys.contains("skill") && !c.user_keys.contains("model"));
        Annotations::new()
    })
    .unwrap();
    assert!(out.starts_with("user config: "), "{out}");
    assert!(out.contains("model = \"u\"  [user config]"), "{out}");
    assert!(out.contains("model = \"p\"  [config]"), "{out}");
    // A broken user config is an error, like a broken project config.
    std::fs::write(&user, "api_key = \"x\"\n").unwrap();
    assert!(check(&d, None, Some(&user), &none).is_err());
}

#[test]
fn project_layer_wins_key_by_key() {
    let user = parse(
        "model = \"u\"\nbase_url = \"http://u\"\nskill = [\"g\"]\nplan_mode = true\n\
             [mission]\nmax_steps = 9\nverify = \"u\"\n",
        &p(),
    )
    .unwrap();
    let project = parse(
        "model = \"p\"\nbase_url = \"  \"\nskill = []\nplan_mode = false\n\
             [mission]\nverify = \"p\"\n",
        &p(),
    )
    .unwrap();
    let c = layered(&project, &user);
    assert_eq!(c.model.as_deref(), Some("p"));
    // Blank string / empty list in the project count as unset.
    assert_eq!(c.base_url.as_deref(), Some("http://u"));
    assert_eq!(c.skill.as_deref().unwrap(), ["g"]);
    // An explicit project `false` is set: it wins over the user `true`.
    assert_eq!(c.plan_mode, Some(false));
    let m = c.mission.clone().unwrap();
    assert_eq!((m.max_steps, m.verify.as_deref()), (Some(9), Some("p")));
    let expected: BTreeSet<&str> = ["base_url", "skill", "mission.max_steps"].into();
    assert_eq!(c.user_keys, expected);
    // No layers at all: nothing set, no mission table.
    assert_eq!(
        layered(&Config::default(), &Config::default()),
        Config::default()
    );
    // One layer alone passes through.
    let only_user = layered(&Config::default(), &user);
    assert_eq!(only_user.model.as_deref(), Some("u"));
    assert!(only_user.user_keys.contains("model"));
}

#[test]
fn sensitive_files_are_validated_and_a_project_can_only_add() {
    let user = parse("sensitive_files = [\"*.secret\"]\n", &p()).unwrap();
    let project = parse("sensitive_files = [\"vault-*.json\", \"*.secret\"]\n", &p()).unwrap();
    let l = layered(&project, &user);
    assert_eq!(
        l.sensitive_files,
        Some(vec!["*.secret".to_string(), "vault-*.json".to_string()])
    );
    // A project with an empty list does not drop the user's.
    let empty = parse("sensitive_files = []\n", &p()).unwrap();
    assert_eq!(
        layered(&empty, &user).sensitive_files,
        Some(vec!["*.secret".to_string()])
    );
    for bad in ["\"\"", "\"*\"", "\"a/b.env\"", "\"a\\\\b\""] {
        let e = parse(&format!("sensitive_files = [{bad}]\n"), &p()).unwrap_err();
        assert!(e.message.contains("sensitive_files"), "{bad}: {e}");
    }
}

#[test]
fn redaction_allowlist_is_validated_and_user_layer_only() {
    let user = parse("redaction_allowlist = [\"^dev-\"]\n", &p()).unwrap();
    let project = parse("redaction_allowlist = [\".*\"]\n", &p()).unwrap();
    assert_eq!(
        layered(&project, &user).redaction_allowlist,
        Some(vec!["^dev-".to_string()]),
        "a project cannot exempt its own values"
    );
    assert_eq!(
        layered(&project, &parse("", &p()).unwrap()).redaction_allowlist,
        None
    );
    let w = redaction_allowlist_ignored_warning("p.toml");
    assert!(
        w.contains("redaction_allowlist") && w.contains("user config"),
        "{w}"
    );
    let e = parse("redaction_allowlist = [\"(\"]\n", &p()).unwrap_err();
    assert!(e.message.contains("redaction_allowlist"), "{e}");
}

#[test]
fn long_system_prompt_is_truncated_for_display() {
    let c = parse(&format!("system_prompt = \"{}\"\n", "x".repeat(200)), &p()).unwrap();
    let line = &render(&c)[0];
    assert!(line.contains("… (200 chars)"), "{line}");
    assert!(line.len() < 150, "{line}");
}

#[test]
fn secrets_bind_names_to_tools_and_a_project_can_only_narrow_them() {
    let p = |src: &str| parse(src, Path::new("c.toml"));
    let user = p(
        "[secrets]\nGITHUB_TOKEN = { tools = [\"gh\", \"git\", \"run_command\"] }\n\
                      NPM_TOKEN = { tools = [\"run_command\"] }\n",
    )
    .unwrap();
    // A key that is a secret NAME is not a forbidden key.
    let r = user.release();
    assert!(r.allows("GITHUB_TOKEN", "run_command"));
    assert!(r.allows("NPM_TOKEN", "run_command"));
    // A project tries to widen (job_start, a new secret) and narrows git.
    let project = p(
        "[secrets]\nGITHUB_TOKEN = { tools = [\"gh\", \"job_start\"] }\n\
                         AWS_SECRET_ACCESS_KEY = { tools = [\"run_command\"] }\n",
    )
    .unwrap();
    let l = layered(&project, &user).release();
    assert!(l.allows("GITHUB_TOKEN", "gh"));
    assert!(!l.allows("GITHUB_TOKEN", "job_start"), "widening refused");
    assert!(!l.allows("GITHUB_TOKEN", "run_command"), "narrowed");
    assert!(!l.allows("AWS_SECRET_ACCESS_KEY", "run_command"));
    let w = secrets_widening_warnings(&project, &user, "p").join("\n");
    assert!(w.contains("GITHUB_TOKEN to job_start"), "{w}");
    assert!(w.contains("AWS_SECRET_ACCESS_KEY to run_command"), "{w}");
    assert!(w.contains("in your user config"), "{w}");
    // Without a user entry, the default binding is the ceiling.
    let l = layered(&project, &Config::default()).release();
    assert!(l.allows("GITHUB_TOKEN", "gh"));
    assert!(!l.allows("GITHUB_TOKEN", "git"));
    assert!(!l.allows("GITHUB_TOKEN", "job_start"));
    // Bad entries name the remedy and never echo a key or value.
    for (src, needle) in [
        (
            "[secrets]\nPATH = { tools = [\"gh\"] }\n",
            "NAME of a secret",
        ),
        (
            "[secrets]\nghp_abcdefTOKEN = { tools = [\"gh\"] }\n",
            "NAME of a secret",
        ),
        (
            "[secrets]\nGITHUB_TOKEN = { tools = [\"Run Command\"] }\n",
            "tool name",
        ),
        (
            "[secrets]\nGITHUB_TOKEN = { tools = [\"gh\"], value = \"x\" }\n",
            "unknown field",
        ),
    ] {
        let e = p(src).unwrap_err().to_string();
        assert!(e.contains(needle), "{src}: {e}");
        assert!(!e.contains("ghp_abcdef"), "{e}");
    }
    // A secret-like key anywhere else is still forbidden.
    let e = p("[mission]\napi_token = \"x\"\n").unwrap_err().to_string();
    assert!(e.contains("forbidden key"), "{e}");
    // `config check` shows the binding, and on a project line the rule.
    let lines = render_with(&project, PROJECT_TAG, &Annotations::new()).join("\n");
    assert!(
        lines.contains("secrets.GITHUB_TOKEN = [\"gh\", \"job_start\"]"),
        "{lines}"
    );
    assert!(lines.contains("can only narrow"), "{lines}");
}
