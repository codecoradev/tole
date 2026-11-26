//! Tests: the `[sandbox]` table, its profiles and consumers.

use super::*;

#[test]
fn sandbox_table_is_off_by_default_strict_and_user_layer_only() {
    use tole_core::sandbox::{Mode, Network, Workspace};
    // Absent or empty: off, so nothing changes for existing setups.
    let none = parse("", &p()).unwrap();
    assert!(none.sandbox.is_none());
    let empty = parse("[sandbox]\n", &p()).unwrap();
    assert_eq!(empty.sandbox.unwrap().policy().unwrap().mode, Mode::Off);

    let c = parse(
        "[sandbox]\nmode = \"required\"\nprofile = \"strict\"\nnetwork = \"host\"\n\
             read = [\"/opt/x\"]\nskip = [\"cora\"]\n",
        &p(),
    )
    .unwrap();
    let pol = c.sandbox.unwrap().policy().unwrap();
    assert_eq!(pol.mode, Mode::Required);
    assert_eq!(pol.profile.workspace, Workspace::None);
    assert_eq!(pol.profile.network, Network::Host);
    assert_eq!(pol.skip, vec!["cora".to_string()]);

    // Lifecycle limits: defaults, and bounds that name the key.
    assert_eq!(pol.max_servers, tole_core::sandbox::DEFAULT_MAX_SERVERS);
    assert_eq!(pol.idle_secs, tole_core::sandbox::DEFAULT_IDLE_SECS);
    let c = parse("[sandbox]\nmax_servers = 4\nidle_secs = 60\n", &p()).unwrap();
    let pol = c.sandbox.unwrap().policy().unwrap();
    assert_eq!((pol.max_servers, pol.idle_secs), (4, 60));
    // Lazy start (#750): off unless the user config says so; a project
    // config cannot turn it on (the whole table is user-layer only).
    let off = parse("[sandbox]\nmode = \"off\"\n", &p()).unwrap();
    assert_eq!(off.sandbox.as_ref().unwrap().lazy_start, None);
    let on = parse("[sandbox]\nlazy_start = true\n", &p()).unwrap();
    assert_eq!(on.sandbox.as_ref().unwrap().lazy_start, Some(true));
    assert!(parse("[sandbox]\nlazy_start = \"yes\"\n", &p()).is_err());
    let layered = layered(&on, &parse("model = \"m\"\n", &p()).unwrap());
    assert_eq!(layered.sandbox.and_then(|s| s.lazy_start), None);
    for bad in [
        "max_servers = 0",
        "max_servers = 65",
        "idle_secs = 0",
        "idle_secs = 86401",
    ] {
        let e = parse(&format!("[sandbox]\n{bad}\n"), &p())
            .unwrap_err()
            .to_string();
        assert!(e.contains("sandbox."), "{bad}: {e}");
    }
    // Unknown keys and values are errors that name the key.
    assert!(parse("[sandbox]\nmodes = \"on\"\n", &p()).is_err());
    for bad in ["mode = \"on\"", "profile = \"x\"", "network = \"some\""] {
        let e = parse(&format!("[sandbox]\n{bad}\n"), &p())
            .unwrap_err()
            .to_string();
        assert!(e.contains("sandbox."), "{e}");
    }
}

const PROFILES: &str = "[sandbox]\nmode = \"required\"\n\
        [sandbox.profiles.net]\nnetwork = \"host\"\nread = [\"/opt/data\"]\n\
        [sandbox.profiles.rw]\nworkspace = \"write\"\n\
        [sandbox.servers.github]\nprofile = \"net\"\n\
        [sandbox.servers.build]\nprofile = \"strict\"\n";

