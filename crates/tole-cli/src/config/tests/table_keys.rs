//! Tests: the user-layer tables (`[tui]`, `[delegation]`, `[cron]`, `[hooks]`, ...).

use super::*;

#[test]
fn tui_table_parses_is_strict_and_only_the_user_layer_survives_layering() {
    let c = parse(
        "[tui]\ntheme = \"light\"\n[tui.colors]\ntool = \"blue bold\"\n",
        &p(),
    )
    .unwrap();
    let t = c.tui.as_ref().unwrap();
    assert_eq!(t.theme.as_deref(), Some("light"));
    assert_eq!(t.colors.as_ref().unwrap()["tool"], "blue bold");
    // Unknown keys and wrong types are loud, like any other key; a bad
    // VALUE is not (it is checked when the theme is built).
    assert!(parse("[tui]\nthemes = \"x\"\n", &p()).is_err());
    assert!(parse("[tui]\ntheme = 3\n", &p()).is_err());
    assert!(parse("[tui]\ntheme = \"neon\"\n", &p()).is_ok());
    // The project layer never reaches the merged config.
    let project = parse("[tui]\ntheme = \"mono\"\n", &p()).unwrap();
    let m = layered(&project, &Config::default());
    assert_eq!(m.tui, None);
    assert!(m.user_keys.is_empty());
    let m = layered(&project, &c);
    assert_eq!(m.tui, c.tui);
    assert!(m.user_keys.contains("tui.theme") && m.user_keys.contains("tui.colors"));
}

#[test]
fn delegation_table_is_bounded_and_user_layer_only() {
    let user = parse(
        "[delegation]\nmax_depth = 3\nmax_live_processes = 2\ngoal_budget = 9\n",
        &p(),
    )
    .unwrap();
    let d = user.delegation.clone().unwrap();
    assert_eq!(
        (d.max_depth, d.max_live_processes, d.goal_budget),
        (Some(3), Some(2), Some(9))
    );
    for bad in [
        "max_depth = 4",
        "max_depth = 0",
        "max_live_processes = 0",
        "goal_steps = 0",
        "depth = 2",
    ] {
        assert!(
            parse(&format!("[delegation]\n{bad}\n"), &p()).is_err(),
            "{bad}"
        );
    }
    let project = parse("[delegation]\nmax_depth = 3\n", &p()).unwrap();
    assert_eq!(layered(&project, &Config::default()).delegation, None);
    assert_eq!(layered(&project, &user).delegation, user.delegation);
    let shown = render(&project).join("\n");
    assert!(
        shown.contains("delegation.max_depth = 3") && shown.contains("IGNORED"),
        "{shown}"
    );
}

/// The resource-guard keys: bounded, user layer only, shown by
/// `config check`.
#[test]
fn delegation_resource_keys_are_bounded_and_user_layer_only() {
    let user = parse(
        "[delegation]\nmin_free_memory_mb = 2048\nchild_memory_mb = 256\n\
             max_load_per_cpu = 2.5\nmin_free_disk_mb = 4096\nmin_free_pids_percent = 20\n\
             spawn_interval_secs = 5\nmachine_max_live = 3\nchild_nice = 15\n",
        &p(),
    )
    .unwrap();
    let t = user
        .delegation
        .as_ref()
        .unwrap()
        .resource_thresholds()
        .unwrap();
    assert_eq!(
        (
            t.min_free_memory_mb,
            t.child_memory_mb,
            t.max_load_per_cpu,
            t.min_free_disk_mb,
            t.min_free_pids_percent,
            t.spawn_interval_secs,
            t.machine_max_live,
            t.child_nice
        ),
        (2048, 256, 2.5, 4096, 20, 5, 3, 15)
    );
    for bad in [
        "min_free_memory_mb = 0",
        "min_free_memory_mb = -1",
        "child_memory_mb = 0",
        "max_load_per_cpu = 0.0",
        "max_load_per_cpu = -2.0",
        "max_load_per_cpu = 1000.0",
        "min_free_disk_mb = 0",
        "min_free_pids_percent = 0",
        "min_free_pids_percent = 95",
        "spawn_interval_secs = 0",
        "machine_max_live = 0",
        "machine_max_live = 100000",
        "child_nice = 20",
        "child_nice = -5",
        "max_load_per_cpu = \"high\"",
    ] {
        assert!(
            parse(&format!("[delegation]\n{bad}\n"), &p()).is_err(),
            "{bad}"
        );
    }
    let project = parse("[delegation]\nmachine_max_live = 64\n", &p()).unwrap();
    assert_eq!(layered(&project, &Config::default()).delegation, None);
    let shown = render(&project).join("\n");
    assert!(
        shown.contains("delegation.machine_max_live = 64") && shown.contains("IGNORED"),
        "{shown}"
    );
    let shown = render(&user).join("\n");
    assert!(
        shown.contains("delegation.max_load_per_cpu = 2.5"),
        "{shown}"
    );
}

