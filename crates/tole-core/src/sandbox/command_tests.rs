use super::*;
use std::path::Path;

use crate::test_dir::TestDir;
fn tmp(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-cmd-{tag}"))
}

fn usable() -> Availability {
    Availability::Usable {
        bwrap: "/usr/bin/bwrap".into(),
        version: "0.9.0".into(),
    }
}

fn gone() -> Availability {
    Availability::Unavailable("no bwrap".into())
}

fn on(rc: Mode, js: Mode) -> Consumers {
    Consumers {
        run_command: rc,
        job_start: js,
        ..Consumers::default()
    }
}

fn env_in(ws: &Path) -> Env {
    Env {
        path: Some("/usr/bin:/bin".into()),
        home: Some("/home/owner".into()),
        data_dir: Some("/home/owner/.codecora/tole".into()),
        workspace: Some(ws.to_path_buf()),
    }
}

fn sh_args() -> Vec<String> {
    vec!["-c".into(), "echo hi".into()]
}

/// The argv of a sandboxed launch of `sh -c 'echo hi'`, as text.
fn argv(c: &Consumers, consumer: Consumer, env: &Env, args: &[String]) -> Vec<String> {
    match plan_command(c, &usable(), false, env, consumer, ("sh", args)) {
        CommandLaunch::Sandboxed { args, .. } => args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        other => panic!("expected a sandboxed launch, got {other:?}"),
    }
}

fn run_argv(ws: &Path) -> Vec<String> {
    let c = on(Mode::Required, Mode::Required);
    argv(&c, Consumer::RunCommand, &env_in(ws), &sh_args())
}

fn has(a: &[String], seq: &[&str]) -> bool {
    a.windows(seq.len())
        .any(|w| w.iter().zip(seq).all(|(x, y)| x == y))
}

// Every hardening flag of the command profile has a test that fails when
// the flag is dropped from the builder (checked by mutation).

#[test]
fn a_command_unshares_everything_including_the_user_namespace() {
    let a = run_argv(&tmp("unshare"));
    assert!(a.contains(&"--unshare-all".into()));
    assert!(a.contains(&"--unshare-user".into()));
}

#[test]
fn a_command_cannot_make_a_nested_user_namespace() {
    assert!(run_argv(&tmp("userns")).contains(&"--disable-userns".into()));
}

#[test]
fn a_command_drops_all_capabilities_and_gets_a_new_session() {
    let a = run_argv(&tmp("caps"));
    assert!(has(&a, &["--cap-drop", "ALL"]));
    assert!(a.contains(&"--new-session".into()));
}

#[test]
fn a_command_dies_with_tole_but_a_job_does_not() {
    let ws = tmp("parent");
    let c = on(Mode::Required, Mode::Required);
    let run = argv(&c, Consumer::RunCommand, &env_in(&ws), &sh_args());
    let job = argv(&c, Consumer::JobStart, &env_in(&ws), &sh_args());
    assert!(run.contains(&"--die-with-parent".into()));
    assert!(!job.contains(&"--die-with-parent".into()), "jobs outlive");
    // The one difference between the two launches.
    let mut a = run;
    a.retain(|x| x != "--die-with-parent");
    assert_eq!(a, job);
}

#[test]
fn a_command_has_a_private_proc_and_a_minimal_dev() {
    let a = run_argv(&tmp("proc"));
    assert!(has(&a, &["--proc", "/proc"]));
    assert!(has(&a, &["--dev", "/dev"]));
    assert!(has(&a, &["--tmpfs", "/tmp"]));
}

#[test]
fn the_system_is_read_only_and_home_and_the_data_dir_are_not_mounted() {
    let a = run_argv(&tmp("system"));
    for d in ["/usr", "/bin", "/lib"] {
        assert!(has(&a, &["--ro-bind-try", d, d]), "{d} read-only");
    }
    assert!(!a.iter().any(|x| x == "/"), "no bind of the root");
    assert!(
        !a.iter()
            .any(|x| x.starts_with("/home") || x.contains(".codecora")),
        "nothing of HOME or the data dir: {a:?}"
    );
}