#[test]
fn sandbox_profiles_and_server_bindings_resolve_into_the_policy() {
    use tole_core::sandbox::{Network, Workspace};
    let c = parse(PROFILES, &p()).unwrap();
    let sb = c.sandbox.as_ref().unwrap();
    let pol = sb.policy().unwrap();
    // The default is untouched by the named profiles.
    assert_eq!(pol.profile_name, "server");
    assert_eq!(pol.servers.len(), 2);
    let (name, net) = pol.profile_for("github");
    assert_eq!(name, "net");
    assert_eq!(
        (net.workspace, net.network),
        (Workspace::Read, Network::Host)
    );
    assert_eq!(net.read, [PathBuf::from("/opt/data")]);
    let (name, strict) = pol.profile_for("build");
    assert_eq!((name, strict.workspace), ("strict", Workspace::None));
    assert_eq!(pol.profile_for("other").0, "server");
    // A defined profile can also be the default.
    let c = parse(
        "[sandbox]\nprofile = \"rw\"\n[sandbox.profiles.rw]\nworkspace = \"write\"\n",
        &p(),
    )
    .unwrap();
    let pol = c.sandbox.unwrap().policy().unwrap();
    assert_eq!(pol.profile.workspace, Workspace::Write);
}

#[test]
fn sandbox_profile_tables_reject_bad_values_naming_the_key() {
    for (bad, key) in [
        (
            "[sandbox.profiles.a]\nworkspace = \"all\"\n",
            "sandbox.profiles.a.workspace",
        ),
        (
            "[sandbox.profiles.a]\nnetwork = \"some\"\n",
            "sandbox.profiles.a.network",
        ),
        (
            "[sandbox.profiles.server]\nnetwork = \"host\"\n",
            "built in",
        ),
        ("[sandbox.profiles.\"a b\"]\n", "sandbox.profiles.a b"),
        (
            "[sandbox.servers.s]\nprofile = \"nope\"\n",
            "sandbox.servers.s.profile",
        ),
        ("[sandbox.servers.s]\n", "profile is required"),
        (
            "[sandbox.servers.\"s@w\"]\nprofile = \"strict\"\n",
            "without `@workspace`",
        ),
        ("[sandbox]\nprofile = \"nope\"\n", "sandbox.profile"),
    ] {
        let e = parse(bad, &p()).unwrap_err().to_string();
        assert!(e.contains(key), "{bad}: {e}");
    }
    // Unknown keys inside the new tables are loud too.
    assert!(parse("[sandbox.profiles.a]\nnet = \"host\"\n", &p()).is_err());
    assert!(parse("[sandbox.servers.a]\nmode = \"off\"\n", &p()).is_err());
}

#[test]
fn a_project_config_never_defines_a_profile_or_binds_a_server() {
    let user = parse(
        "[sandbox]\nmode = \"required\"\n[sandbox.servers.github]\nprofile = \"strict\"\n",
        &p(),
    )
    .unwrap();
    // The repository tries to widen github and to define a wide profile.
    let project = parse(
        "[sandbox]\nmode = \"off\"\n[sandbox.profiles.wide]\nnetwork = \"host\"\n\
             workspace = \"write\"\n[sandbox.servers.github]\nprofile = \"wide\"\n",
        &p(),
    )
    .unwrap();
    let merged = layered(&project, &user);
    assert_eq!(merged.sandbox, user.sandbox);
    let pol = merged.sandbox.unwrap().policy().unwrap();
    assert_eq!(pol.profile_for("github").0, "strict");
    // And a project with no user sandbox gets none.
    assert!(layered(&project, &Config::default()).sandbox.is_none());
    // `config check` marks the project's lines as ignored.
    let lines = render_with(&project, PROJECT_TAG, &Annotations::new());
    for key in [
        "sandbox.profiles.wide.network",
        "sandbox.servers.github.profile",
    ] {
        let l = lines.iter().find(|l| l.starts_with(key)).unwrap();
        assert!(l.ends_with(SANDBOX_IGNORED_NOTE), "{l}");
    }
}