/// The approvals switch (#714): off unless set, bounded, user layer only.
#[test]
fn delegation_approvals_switch_defaults_off_and_is_bounded() {
    use tole_core::delegation_approvals::ApprovalsMode;
    let d = |src: &str| parse(src, &p()).map(|c| c.delegation.unwrap_or_default());
    assert_eq!(d("[delegation]\nmax_depth = 2\n").unwrap().approvals, None);
    let on = d("[delegation]\napprovals = \"chain\"\napproval_secs = 3600\n").unwrap();
    assert_eq!(on.approvals, Some(ApprovalsMode::Chain));
    for bad in [
        "approvals = \"agent\"",
        "approvals = true",
        "approval_secs = 0",
        "approval_secs = 3601",
        "approval_level_secs = 0",
        "approval_level_secs = 901",
        "approval_secs = 60\napproval_level_secs = 61",
    ] {
        assert!(d(&format!("[delegation]\n{bad}\n")).is_err(), "{bad}");
    }
    assert!(d("[delegation]\napproval_secs = 60\napproval_level_secs = 60\n").is_ok());
    let project = parse("[delegation]\napprovals = \"owner\"\n", &p()).unwrap();
    assert_eq!(layered(&project, &Config::default()).delegation, None);
    let shown = render(&project).join("\n");
    assert!(
        shown.contains("delegation.approvals = owner") && shown.contains("IGNORED"),
        "{shown}"
    );
}

#[test]
fn cron_table_is_strict_and_user_layer_only() {
    let user = parse("[cron]\ndaily_token_cap = 50000\n", &p()).unwrap();
    assert_eq!(user.cron.as_ref().unwrap().daily_token_cap, Some(50_000));
    assert!(parse("[cron]\ndaily_cap = 1\n", &p()).is_err());
    // Exempt from the secret-name rule only at that exact position.
    assert!(parse("[cron]\napi_token = \"x\"\n", &p()).is_err());
    assert!(parse("[mission]\ndaily_token_cap = 1\n", &p()).is_err());
    assert!(parse("[cron]\ndaily_token_cap = \"1\"\n", &p()).is_err());
    // #580: the zone keys; an unknown source names the accepted ones.
    let tz = parse(
        "[cron]\ntimezone = \"Asia/Jakarta\"\ntz_source = \"offset-only\"\n",
        &p(),
    )
    .unwrap()
    .cron
    .unwrap();
    assert_eq!(
        (tz.timezone.as_deref(), tz.tz_source),
        (Some("Asia/Jakarta"), Some(TzSource::OffsetOnly))
    );
    let e = parse("[cron]\ntz_source = \"bundled\"\n", &p())
        .unwrap_err()
        .to_string();
    assert!(e.contains("system") && e.contains("offset-only"), "{e}");
    let project = parse("[cron]\ndaily_token_cap = 999999999\n", &p()).unwrap();
    assert_eq!(layered(&project, &Config::default()).cron, None);
    let m = layered(&project, &user);
    assert_eq!(m.cron, user.cron);
    assert!(m.user_keys.contains("cron.daily_token_cap"));
    let shown = render(&project).join("\n");
    assert!(shown.contains("cron.daily_token_cap = 999999999") && shown.contains("IGNORED"));
}

/// #582: webhook routes come from the user config only. A project
/// config can neither define a route nor change a user route, and
/// `config check` shows route names, never where a key comes from.
#[test]
fn hooks_are_user_layer_only_and_shown_by_name() {
    let user = parse(
        "[hooks.gh]\nhmac_key_env = \"GH_HOOK_KEY\"\nrate_per_minute = 5\n",
        &p(),
    )
    .unwrap();
    let project = parse(
            "[hooks.gh]\ninsecure_loopback = true\nrate_per_minute = 600\n[hooks.evil]\nhmac_key_env = \"X\"\n",
            &p(),
        )
        .unwrap();
    assert_eq!(layered(&project, &Config::default()).hooks, None);
    assert_eq!(layered(&project, &user).hooks, user.hooks);
    assert!(parse("[hooks.gh]\nsecret = \"x\"\n", &p()).is_err());
    let shown = render(&user).join("\n");
    assert!(
        shown.contains("hooks = [1 entries]") && shown.contains("gh"),
        "{shown}"
    );
    assert!(!shown.contains("GH_HOOK_KEY"), "{shown}");
    assert!(render(&project).join("\n").contains("IGNORED"));
}