#[test]
fn the_network_is_none_unless_the_owner_wrote_host() {
    let ws = tmp("net");
    let mut c = on(Mode::Required, Mode::Required);
    let none = argv(&c, Consumer::RunCommand, &env_in(&ws), &sh_args());
    assert!(!none.contains(&"--share-net".into()));
    assert!(!none.contains(&"/etc/resolv.conf".into()), "no resolver");
    c.network = Network::Host;
    let host = argv(&c, Consumer::RunCommand, &env_in(&ws), &sh_args());
    assert!(host.contains(&"--share-net".into()));
    assert!(host.contains(&"/etc/resolv.conf".into()));
}

#[test]
fn the_workspace_is_writable_and_is_the_working_directory() {
    let ws = tmp("write");
    let a = run_argv(&ws);
    let w = ws.display().to_string();
    assert!(has(&a, &["--bind", &w, &w]), "writable bind");
    assert!(has(&a, &["--chdir", &w]));
    assert!(!has(&a, &["--ro-bind", &w, &w]), "not read-only");
}

#[test]
fn the_protected_paths_are_read_only_on_top_of_the_writable_workspace() {
    let ws = tmp("protect");
    for d in [".git/hooks", ".tole"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    std::fs::write(ws.join(".git/config"), "x").unwrap();
    let a = run_argv(&ws);
    let w = a.iter().position(|x| x == "--bind").unwrap();
    for rel in PROTECTED {
        let p = ws.join(rel).display().to_string();
        assert!(has(&a, &["--ro-bind", &p, &p]), "{rel} read-only");
        assert!(
            a.iter().rposition(|x| *x == p).unwrap() > w,
            "{rel} comes after the writable bind, so it wins"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_protected_link_is_followed_and_its_target_is_what_gets_bound() {
    let ws = tmp("plink");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::create_dir_all(ws.join("githooks")).unwrap();
    std::os::unix::fs::symlink("../githooks", ws.join(".git/hooks")).unwrap();
    // `.tole` leads out of the workspace: not in the sandbox's view at all.
    let outside = tmp("plink-out");
    std::os::unix::fs::symlink(&outside, ws.join(".tole")).unwrap();
    let a = run_argv(&ws);
    let target = ws.join("githooks").display().to_string();
    assert!(has(&a, &["--ro-bind", &target, &target]), "target bound ro");
    assert!(
        !a.contains(&outside.display().to_string()),
        "a target outside the workspace is not bound"
    );
    let link = ws.join(".git/hooks").display().to_string();
    assert!(
        !a.contains(&link),
        "the link spelling is never a bind path: {a:?}"
    );
}

#[test]
fn a_protected_path_that_does_not_exist_is_not_bound() {
    let ws = tmp("absent");
    assert!(protected_paths(&ws).is_empty());
    let a = run_argv(&ws);
    assert!(!a.iter().any(|x| x.ends_with(".tole")), "{a:?}");
}

#[test]
fn only_home_and_tmpdir_use_setenv_and_never_a_secret() {
    let a = run_argv(&tmp("setenv"));
    let set: Vec<&str> = a
        .windows(3)
        .filter(|w| w[0] == "--setenv")
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(set, ["HOME", "TMPDIR"]);
    assert!(a
        .windows(3)
        .filter(|w| w[0] == "--setenv")
        .all(|w| w[2] == "/scratch"));
    assert!(
        !a.contains(&"--clearenv".into()),
        "env comes from the scrubbed launcher"
    );
}

#[test]
fn the_command_is_argv_after_a_separator_and_never_a_shell_string() {
    let ws = tmp("argv");
    let hostile: Vec<String> = [
        "--bind",
        "/",
        "/",
        "; rm -rf ~",
        "$(id)",
        "a\nb",
        "--setenv",
        "X",
        "--",
        "",
    ]
    .map(String::from)
    .into();
    let c = on(Mode::Required, Mode::Required);
    let a = argv(&c, Consumer::RunCommand, &env_in(&ws), &hostile);
    let sep = a.iter().position(|x| x == "--").unwrap();
    // After the separator: the resolved program, then every argument
    // verbatim and in order.
    assert!(a[sep + 1].ends_with("/sh"), "{}", a[sep + 1]);
    assert_eq!(&a[sep + 2..], &hostile[..]);
    // And no argument of the command sits before the separator.
    assert!(!a[..sep].iter().any(|x| x == "; rm -rf ~" || x == "$(id)"));
    assert_eq!(
        a.iter().filter(|x| *x == "--").count(),
        2,
        "the separator plus the hostile one, which stayed an argument"
    );
}

#[test]
fn a_program_with_a_slash_or_not_found_is_refused_in_every_mode() {
    let ws = tmp("prog");
    for mode in [Mode::Optional, Mode::Required] {
        for unattended in [false, true] {
            for name in ["./x", "/bin/sh", "no-such-program-xyz", ""] {
                let l = plan_command(
                    &on(mode, mode),
                    &usable(),
                    unattended,
                    &env_in(&ws),
                    Consumer::RunCommand,
                    (name, &[]),
                );
                let CommandLaunch::Refused(why) = l else {
                    panic!("{name:?}: {l:?}")
                };
                if name.contains('/') {
                    assert!(why.contains("bare name"), "{name:?}: {why}");
                }
            }
        }
    }
}

#[test]
fn a_relative_path_entry_is_never_searched() {
    let ws = tmp("relpath");
    let mut env = env_in(&ws);
    env.path = Some(".:bin:/usr/bin".into());
    std::fs::create_dir_all(ws.join("bin")).unwrap();
    let c = on(Mode::Required, Mode::Required);
    let a = match plan_command(
        &c,
        &usable(),
        false,
        &env,
        Consumer::RunCommand,
        ("sh", &[]),
    ) {
        CommandLaunch::Sandboxed { args, .. } => args,
        other => panic!("{other:?}"),
    };
    let a: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
    assert!(a.contains(&"/usr/bin/sh".to_string()) || a.contains(&"/bin/sh".to_string()));
    // PATH of nothing absolute finds nothing.
    env.path = Some(".:bin".into());
    let l = plan_command(
        &c,
        &usable(),
        false,
        &env,
        Consumer::RunCommand,
        ("sh", &[]),
    );
    assert!(matches!(l, CommandLaunch::Refused(_)), "{l:?}");
}

#[test]
fn find_program_skips_every_relative_path_entry() {
    // A relative entry that WOULD reach /usr/bin from this process's cwd.
    let up = std::env::current_dir().unwrap().components().count();
    let rel = format!("{}usr/bin", "../".repeat(up));
    assert!(Path::new("/usr/bin/sh").exists() || Path::new("/bin/sh").exists());
    assert!(
        std::fs::metadata(Path::new(&rel).join("sh")).is_ok(),
        "sanity: the relative entry reaches sh"
    );
    assert_eq!(find_program("sh", Some(&rel)), None);
    assert!(find_program("sh", Some(&format!("{rel}:/usr/bin:/bin"))).is_some());
    assert_eq!(find_program("sh", None), None);
}

#[cfg(unix)]
fn script(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    crate::test_dir::write_executable(&p, "#!/bin/sh\nexit 0\n");
    p
}

#[cfg(unix)]
#[test]
fn a_program_outside_the_system_dirs_is_bound_resolved_and_run_through_it() {
    let ws = tmp("prefix-ws");
    let bin = tmp("prefix-bin");
    script(&bin, "mytool");
    // A link to the dir: the bind and the program path use the target.
    let links = tmp("prefix-links");
    let linkdir = links.join("l");
    std::os::unix::fs::symlink(&bin, &linkdir).unwrap();
    let mut env = env_in(&ws);
    env.path = Some(format!("{}:/usr/bin", linkdir.display()));
    let c = on(Mode::Required, Mode::Required);
    let l = plan_command(
        &c,
        &usable(),
        false,
        &env,
        Consumer::RunCommand,
        ("mytool", &[]),
    );
    let CommandLaunch::Sandboxed { args, .. } = l else {
        panic!("{l:?}")
    };
    let a: Vec<String> = args
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    let b = bin.display().to_string();
    assert!(has(&a, &["--ro-bind-try", &b, &b]), "dir bound read-only");
    assert!(a.contains(&bin.join("mytool").display().to_string()));
    assert!(
        !a.contains(&linkdir.display().to_string()),
        "the link is not a path of the argv: {a:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_program_in_the_workspace_is_not_rebound_read_only() {
    let ws = tmp("inws");
    std::fs::create_dir_all(ws.join("tools")).unwrap();
    script(&ws.join("tools"), "mine");
    let mut env = env_in(&ws);
    env.path = Some(format!("{}/tools:/usr/bin", ws.display()));
    let c = on(Mode::Required, Mode::Required);
    let l = plan_command(
        &c,
        &usable(),
        false,
        &env,
        Consumer::RunCommand,
        ("mine", &[]),
    );
    let CommandLaunch::Sandboxed { args, .. } = l else {
        panic!("{l:?}")
    };
    let a: Vec<String> = args
        .iter()
        .map(|x| x.to_string_lossy().into_owned())
        .collect();
    let t = ws.join("tools").display().to_string();
    assert!(!has(&a, &["--ro-bind-try", &t, &t]), "{a:?}");
    assert!(a.contains(&format!("{t}/mine")));
}

#[test]
fn a_workspace_symlink_is_resolved_and_only_the_target_is_bound() {
    let real = tmp("wsreal");
    let links = tmp("wslink");
    let link = links.join("w");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    #[cfg(unix)]
    {
        let a = run_argv(&link);
        let r = real.display().to_string();
        assert!(has(&a, &["--bind", &r, &r]));
        assert!(!a.contains(&link.display().to_string()), "{a:?}");
    }
}

#[test]
fn a_workspace_that_is_or_holds_home_or_the_data_dir_is_refused() {
    let home = tmp("home");
    let data = home.join(".codecora/tole");
    std::fs::create_dir_all(&data).unwrap();
    let mut env = env_in(&home);
    env.home = Some(home.to_path_buf());
    env.data_dir = Some(data.clone());
    let c = on(Mode::Required, Mode::Required);
    for ws in [home.path(), data.as_path(), home.parent().unwrap()] {
        env.workspace = Some(ws.to_path_buf());
        let l = plan_command(
            &c,
            &usable(),
            false,
            &env,
            Consumer::RunCommand,
            ("sh", &[]),
        );
        let CommandLaunch::Refused(why) = l else {
            panic!("{}: {l:?}", ws.display())
        };
        assert!(why.contains("run_command"), "{why}");
    }
    // A project under HOME is fine.
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    env.workspace = Some(project);
    assert!(matches!(
        plan_command(
            &c,
            &usable(),
            false,
            &env,
            Consumer::RunCommand,
            ("sh", &[])
        ),
        CommandLaunch::Sandboxed { .. }
    ));
}

#[test]
fn a_launch_binds_the_workspace_it_was_given_and_no_other() {
    let (a, b) = (tmp("sess-a"), tmp("sess-b"));
    let c = on(Mode::Required, Mode::Required);
    let (xa, xb) = (
        argv(&c, Consumer::RunCommand, &env_in(&a), &sh_args()),
        argv(&c, Consumer::RunCommand, &env_in(&b), &sh_args()),
    );
    assert!(xa.contains(&a.display().to_string()) && !xa.contains(&b.display().to_string()));
    assert!(xb.contains(&b.display().to_string()) && !xb.contains(&a.display().to_string()));
}

// Policy: off, optional, required, unattended, unavailable backend.

fn plan(
    c: &Consumers,
    avail: &Availability,
    unattended: bool,
    consumer: Consumer,
) -> CommandLaunch {
    let ws = tmp("policy");
    plan_command(c, avail, unattended, &env_in(&ws), consumer, ("sh", &[]))
}

#[test]
fn off_is_the_unchanged_direct_path_whatever_the_backend() {
    let direct = CommandLaunch::Direct { warning: None };
    for avail in [usable(), gone()] {
        for unattended in [false, true] {
            for k in Consumer::ALL {
                assert_eq!(plan(&Consumers::default(), &avail, unattended, k), direct);
            }
        }
    }
}

#[test]
fn each_consumer_has_its_own_mode() {
    let c = on(Mode::Required, Mode::Off);
    assert!(matches!(
        plan(&c, &usable(), false, Consumer::RunCommand),
        CommandLaunch::Sandboxed { .. }
    ));
    assert_eq!(
        plan(&c, &usable(), false, Consumer::JobStart),
        CommandLaunch::Direct { warning: None }
    );
}

#[test]
fn a_usable_backend_wraps_for_optional_and_required_and_carries_the_limits() {
    for mode in [Mode::Optional, Mode::Required] {
        for unattended in [false, true] {
            let mut c = on(mode, mode);
            c.limits.cpu_secs = 77;
            let l = plan(&c, &usable(), unattended, Consumer::JobStart);
            let CommandLaunch::Sandboxed {
                program, limits, ..
            } = l
            else {
                panic!("{mode:?}: {l:?}")
            };
            assert_eq!(program, PathBuf::from("/usr/bin/bwrap"));
            assert_eq!(limits.cpu_secs, 77);
        }
    }
}

#[test]
fn required_without_a_backend_refuses_naming_the_remedy() {
    for unattended in [false, true] {
        let c = on(Mode::Required, Mode::Required);
        for k in [Consumer::RunCommand, Consumer::JobStart] {
            let CommandLaunch::Refused(why) = plan(&c, &gone(), unattended, k) else {
                panic!("must refuse")
            };
            assert!(why.contains(k.name()) && why.contains("no bwrap"), "{why}");
            assert!(why.contains("install bubblewrap"), "{why}");
        }
    }
}

#[test]
fn optional_without_a_backend_runs_with_a_warning_only_when_attended() {
    let c = on(Mode::Optional, Mode::Optional);
    let CommandLaunch::Direct { warning } = plan(&c, &gone(), false, Consumer::RunCommand) else {
        panic!("attended optional falls back")
    };
    let w = warning.expect("a warning");
    assert!(w.contains("no sandbox") && w.contains("no bwrap"), "{w}");
    for k in [Consumer::RunCommand, Consumer::JobStart] {
        let CommandLaunch::Refused(why) = plan(&c, &gone(), true, k) else {
            panic!("an unattended run never runs unsandboxed")
        };
        assert!(why.contains("unattended"), "{why}");
    }
}

#[test]
fn a_platform_without_a_sandbox_is_honest() {
    let none = Availability::Unavailable("no sandbox on this platform (Linux only)".into());
    let CommandLaunch::Refused(why) = plan(
        &on(Mode::Required, Mode::Off),
        &none,
        false,
        Consumer::RunCommand,
    ) else {
        panic!("required refuses where no sandbox exists")
    };
    assert!(why.contains("Linux only"), "{why}");
    assert!(matches!(
        plan(
            &on(Mode::Optional, Mode::Off),
            &none,
            false,
            Consumer::RunCommand
        ),
        CommandLaunch::Direct { warning: Some(_) }
    ));
}

#[test]
fn a_workspace_the_sandbox_cannot_grant_follows_the_same_policy() {
    let run = |ws: Option<&str>, mode, unattended| {
        let mut env = env_in(Path::new("/"));
        env.workspace = ws.map(PathBuf::from);
        plan_command(
            &on(mode, mode),
            &usable(),
            unattended,
            &env,
            Consumer::RunCommand,
            ("sh", &[]),
        )
    };
    for ws in [Some("/"), None] {
        assert!(matches!(
            run(ws, Mode::Required, false),
            CommandLaunch::Refused(_)
        ));
        assert!(matches!(
            run(ws, Mode::Optional, true),
            CommandLaunch::Refused(_)
        ));
        assert!(matches!(
            run(ws, Mode::Optional, false),
            CommandLaunch::Direct { warning: Some(_) }
        ));
    }
}

#[test]
fn limits_are_bounded() {
    assert!(Limits::default().check().is_ok());
    for (bad, key) in [
        (
            Limits {
                cpu_secs: 0,
                ..Limits::default()
            },
            "cpu_secs",
        ),
        (
            Limits {
                cpu_secs: 86_401,
                ..Limits::default()
            },
            "cpu_secs",
        ),
        (
            Limits {
                memory_mb: 255,
                ..Limits::default()
            },
            "memory_mb",
        ),
        (
            Limits {
                memory_mb: 1_048_577,
                ..Limits::default()
            },
            "memory_mb",
        ),
        (
            Limits {
                processes: 15,
                ..Limits::default()
            },
            "processes",
        ),
        (
            Limits {
                processes: 65_537,
                ..Limits::default()
            },
            "processes",
        ),
        (
            Limits {
                file_size_mb: 0,
                ..Limits::default()
            },
            "file_size_mb",
        ),
        (
            Limits {
                file_size_mb: 1_048_577,
                ..Limits::default()
            },
            "file_size_mb",
        ),
    ] {
        let e = bad.check().unwrap_err();
        assert!(e.contains(&format!("sandbox.consumers.{key}")), "{e}");
    }
    for ok in [
        Limits {
            cpu_secs: 1,
            memory_mb: 256,
            processes: 16,
            file_size_mb: 1,
        },
        Limits {
            cpu_secs: 86_400,
            memory_mb: 1_048_576,
            processes: 65_536,
            file_size_mb: 1_048_576,
        },
    ] {
        assert!(ok.check().is_ok());
    }
}

#[test]
fn the_default_consumers_are_all_off_with_no_network() {
    let c = Consumers::default();
    assert_eq!(
        (c.run_command, c.job_start, c.verify),
        (Mode::Off, Mode::Off, Mode::Off)
    );
    assert_eq!(c.network, Network::None);
    assert_eq!(consumers(), c, "nothing set: everything off");
}

#[test]
fn doctor_says_what_is_there_and_that_off_is_off() {
    let off = doctor_lines(&Consumers::default(), &usable());
    assert!(off[0].contains("run_command off, job_start off"), "{off:?}");
    assert!(off[0].contains("verify off"), "{off:?}");
    // Only what is still not a consumer is called unsandboxed.
    let last = off.last().unwrap();
    assert!(
        last.contains("turnend, cron scripts and deliveries are not sandboxed yet")
            && !last.contains("verify"),
        "{off:?}"
    );
    assert!(!off.iter().any(|l| l.contains("ERROR")));

    let mut c = on(Mode::Required, Mode::Optional);
    c.network = Network::Host;
    let l = doctor_lines(&c, &usable());
    assert!(
        l[0].contains("run_command required, job_start optional"),
        "{l:?}"
    );
    assert!(l[0].contains("network host"), "{l:?}");
    assert!(
        l[0].contains("cpu 3600s") && l[0].contains("processes 4096"),
        "{l:?}"
    );
    assert!(!l.iter().any(|x| x.contains("ERROR")));

    let l = doctor_lines(&c, &gone());
    let errs: Vec<_> = l.iter().filter(|x| x.contains("ERROR")).collect();
    assert_eq!(errs.len(), 1, "only a required consumer fails: {l:?}");
    assert!(errs[0].contains("run_command") && errs[0].contains("refused (required)"));
    let warns: Vec<_> = l.iter().filter(|x| x.contains("WARNING")).collect();
    assert_eq!(warns.len(), 1, "{l:?}");
    assert!(warns[0].contains("job_start") && warns[0].contains("unattended"));
    // Off consumers never report an error.
    let l = doctor_lines(&on(Mode::Off, Mode::Off), &gone());
    assert!(!l.iter().any(|x| x.contains("ERROR")));
}

// Slice 2 (#748): the mission `--verify` command.

fn all(mode: Mode) -> Consumers {
    Consumers {
        run_command: mode,
        job_start: mode,
        verify: mode,
        ..Consumers::default()
    }
}

#[test]
fn consumer_names_are_the_config_and_secrets_keys() {
    let names: Vec<_> = Consumer::ALL.iter().map(|k| k.name()).collect();
    assert_eq!(names, ["run_command", "job_start", "verify"]);
    assert_eq!(Consumer::Verify.name(), crate::secret_release::VERIFY);
}

#[test]
fn verify_has_its_own_mode() {
    let c = Consumers {
        verify: Mode::Required,
        ..Consumers::default()
    };
    for k in Consumer::ALL {
        let want = if k == Consumer::Verify {
            Mode::Required
        } else {
            Mode::Off
        };
        assert_eq!(c.mode(k), want, "{k:?}");
    }
    assert!(matches!(
        plan(&c, &usable(), false, Consumer::Verify),
        CommandLaunch::Sandboxed { .. }
    ));
    assert_eq!(
        plan(&c, &usable(), false, Consumer::RunCommand),
        CommandLaunch::Direct { warning: None },
        "run_command stays off while verify is on"
    );
}

/// Every hardening flag of the command profile is on for verify too: the
/// same argv as a `run_command` of that program.
#[test]
fn verify_gets_the_same_hardening_as_run_command() {
    let ws = tmp("slice2-flags");
    let c = all(Mode::Required);
    let base = argv(&c, Consumer::RunCommand, &env_in(&ws), &sh_args());
    let a = argv(&c, Consumer::Verify, &env_in(&ws), &sh_args());
    assert_eq!(a, base, "one profile for every command");
    for flag in [
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--new-session",
        "--die-with-parent",
    ] {
        assert!(a.contains(&flag.to_string()), "{flag}");
    }
    assert!(has(&a, &["--cap-drop", "ALL"]));
    assert!(!a.contains(&"--share-net".to_string()), "no network");
    let w = ws.display().to_string();
    assert!(has(&a, &["--bind", &w, &w]), "writable workspace");
}

#[test]
fn verify_protects_hooks_config_and_tole_dir() {
    let ws = tmp("slice2-protect");
    for d in [".git/hooks", ".tole"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
    let ws = std::fs::canonicalize(&*ws).unwrap();
    let a = argv(
        &all(Mode::Required),
        Consumer::Verify,
        &env_in(&ws),
        &sh_args(),
    );
    for rel in PROTECTED {
        let p = ws.join(rel).display().to_string();
        assert!(has(&a, &["--ro-bind", &p, &p]), "{rel} read-only");
    }
}

#[test]
fn the_verify_command_is_sh_dash_c_with_the_string_as_one_argument() {
    let ws = tmp("slice2-verify");
    let line = "test -f x && echo 'a b' ; rm -rf $HOME";
    let a = argv(
        &all(Mode::Required),
        Consumer::Verify,
        &env_in(&ws),
        &["-c".into(), line.into()],
    );
    let n = a.len();
    assert_eq!(&a[n - 2..], ["-c", line], "the string is one argv element");
    assert!(a[n - 3].ends_with("/sh"), "{a:?}");
    assert_eq!(a[n - 4], "--", "after the separator");
}

#[test]
fn a_mission_is_unattended_so_optional_verify_is_required() {
    assert!(Consumer::Verify.always_unattended());
    for k in [Consumer::RunCommand, Consumer::JobStart] {
        assert!(
            !k.always_unattended(),
            "{k:?}: only verify runs in a mission"
        );
    }
    // `launch_command` passes `true` for verify, and then a missing sandbox
    // refuses even for `optional`.
    let c = all(Mode::Optional);
    let CommandLaunch::Refused(why) = plan(&c, &gone(), true, Consumer::Verify) else {
        panic!("optional verify without a sandbox must refuse")
    };
    assert!(
        why.starts_with("verify: ") && why.contains("unattended"),
        "{why}"
    );
}

#[test]
fn required_verify_without_a_backend_refuses_naming_the_key() {
    let c = all(Mode::Required);
    for unattended in [false, true] {
        let CommandLaunch::Refused(why) = plan(&c, &gone(), unattended, Consumer::Verify) else {
            panic!("must refuse")
        };
        assert!(why.contains("sandbox.consumers.verify"), "{why}");
        assert!(
            why.contains("no bwrap") && why.contains("install bubblewrap"),
            "{why}"
        );
    }
}

#[test]
fn doctor_lists_verify_and_fails_for_it_without_a_sandbox() {
    let mut c = Consumers {
        verify: Mode::Required,
        ..Consumers::default()
    };
    let l = doctor_lines(&c, &usable());
    assert!(l[0].contains("verify required"), "{l:?}");
    assert!(!l.iter().any(|x| x.contains("ERROR")), "{l:?}");
    let l = doctor_lines(&c, &gone());
    assert!(
        l.iter()
            .any(|x| x.starts_with("sandbox consumers: ERROR verify")
                && x.contains("refused (required)")),
        "{l:?}"
    );
    // An optional verify cannot fall back: a mission is unattended.
    c.verify = Mode::Optional;
    let l = doctor_lines(&c, &gone());
    assert!(
        l.iter()
            .any(|x| x.starts_with("sandbox consumers: ERROR verify") && x.contains("mission")),
        "{l:?}"
    );
}