#[test]
fn effective_profile_and_its_source() {
    use sandbox::Source;
    let c = parse(PROFILES, &p()).unwrap();
    let sb = c.sandbox.unwrap();
    let e = sb.effective("github").unwrap();
    assert_eq!((e.name.as_str(), e.source), ("net", Source::Server));
    let e = sb.effective("other").unwrap();
    assert_eq!(
        (e.name.as_str(), e.source),
        ("server", Source::BuiltinDefault)
    );
    let sb = parse("[sandbox]\nprofile = \"strict\"\n", &p())
        .unwrap()
        .sandbox
        .unwrap();
    assert_eq!(sb.effective("x").unwrap().source, Source::UserDefault);
    let lines = sandbox::effective_lines(&parse(PROFILES, &p()).unwrap().sandbox.unwrap());
    assert!(lines.contains(
        &"sandbox effective: server \"github\" profile \"net\" [user config [sandbox.servers]]"
            .to_string()
    ));
    assert!(lines
        .iter()
        .any(|l| l.starts_with("sandbox effective: default profile \"server\"")));
}

/// #748: an absent key is the DEFAULT (implicit optional), a written one is
/// the owner's word, whatever its value, and `off` is off. The state
/// survives the parse, so `tole doctor` and the launcher can tell them apart.
#[test]
fn sandbox_consumers_distinguish_absent_from_written() {
    use tole_core::sandbox::command::Consumer::{TurnEnd, Verify};
    use tole_core::sandbox::Mode;
    let c = |t: &str| {
        parse(t, &p())
            .unwrap()
            .sandbox
            .unwrap()
            .consumers()
            .unwrap()
    };
    // Only `verify` written (optional): explicit, so not implicit; turnend
    // is still the default.
    let s = c("[sandbox.consumers]\nverify = \"optional\"\n");
    assert_eq!((s.verify, s.turnend), (Mode::Optional, Mode::Optional));
    assert!(!s.implicit(Verify) && s.implicit(TurnEnd));
    // Written required / off: never implicit.
    let s = c("[sandbox.consumers]\nverify = \"required\"\nturnend = \"off\"\n");
    assert_eq!((s.verify, s.turnend), (Mode::Required, Mode::Off));
    assert!(!s.implicit(Verify) && !s.implicit(TurnEnd));
    // Other keys written leave the two defaulted.
    let s = c("[sandbox.consumers]\nrun_command = \"required\"\n");
    assert!(s.implicit(Verify) && s.implicit(TurnEnd));
}