/// #911: outbound webhook destinations come from the user config only.
/// A project config can neither define one nor change a user one, a
/// misspelt or extra key is an error, and `config check` shows names, never
/// a URL or where a secret comes from.
#[test]
fn webhook_out_is_user_layer_only_strict_and_shown_by_name() {
    let user = parse(
        "[webhook_out.gateway]\nurl = \"https://hooks.example.com/secret-path-token\"\n\
             hmac_key_env = \"GW_SECRET_VAR\"\nevents = [\"cron_run_finished\"]\n",
        &p(),
    )
    .unwrap();
    let project = parse(
        "[webhook_out.gateway]\nurl = \"https://evil.example/\"\nhmac_key_env = \"X\"\n\
             [webhook_out.evil]\nurl = \"https://evil.example/\"\nhmac_key_env = \"X\"\n",
        &p(),
    )
    .unwrap();
    assert_eq!(layered(&project, &Config::default()).webhook_out, None);
    assert_eq!(layered(&project, &user).webhook_out, user.webhook_out);
    // Unknown field: an error, so a typo never silently drops a setting.
    assert!(parse("[webhook_out.gw]\nsecret = \"x\"\n", &p()).is_err());
    assert!(parse("[webhook_out.gw]\ntimeout = 5\n", &p()).is_err());
    let shown = render(&user).join("\n");
    assert!(
        shown.contains("webhook_out = [1 entries]") && shown.contains("gateway"),
        "{shown}"
    );
    assert!(
        !shown.contains("secret-path-token") && !shown.contains("GW_SECRET_VAR"),
        "{shown}"
    );
    assert!(render(&project).join("\n").contains("IGNORED"));
    assert!(webhook_out_ignored_warning("p.toml").contains("user config"));
}

/// #579: `cron.max_concurrent` is a bounded, user-layer-only key.
#[test]
fn cron_max_concurrent_is_bounded_and_user_layer_only() {
    let user = parse("[cron]\nmax_concurrent = 8\n", &p()).unwrap();
    assert_eq!(user.cron.as_ref().unwrap().max_concurrent, Some(8));
    for bad in ["0", "65", "-1", "\"4\""] {
        assert!(
            parse(&format!("[cron]\nmax_concurrent = {bad}\n"), &p()).is_err(),
            "{bad}"
        );
    }
    let project = parse("[cron]\nmax_concurrent = 64\n", &p()).unwrap();
    assert_eq!(layered(&project, &Config::default()).cron, None);
    let m = layered(&project, &user);
    assert_eq!(m.cron, user.cron);
    assert!(m.user_keys.contains("cron.max_concurrent"));
    let shown = render(&project).join("\n");
    assert!(shown.contains("cron.max_concurrent = 64") && shown.contains("IGNORED"));
}

/// #593: `[curator]` is strict, bounded, and user layer only.
#[test]
fn curator_table_is_strict_bounded_and_user_layer_only() {
    let user = parse(
        "[curator]\nenabled = false\nstale_days = 7\narchive_days = 21\ngrace_days = 5\n",
        &p(),
    )
    .unwrap();
    let c = user.curator.as_ref().unwrap();
    assert_eq!(
        (c.enabled, c.stale_days, c.archive_days, c.grace_days),
        (Some(false), Some(7), Some(21), Some(5))
    );
    let th = curator_thresholds(Some(c));
    assert_eq!((th.stale_days, th.archive_days, th.grace_days), (7, 21, 5));
    let d = curator_thresholds(None);
    assert_eq!((d.stale_days, d.archive_days, d.grace_days), (14, 30, 30));

    assert!(
        parse("[curator]\nenable = true\n", &p()).is_err(),
        "unknown key"
    );
    assert!(parse("[curator]\nenabled = \"yes\"\n", &p()).is_err());
    assert!(parse("[curator]\nstale_days = -1\n", &p()).is_err());
    assert!(
        parse("[curator]\nstale_days = 3651\n", &p()).is_err(),
        "too long"
    );
    // The pair is checked with defaults filled in: one key alone can
    // leave archive_days at or below stale_days.
    for bad in [
        "stale_days = 30",
        "archive_days = 14",
        "stale_days = 0",
        "stale_days = 20\narchive_days = 20",
    ] {
        let e = parse(&format!("[curator]\n{bad}\n"), &p()).unwrap_err();
        assert!(e.to_string().contains("archive_days"), "{bad}: {e}");
    }
    assert!(parse("[curator]\nstale_days = 20\narchive_days = 21\n", &p()).is_ok());

    // A project config can neither switch it off nor shorten a skill's life.
    let project = parse(
        "[curator]\nenabled = false\nstale_days = 1\narchive_days = 2\n",
        &p(),
    )
    .unwrap();
    assert_eq!(layered(&project, &Config::default()).curator, None);
    let m = layered(&project, &user);
    assert_eq!(m.curator, user.curator);
    assert!(m.user_keys.contains("curator.enabled"));
    assert!(m.user_keys.contains("curator.archive_days"));
    let shown = render(&project).join("\n");
    assert!(
        shown.contains("curator.enabled = false") && shown.contains("IGNORED"),
        "{shown}"
    );
}