#[test]
fn sandbox_consumers_default_flip_is_bounded_and_user_layer_only() {
    use tole_core::sandbox::command::{Consumer, Consumers, Limits};
    use tole_core::sandbox::{Mode, Network};
    // Absent or empty (#748): `verify` and `turnend` are optional BY
    // DEFAULT (implicit), the other three off, no network, the default
    // limits.
    for text in ["", "[sandbox]\n", "[sandbox.consumers]\n"] {
        let s = match parse(text, &p()).unwrap().sandbox {
            None => Consumers::host_default(),
            Some(s) => s.consumers().unwrap(),
        };
        assert_eq!(
            (s.run_command, s.job_start, s.verify, s.turnend, s.cron),
            (
                Mode::Off,
                Mode::Off,
                Mode::Optional,
                Mode::Optional,
                Mode::Off
            )
        );
        assert!(s.implicit(Consumer::Verify) && s.implicit(Consumer::TurnEnd));
        assert!(!s.implicit(Consumer::RunCommand) && !s.implicit(Consumer::Cron));
        assert_eq!((s.network, s.limits), (Network::None, Limits::default()));
    }
    let c = parse(
            "[sandbox.consumers]\nrun_command = \"required\"\njob_start = \"optional\"\n\
             verify = \"required\"\nturnend = \"optional\"\ncron = \"required\"\nnetwork = \"host\"\ncpu_secs = 60\nmemory_mb = 512\nprocesses = 64\n\
             file_size_mb = 10\n",
            &p(),
        )
        .unwrap();
    let s = c.sandbox.unwrap().consumers().unwrap();
    assert_eq!(
        (s.run_command, s.job_start, s.verify, s.turnend, s.cron),
        (
            Mode::Required,
            Mode::Optional,
            Mode::Required,
            Mode::Optional,
            Mode::Required
        )
    );
    assert_eq!(s.network, Network::Host);
    assert_eq!(
        s.limits,
        Limits {
            cpu_secs: 60,
            memory_mb: 512,
            processes: 64,
            file_size_mb: 10
        }
    );
    // Bad values and unknown keys are errors that name the key.
    for (bad, key) in [
        ("run_command = \"on\"", "sandbox.consumers.run_command"),
        ("job_start = \"yes\"", "sandbox.consumers.job_start"),
        ("verify = \"on\"", "sandbox.consumers.verify"),
        ("turnend = \"1\"", "sandbox.consumers.turnend"),
        ("cron = \"yes\"", "sandbox.consumers.cron"),
        ("network = \"some\"", "sandbox.consumers.network"),
        ("cpu_secs = 0", "sandbox.consumers.cpu_secs"),
        ("cpu_secs = 86401", "sandbox.consumers.cpu_secs"),
        ("memory_mb = 255", "sandbox.consumers.memory_mb"),
        ("processes = 15", "sandbox.consumers.processes"),
        ("processes = 65537", "sandbox.consumers.processes"),
        ("file_size_mb = 0", "sandbox.consumers.file_size_mb"),
    ] {
        let e = parse(&format!("[sandbox.consumers]\n{bad}\n"), &p())
            .unwrap_err()
            .to_string();
        assert!(e.contains(key), "{bad}: {e}");
    }
    // `cron` is a consumer since #748 slice 3; a misspelt key still is not.
    assert!(parse("[sandbox.consumers]\ncron = \"required\"\n", &p()).is_ok());
    assert!(parse("[sandbox.consumers]\nrun_commands = \"off\"\n", &p()).is_err());
    // A project config cannot set them; the user layer wins whole.
    let user = parse("[sandbox.consumers]\nrun_command = \"required\"\n", &p()).unwrap();
    let project = parse(
        "[sandbox.consumers]\nrun_command = \"off\"\nnetwork = \"host\"\n",
        &p(),
    )
    .unwrap();
    assert_eq!(layered(&project, &user).sandbox, user.sandbox);
    assert!(layered(&project, &Config::default()).sandbox.is_none());
}

#[test]
fn config_check_shows_the_consumer_keys() {
    let user = parse(
        "[sandbox.consumers]\nrun_command = \"required\"\nnetwork = \"host\"\ncpu_secs = 9\n",
        &p(),
    )
    .unwrap();
    let lines = render_with(&user, USER_TAG, &Annotations::new());
    for want in [
        "sandbox.consumers.run_command = \"required\"",
        "sandbox.consumers.network = \"host\"",
        "sandbox.consumers.cpu_secs = 9",
    ] {
        assert!(
            lines.iter().any(|l| l.contains(want)),
            "{want} in {lines:?}"
        );
    }
    // A project layer's lines are marked ignored.
    let lines = render_with(&user, PROJECT_TAG, &Annotations::new());
    let mine: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("sandbox.consumers."))
        .collect();
    assert_eq!(mine.len(), 3, "{lines:?}");
    assert!(mine.iter().all(|l| l.contains("IGNORED")), "{lines:?}");
}

#[test]
fn a_project_config_cannot_set_the_sandbox() {
    let user = parse("[sandbox]\nmode = \"required\"\n", &p()).unwrap();
    let project = parse("[sandbox]\nmode = \"off\"\nnetwork = \"host\"\n", &p()).unwrap();
    let eff = layered(&project, &user);
    assert_eq!(eff.sandbox, user.sandbox, "the user layer wins whole");
    let eff = layered(&project, &Config::default());
    assert!(eff.sandbox.is_none(), "a project alone cannot turn it on");
}