#[test]
fn profiles_parse_strictly_and_only_the_user_layer_survives_layering() {
    let src = "[profiles.coder]\nallow = [\"write_*\"]\nmemory_room = \"profile-code\"\n\
                   grants = [\"GITHUB_TOKEN\"]\n[profiles.coder.models]\nstandard = \"m\"\n";
    let c = parse(src, &p()).unwrap();
    let coder = &c.profiles.as_ref().unwrap()["coder"];
    assert_eq!(coder.allow.as_deref(), Some(&["write_*".to_string()][..]));
    assert_eq!(coder.models.as_ref().unwrap().default_tier(), "standard");
    // Unknown keys, bad names, bad rooms and impossible defaults are loud.
    for (bad, want) in [
        ("[profiles.coder]\nmodel = \"m\"\n", "unknown field"),
        ("[profiles.Coder]\n", "profile name `Coder`"),
        ("[profiles.\"-x\"]\n", "profile name `-x`"),
        ("[profiles.a]\nmemory_room = \"a b\"\n", "memory_room"),
        (
            "[profiles.a.models]\nlight = \"s\"\n",
            "models.standard is not set",
        ),
        (
            "[profiles.a.models]\nadvisor = \"s\"\ndefault = \"advisor\"\n",
            "light, standard or heavy",
        ),
    ] {
        let e = parse(bad, &p()).unwrap_err().to_string();
        assert!(e.contains(want), "{bad:?}: {e}");
    }
    // A grant is a NAME; a value pasted by mistake is refused and never echoed.
    let e = parse("[profiles.a]\ngrants = [\"ghp-abcdef123456\"]\n", &p())
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("environment variable NAME") && !e.contains("ghp-abcdef"),
        "{e}"
    );
    // The secret-key guard still applies inside a profile.
    assert!(parse("[profiles.a]\napi_key = \"x\"\n", &p()).is_err());
    // The project layer never supplies profiles.
    let m = layered(&c, &Config::default());
    assert_eq!(m.profiles, None);
    let m = layered(&Config::default(), &c);
    assert_eq!(m.profiles, c.profiles);
    let lines = render(&c);
    assert!(lines.iter().any(|l| l
            == "profiles = [1 entries]  [config]  IGNORED: a project config cannot define profiles (user config only)"));
    assert!(lines.contains(&"  - \"coder\"".to_string()));
}

#[test]
fn tui_keys_parse_as_string_or_list_are_type_strict_and_user_layer_only() {
    let c = parse(
        "[tui.keys]\ntoggle_tools = \"ctrl+o\"\nnewline = [\"shift+enter\", \"alt+n\"]\n",
        &p(),
    )
    .unwrap();
    let k = c.tui.as_ref().unwrap().keys.as_ref().unwrap();
    assert_eq!(k["toggle_tools"].to_vec(), vec!["ctrl+o"]);
    assert_eq!(k["newline"].to_vec(), vec!["shift+enter", "alt+n"]);
    // Wrong types are loud; a bad action NAME or spec is not (it is
    // checked, with a note, when the keymap is built).
    assert!(parse("[tui.keys]\ntoggle_tools = 3\n", &p()).is_err());
    assert!(parse("[tui.keys]\ntoggle_tools = [1]\n", &p()).is_err());
    assert!(parse("[tui]\nkeys = \"x\"\n", &p()).is_err());
    assert!(parse("[tui.keys]\nno_such_action = \"ctrl+o\"\n", &p()).is_ok());
    // The project layer never reaches the merged config.
    let project = parse("[tui.keys]\ntoggle_tools = \"f5\"\n", &p()).unwrap();
    let m = layered(&project, &Config::default());
    assert_eq!(m.tui, None);
    assert!(m.user_keys.is_empty());
    let m = layered(&project, &c);
    assert_eq!(m.tui, c.tui);
    assert!(m.user_keys.contains("tui.keys"));
    // `config check` rendering lists the entries.
    let lines = render(&c);
    assert!(lines
        .iter()
        .any(|l| l.starts_with("tui.keys = [2 entries]  [config]")));
    assert!(lines.contains(&"  - newline = [\"shift+enter\", \"alt+n\"]".to_string()));
}
