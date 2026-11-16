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
fn the_git_dir_and_tole_are_read_only_on_top_of_the_writable_workspace() {
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

/// #849: the WHOLE `.git` is one read-only mount: no leaf mounts and no
/// writable anchor (a writable `.git` can redirect git to a planted gitdir
/// with `commondir`, whatever is locked inside it).
#[test]
fn the_whole_git_dir_is_one_read_only_mount_with_no_leaves_and_no_writable_anchor() {
    let ws = tmp("wholegit");
    for d in [".git/hooks", ".git/modules/sub/hooks", ".tole"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    std::fs::write(ws.join(".git/config"), "x").unwrap();
    let a = run_argv(&ws);
    let g = ws.join(".git").display().to_string();
    assert!(has(&a, &["--ro-bind", &g, &g]), "{a:?}");
    assert!(!has(&a, &["--bind", &g, &g]), ".git is never writable");
    assert!(
        !a.iter().any(|x| x.starts_with(&format!("{g}/"))),
        "nothing is mounted inside .git: {a:?}"
    );
}

/// The mount is made even when `hooks` or `config` is missing, so nothing can
/// be created inside; a missing `.git` creates nothing.
#[test]
fn a_git_dir_without_hooks_or_config_is_still_read_only_and_a_missing_one_is_not_created() {
    let ws = tmp("wholegit-bare");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    assert_eq!(protect_plan(&ws).unwrap(), [ws.join(".git")]);
    let bare = tmp("wholegit-nogit");
    assert!(protect_plan(&bare).unwrap().is_empty());
    assert!(!bare.join(".git").exists(), "planning creates nothing");
}

/// A gitdir file (worktree, submodule) is bound read-only itself: it cannot be
/// replaced by a directory with hooks. Its target is not bound.
#[test]
fn a_gitdir_file_is_bound_read_only_itself() {
    let ws = tmp("gitfile");
    std::fs::write(ws.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
    assert_eq!(protect_plan(&ws).unwrap(), [ws.join(".git")]);
    let a = run_argv(&ws);
    let g = ws.join(".git").display().to_string();
    assert!(has(&a, &["--ro-bind", &g, &g]));
    assert!(!a.contains(&"/elsewhere/.git/worktrees/x".to_string()));
}

/// A link cannot be protected by a mount (the command removes it and makes a
/// real directory, or it leads to a writable place), so the launch is refused
/// instead of binding the target.
#[cfg(unix)]
#[test]
fn a_protected_path_that_is_a_link_is_refused() {
    for (tag, link, target) in [
        ("git", ".git", "gitdir"),
        ("hooks", ".git/hooks", "githooks"),
        ("config", ".git/config", "gitconfig"),
        ("tole", ".tole", "tole-real"),
    ] {
        let ws = tmp(&format!("plink-{tag}"));
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join(target)).unwrap();
        if link == ".git" {
            std::fs::remove_dir_all(ws.join(".git")).unwrap();
        }
        std::os::unix::fs::symlink(ws.join(target), ws.join(link)).unwrap();
        let env = env_in(&ws);
        let l = plan_command(
            &on(Mode::Required, Mode::Required),
            &usable(),
            false,
            &env,
            Consumer::RunCommand,
            ("sh", &[]),
        );
        let CommandLaunch::Refused(why) = l else {
            panic!("{link}: {l:?}")
        };
        assert!(why.contains("symbolic link") && why.contains(link), "{why}");
        // Optional, attended: the owner's own fallback, with the reason.
        let l = plan_command(
            &on(Mode::Optional, Mode::Optional),
            &usable(),
            false,
            &env,
            Consumer::RunCommand,
            ("sh", &[]),
        );
        let CommandLaunch::Direct { warning: Some(w) } = l else {
            panic!("{link}: {l:?}")
        };
        assert!(w.contains("symbolic link"), "{w}");
    }
}

#[test]
fn a_protected_path_that_does_not_exist_is_not_bound() {
    let ws = tmp("absent");
    assert!(protect_plan(&ws).unwrap().is_empty());
    let a = run_argv(&ws);
    assert!(!a.iter().any(|x| x.ends_with(".tole")), "{a:?}");
    assert!(!a.iter().any(|x| x.ends_with(".git")), "{a:?}");
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
        (c.run_command, c.job_start, c.verify, c.turnend),
        (Mode::Off, Mode::Off, Mode::Off, Mode::Off)
    );
    assert_eq!(c.network, Network::None);
    assert_eq!(consumers(), c, "nothing set: everything off");
}

#[test]
fn doctor_says_what_is_there_and_that_off_is_off() {
    let off = doctor_lines(&Consumers::default(), &usable());
    assert!(off[0].contains("run_command off, job_start off"), "{off:?}");
    assert!(off[0].contains("verify off, turnend off"), "{off:?}");
    // Only what is still not a consumer is called unsandboxed.
    let last = off.last().unwrap();
    assert!(
        last.contains("cron scripts and deliveries are not sandboxed yet")
            && !last.contains("verify")
            && !last.contains("turnend"),
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

// Slice 2 (#748): the mission `--verify` command and the stop gates.

fn all(mode: Mode) -> Consumers {
    Consumers {
        run_command: mode,
        job_start: mode,
        verify: mode,
        turnend: mode,
        ..Consumers::default()
    }
}

#[test]
fn consumer_names_are_the_config_and_secrets_keys() {
    let names: Vec<_> = Consumer::ALL.iter().map(|k| k.name()).collect();
    assert_eq!(names, ["run_command", "job_start", "verify", "turnend"]);
    assert_eq!(Consumer::Verify.name(), crate::secret_release::VERIFY);
    assert_eq!(Consumer::TurnEnd.name(), crate::secret_release::TURNEND);
}

#[test]
fn verify_and_turnend_have_their_own_mode() {
    let mut c = Consumers {
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
        plan(&c, &usable(), false, Consumer::TurnEnd),
        CommandLaunch::Direct { warning: None },
        "turnend stays off while verify is on"
    );
    c.turnend = Mode::Optional;
    assert!(matches!(
        plan(&c, &usable(), false, Consumer::TurnEnd),
        CommandLaunch::Sandboxed { .. }
    ));
}

/// Every hardening flag of the command profile is on for the two new
/// consumers too: the same argv as a `run_command` of that program.
#[test]
fn verify_and_turnend_get_the_same_hardening_as_run_command() {
    let ws = tmp("slice2-flags");
    let c = all(Mode::Required);
    let base = argv(&c, Consumer::RunCommand, &env_in(&ws), &sh_args());
    for k in [Consumer::Verify, Consumer::TurnEnd] {
        let a = argv(&c, k, &env_in(&ws), &sh_args());
        assert_eq!(a, base, "{k:?}: one profile for every command");
        for flag in [
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--new-session",
            "--die-with-parent",
        ] {
            assert!(a.contains(&flag.to_string()), "{k:?}: {flag}");
        }
        assert!(has(&a, &["--cap-drop", "ALL"]), "{k:?}");
        assert!(!a.contains(&"--share-net".to_string()), "{k:?}: no network");
        let w = ws.display().to_string();
        assert!(has(&a, &["--bind", &w, &w]), "{k:?}: writable workspace");
    }
}

#[test]
fn verify_and_turnend_protect_hooks_config_and_tole_dir() {
    let ws = tmp("slice2-protect");
    for d in [".git/hooks", ".tole"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
    let ws = std::fs::canonicalize(&*ws).unwrap();
    let c = all(Mode::Required);
    for k in [Consumer::Verify, Consumer::TurnEnd] {
        let a = argv(&c, k, &env_in(&ws), &sh_args());
        for rel in PROTECTED {
            let p = ws.join(rel).display().to_string();
            assert!(has(&a, &["--ro-bind", &p, &p]), "{k:?}: {rel} read-only");
        }
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
fn a_mission_and_a_stop_gate_are_unattended_so_optional_is_required() {
    assert!(Consumer::Verify.always_unattended());
    assert!(Consumer::TurnEnd.always_unattended());
    for k in [Consumer::RunCommand, Consumer::JobStart] {
        assert!(
            !k.always_unattended(),
            "{k:?}: only verify and the stop gates have nobody to answer"
        );
    }
    // `launch_command` passes `true` for them, and then a missing sandbox
    // refuses even for `optional`, which `run_command` would run unsandboxed
    // with a human attached.
    let c = all(Mode::Optional);
    for k in [Consumer::Verify, Consumer::TurnEnd] {
        let CommandLaunch::Refused(why) = plan(&c, &gone(), true, k) else {
            panic!("optional {k:?} without a sandbox must refuse")
        };
        assert!(
            why.starts_with(&format!("{}: ", k.name())) && why.contains("unattended"),
            "{why}"
        );
    }
}

#[test]
fn required_verify_and_turnend_without_a_backend_refuse_naming_the_key() {
    let c = all(Mode::Required);
    for k in [Consumer::Verify, Consumer::TurnEnd] {
        for unattended in [false, true] {
            let CommandLaunch::Refused(why) = plan(&c, &gone(), unattended, k) else {
                panic!("{k:?} must refuse")
            };
            assert!(
                why.contains(&format!("sandbox.consumers.{}", k.name())),
                "{why}"
            );
            assert!(
                why.contains("no bwrap") && why.contains("install bubblewrap"),
                "{why}"
            );
        }
    }
}

#[test]
fn an_optional_stop_gate_falls_back_only_when_a_human_is_attached() {
    let c = all(Mode::Optional);
    assert!(matches!(
        plan(&c, &gone(), false, Consumer::TurnEnd),
        CommandLaunch::Direct { warning: Some(_) }
    ));
    assert!(matches!(
        plan(&c, &gone(), true, Consumer::TurnEnd),
        CommandLaunch::Refused(_)
    ));
}

/// The stop gate is a program path or name; the tools take a bare name.
#[cfg(unix)]
#[test]
fn a_stop_gate_may_be_a_path_and_the_tools_may_not() {
    let ws = tmp("slice2-path-ws");
    let bin = tmp("slice2-path-bin");
    let gate = script(&bin, "gate.sh");
    std::fs::create_dir_all(ws.join("tools")).unwrap();
    script(&ws.join("tools"), "mine.sh");
    let ws = std::fs::canonicalize(&*ws).unwrap();
    let c = all(Mode::Required);
    let run = |k, prog: &str| plan_command(&c, &usable(), false, &env_in(&ws), k, (prog, &[]));
    let argv_of = |l: CommandLaunch| match l {
        CommandLaunch::Sandboxed { args, .. } => args
            .iter()
            .map(|x| x.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    // An absolute path outside the system dirs: its dir read-only, run by path.
    let a = argv_of(run(Consumer::TurnEnd, &gate.display().to_string()));
    let b = std::fs::canonicalize(&*bin).unwrap().display().to_string();
    assert!(has(&a, &["--ro-bind-try", &b, &b]), "{a:?}");
    assert_eq!(a.last().unwrap(), &format!("{b}/gate.sh"));
    // A relative path is the workspace's, not rebound.
    let a = argv_of(run(Consumer::TurnEnd, "./tools/mine.sh"));
    let w = ws.display().to_string();
    assert_eq!(a.last().unwrap(), &format!("{w}/tools/mine.sh"));
    let t = format!("{w}/tools");
    assert!(!has(&a, &["--ro-bind-try", &t, &t]));
    // A system program by path.
    assert!(matches!(
        run(Consumer::TurnEnd, "/bin/sh"),
        CommandLaunch::Sandboxed { .. }
    ));
    // Not an executable file, or missing: refused in every mode.
    for bad in ["/nonexistent/dir/x", "./tools/none.sh", "./tools"] {
        let CommandLaunch::Refused(why) = run(Consumer::TurnEnd, bad) else {
            panic!("{bad}: must refuse")
        };
        assert!(why.contains("not an executable file"), "{bad}: {why}");
    }
    // The other consumers still take a bare name only.
    for k in [Consumer::RunCommand, Consumer::JobStart, Consumer::Verify] {
        let CommandLaunch::Refused(why) = run(k, &gate.display().to_string()) else {
            panic!("{k:?}: a path must refuse")
        };
        assert!(why.contains("bare name"), "{why}");
    }
}

#[cfg(unix)]
#[test]
fn a_stop_gate_in_home_is_refused_not_bound() {
    let ws = tmp("slice2-home-ws");
    let home = tmp("slice2-home");
    let gate = script(&home, "gate.sh");
    let mut env = env_in(&ws);
    env.home = Some(home.to_path_buf());
    let CommandLaunch::Refused(why) = plan_command(
        &all(Mode::Required),
        &usable(),
        false,
        &env,
        Consumer::TurnEnd,
        (&gate.display().to_string(), &[]),
    ) else {
        panic!("a gate that lives in $HOME itself cannot be granted")
    };
    assert!(why.contains("$HOME"), "{why}");
}

#[test]
fn doctor_lists_verify_and_turnend_and_fails_for_a_required_one_without_a_sandbox() {
    let mut c = Consumers {
        verify: Mode::Required,
        turnend: Mode::Optional,
        ..Consumers::default()
    };
    let l = doctor_lines(&c, &usable());
    assert!(l[0].contains("verify required, turnend optional"), "{l:?}");
    assert!(!l.iter().any(|x| x.contains("ERROR")), "{l:?}");
    let l = doctor_lines(&c, &gone());
    assert!(
        l.iter()
            .any(|x| x.starts_with("sandbox consumers: ERROR verify")
                && x.contains("refused (required)")),
        "{l:?}"
    );
    // An optional stop gate cannot fall back either: nobody answers while the
    // model is finishing.
    assert!(
        l.iter()
            .any(|x| x.starts_with("sandbox consumers: ERROR turnend") && x.contains("unattended")),
        "{l:?}"
    );
    // An optional verify cannot fall back: a mission is unattended.
    c.verify = Mode::Optional;
    c.turnend = Mode::Off;
    let l = doctor_lines(&c, &gone());
    assert!(
        l.iter()
            .any(|x| x.starts_with("sandbox consumers: ERROR verify") && x.contains("mission")),
        "{l:?}"
    );
}

/// The launcher report has an exit code only when the command itself ended
/// (`child-pid` is written before the mounts are made, so it says nothing).
#[cfg(target_os = "linux")]
#[test]
fn the_launcher_report_has_an_exit_code_only_when_the_command_ended() {
    let started = r#"{ "child-pid": 5644, "pid-namespace": 4026532268 }"#;
    assert_eq!(exit_code_of(started), None, "started is not ended");
    let ended = format!("{started}\n{{ \"exit-code\": 7 }}\n");
    assert_eq!(exit_code_of(&ended), Some(7));
    assert_eq!(exit_code_of("{ \"exit-code\": 0 }"), Some(0));
    for junk in [
        "",
        "bwrap: Can't find source path /x",
        "{ \"exit-code\": \"x\" }",
    ] {
        assert_eq!(exit_code_of(junk), None, "{junk:?}");
    }
}

#[test]
fn doctor_warns_when_verify_or_a_gate_needs_a_toolchain_the_sandbox_cannot_see() {
    let home = tmp("toolchain-home");
    let c = Consumers {
        verify: Mode::Required,
        ..Consumers::default()
    };
    let lines = |c: &Consumers, h: Option<&Path>| doctor_lines_for(c, &usable(), h);
    // The exact notes: no run of spaces where a line continuation was meant.
    let note = |names: &str| {
        format!(
            "sandbox consumers: a sandboxed command ({names}) can also fail deterministically \
             for lack of network (network none unless the owner wrote host) or under the \
             address-space limit (memory_mb); the model is told to fix it like any failure"
        )
    };
    let warning = |names: &str, dirs: &str| {
        format!(
            "sandbox consumers: WARNING sandboxed {names}: {dirs} exist but are not visible \
             inside the sandbox ($HOME is absent), so a command that needs them (cargo, \
             rustc) fails every time, for example with exit 127, and uses up the three \
             strikes of a verify or the three denials of a stop gate"
        )
    };
    // No toolchain dir: no warning, but the plain note about deterministic
    // failures stays (network, address space).
    let l = lines(&c, Some(&home));
    assert!(!l.iter().any(|x| x.contains("WARNING sandboxed")), "{l:?}");
    assert!(l.contains(&note("verify")), "{l:?}");
    std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
    std::fs::create_dir_all(home.join(".rustup")).unwrap();
    let dirs = format!(
        "{}, {}",
        home.join(".cargo/bin").display(),
        home.join(".rustup").display()
    );
    let l = lines(&c, Some(&home));
    assert!(l.contains(&warning("verify", &dirs)), "{l:?}");
    assert!(l.contains(&note("verify")), "{l:?}");
    for x in &l {
        assert!(!x.contains("  "), "a run of spaces in {x:?}");
    }
    assert!(!l.iter().any(|x| x.contains("ERROR")), "a warning only");
    // The stop gates have the same problem, and the lines name them.
    let both = Consumers {
        verify: Mode::Required,
        turnend: Mode::Required,
        ..Consumers::default()
    };
    let l = lines(&both, Some(&home));
    assert!(l.contains(&warning("verify and turnend", &dirs)), "{l:?}");
    let gate = Consumers {
        turnend: Mode::Required,
        ..Consumers::default()
    };
    let l = lines(&gate, Some(&home));
    assert!(l.contains(&warning("turnend", &dirs)), "{l:?}");
    assert!(l.contains(&note("turnend")), "{l:?}");
    // Both off: nothing to say about them.
    let off = lines(&Consumers::default(), Some(&home));
    assert!(
        !off.iter().any(|x| x.contains("WARNING sandboxed")),
        "{off:?}"
    );
}

// Nested gitdirs (#849 follow-up): what the plan names, and what it refuses.

/// A fresh resolved directory and the guard that removes it.
fn keep(tag: &str) -> (TestDir, PathBuf) {
    let g = tmp(tag);
    let p = std::fs::canonicalize(&*g).unwrap();
    (g, p)
}

fn rw(p: &Path) -> String {
    p.display().to_string()
}

/// A registered submodule: its `.git` is a gitdir FILE inside the workspace,
/// pointing into the (read-only) top-level `.git/modules`.
#[cfg(unix)]
#[test]
fn a_submodule_listed_in_gitmodules_gets_its_git_file_bound_read_only() {
    let (_guard, ws) = keep("nested-sub");
    std::fs::create_dir_all(ws.join(".git/modules/sub")).unwrap();
    std::fs::create_dir_all(ws.join("libs/sub")).unwrap();
    std::fs::write(ws.join("libs/sub/.git"), "gitdir: ../../.git/modules/sub\n").unwrap();
    std::fs::write(
        ws.join(".gitmodules"),
        "[submodule \"sub\"]\n\tpath = libs/sub\n\turl = x\n",
    )
    .unwrap();
    let plan = protect_plan(&ws).unwrap();
    assert!(plan.contains(&ws.join(".git")));
    assert!(plan.contains(&ws.join("libs/sub/.git")), "{plan:?}");
    // The file that names the submodules is read-only too (the next plan reads it).
    assert!(plan.contains(&ws.join(".gitmodules")), "{plan:?}");
    // The gitdir it points to is inside the already read-only .git.
    assert!(!plan.contains(&ws.join(".git/modules/sub")), "{plan:?}");
    let a = run_argv(&ws);
    let f = rw(&ws.join("libs/sub/.git"));
    assert!(has(&a, &["--ro-bind", &f, &f]));
}

/// `git init --separate-git-dir=ws/.gd`: the top-level `.git` file points into
/// the workspace, and the target is bound read-only too.
#[cfg(unix)]
#[test]
fn a_top_level_gitdir_file_pointing_inside_the_workspace_binds_the_target_too() {
    let (_guard, ws) = keep("sepgit");
    std::fs::create_dir_all(ws.join(".gd/hooks")).unwrap();
    std::fs::write(
        ws.join(".git"),
        format!("gitdir: {}\n", ws.join(".gd").display()),
    )
    .unwrap();
    let plan = protect_plan(&ws).unwrap();
    assert!(plan.contains(&ws.join(".gd")), "{plan:?}");
    assert!(plan.contains(&ws.join(".git")), "{plan:?}");
    // A relative target, and one that leaves the workspace (not in the view).
    std::fs::write(ws.join(".git"), "gitdir: .gd\n").unwrap();
    assert!(protect_plan(&ws).unwrap().contains(&ws.join(".gd")));
    std::fs::write(ws.join(".git"), "gitdir: /elsewhere/x\n").unwrap();
    assert_eq!(protect_plan(&ws).unwrap(), [ws.join(".git")]);
}

#[cfg(unix)]
#[test]
fn child_agent_worktrees_get_their_git_file_bound_read_only() {
    let (_guard, ws) = keep("wts");
    std::fs::create_dir_all(ws.join(".git/worktrees/wt1")).unwrap();
    std::fs::create_dir_all(ws.join(".tole-worktrees/wt1")).unwrap();
    std::fs::write(
        ws.join(".tole-worktrees/wt1/.git"),
        format!("gitdir: {}\n", ws.join(".git/worktrees/wt1").display()),
    )
    .unwrap();
    let plan = protect_plan(&ws).unwrap();
    // The whole directory is read-only, which covers every worktree `.git` file.
    assert!(plan.contains(&ws.join(".tole-worktrees")), "{plan:?}");
    assert!(
        !plan.contains(&ws.join(".tole-worktrees/wt1/.git")),
        "{plan:?}"
    );
}

/// Refused, not skipped: a link anywhere in the chain, a path that leaves the
/// workspace, too many entries.
#[cfg(unix)]
#[test]
fn a_link_or_an_escape_in_the_nested_chain_is_refused() {
    use std::os::unix::fs::symlink;
    let guards = std::cell::RefCell::new(Vec::new());
    let fresh = |tag: &str| {
        let (g, p) = keep(tag);
        guards.borrow_mut().push(g);
        p
    };
    // `.gitmodules` that is a link.
    let ws = fresh("nest-l1");
    std::fs::write(ws.join("real"), "").unwrap();
    symlink(ws.join("real"), ws.join(".gitmodules")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));
    // A submodule path that is a link, or passes through one.
    let ws = fresh("nest-l2");
    std::fs::create_dir_all(ws.join("real")).unwrap();
    symlink(ws.join("real"), ws.join("sub")).unwrap();
    std::fs::write(ws.join(".gitmodules"), "[submodule \"s\"]\n\tpath = sub\n").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));
    // Paths that leave the workspace.
    for bad in ["../outside", "/etc", "a/../../b", ""] {
        let ws = fresh("nest-esc");
        std::fs::write(
            ws.join(".gitmodules"),
            format!("[submodule \"s\"]\n\tpath = {bad}\n"),
        )
        .unwrap();
        assert!(
            protect_plan(&ws).unwrap_err().contains(".gitmodules"),
            "{bad:?}"
        );
    }
    // A gitdir target that passes through a link inside the workspace.
    let ws = fresh("nest-l3");
    std::fs::create_dir_all(ws.join("real/gd")).unwrap();
    symlink(ws.join("real"), ws.join("lnk")).unwrap();
    std::fs::write(ws.join(".git"), "gitdir: lnk/gd\n").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));
    // More than the bound.
    let ws = fresh("nest-many");
    let many: String = (0..=MAX_NESTED)
        .map(|i| format!("[submodule \"s{i}\"]\n\tpath = m{i}\n"))
        .collect();
    std::fs::write(ws.join(".gitmodules"), many).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("more than"));
}

/// Pinned so the doc stays exact: no workspace scan, a nested repository that
/// is in neither `.gitmodules` nor `.tole-worktrees` is not protected.
#[test]
fn an_arbitrary_nested_repository_is_not_protected() {
    let (_guard, ws) = keep("nest-arb");
    std::fs::create_dir_all(ws.join("vendor/x/.git/hooks")).unwrap();
    assert!(protect_plan(&ws).unwrap().is_empty());
}

/// #849 (TOCTOU): the plan is made again right before the command line is
/// handed out. A `.git` that appeared since, or a directory swapped for a
/// link, refuses the launch.
#[cfg(unix)]
#[test]
fn a_path_that_appears_or_becomes_a_link_between_plan_and_launch_is_refused() {
    use std::os::unix::fs::symlink;
    let (_guard, ws) = keep("recheck");
    let c = on(Mode::Required, Mode::Required);
    let args_of = |ws: &Path| match plan_command(
        &c,
        &usable(),
        false,
        &env_in(ws),
        Consumer::RunCommand,
        ("sh", &[]),
    ) {
        CommandLaunch::Sandboxed { args, .. } => args,
        other => panic!("{other:?}"),
    };
    // Planned with no `.git`; one appears.
    let args = args_of(&ws);
    assert!(recheck_plan(&ws, &args).is_ok());
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    let e = recheck_plan(&ws, &args).unwrap_err();
    assert!(e.contains("appeared or changed"), "{e}");
    // Planned with a real `.git`; swapped for a link.
    let args = args_of(&ws);
    assert!(recheck_plan(&ws, &args).is_ok());
    std::fs::remove_dir_all(ws.join(".git")).unwrap();
    std::fs::create_dir_all(ws.join("elsewhere/hooks")).unwrap();
    symlink(ws.join("elsewhere"), ws.join(".git")).unwrap();
    let e = recheck_plan(&ws, &args).unwrap_err();
    assert!(e.contains("symbolic link"), "{e}");
}

/// A `.gitmodules` padded past the read bound is refused, not read in part:
/// git on the host parses all of it, so a registered submodule whose `path`
/// lies past the cut would go unprotected.
#[test]
fn an_oversized_gitmodules_or_gitdir_file_is_refused_not_read_in_part() {
    let (_g, ws) = keep("oversize-modules");
    let pad = "# ".to_string() + &"x".repeat(70 * 1024) + "\n";
    std::fs::write(
        ws.join(".gitmodules"),
        format!("{pad}[submodule \"s\"]\n\tpath = libs/s\n"),
    )
    .unwrap();
    let e = protect_plan(&ws).unwrap_err();
    assert!(
        e.contains(".gitmodules") && e.contains("larger than"),
        "{e}"
    );
    // Exactly at the bound is fine; one byte over is not.
    let at = format!("{}\n", "#".repeat(64 * 1024 - 1));
    std::fs::write(ws.join(".gitmodules"), &at).unwrap();
    assert!(protect_plan(&ws).is_ok());
    std::fs::write(ws.join(".gitmodules"), format!("{at}#")).unwrap();
    assert!(protect_plan(&ws).is_err());
    // A gitdir file with padding is refused too.
    let (_g2, ws2) = keep("oversize-gitdir");
    std::fs::write(
        ws2.join(".git"),
        format!("gitdir: .gd\n{}\n", "x".repeat(5000)),
    )
    .unwrap();
    assert!(protect_plan(&ws2).unwrap_err().contains("larger than"));
}

/// A mount stops writes by path, not by inode, and a gitdir the host's git
/// follows must not have another way in: a link inside the target's `hooks`,
/// a second hard link to its config or a hook, a hard-linked gitdir file.
#[cfg(unix)]
#[test]
fn another_name_or_link_into_what_git_runs_is_refused() {
    use std::os::unix::fs::symlink;
    let fresh = |tag: &str| keep(tag);
    // hooks entry that is a link, in a `.git` directory.
    let (_g, ws) = fresh("alias-hooklink");
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    std::fs::write(ws.join("x.sh"), "#!/bin/sh\n").unwrap();
    symlink(ws.join("x.sh"), ws.join(".git/hooks/pre-commit")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));
    // The same in the target of a gitdir FILE (separate git dir).
    let (_g, ws) = fresh("alias-targetlink");
    std::fs::create_dir_all(ws.join(".gd/hooks")).unwrap();
    std::fs::write(ws.join("x.sh"), "#!/bin/sh\n").unwrap();
    symlink(ws.join("x.sh"), ws.join(".gd/hooks/pre-commit")).unwrap();
    std::fs::write(ws.join(".git"), "gitdir: .gd\n").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));
    // `hooks` itself a link inside the target.
    let (_g, ws) = fresh("alias-targethooks");
    std::fs::create_dir_all(ws.join(".gd")).unwrap();
    std::fs::create_dir_all(ws.join("realhooks")).unwrap();
    symlink(ws.join("realhooks"), ws.join(".gd/hooks")).unwrap();
    std::fs::write(ws.join(".git"), "gitdir: .gd\n").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));
    // A second hard link to the config, to a hook, to a gitdir file.
    let (_g, ws) = fresh("alias-hard-config");
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
    std::fs::hard_link(ws.join(".git/config"), ws.join("config-twin")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("hard links"));
    std::fs::remove_file(ws.join("config-twin")).unwrap();
    assert!(protect_plan(&ws).is_ok());
    std::fs::write(ws.join(".git/hooks/pre-commit"), "x").unwrap();
    std::fs::hard_link(ws.join(".git/hooks/pre-commit"), ws.join("hook-twin")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("hard links"));
    let (_g, ws) = fresh("alias-hard-gitfile");
    std::fs::write(ws.join(".git"), "gitdir: /elsewhere\n").unwrap();
    std::fs::hard_link(ws.join(".git"), ws.join("git-twin")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("hard links"));
}

/// A FIFO named `.git`, `.gitmodules` or a submodule's `.git` must not hang the
/// plan (opening a FIFO for reading blocks until a writer shows up).
#[cfg(unix)]
#[test]
fn a_fifo_in_the_protected_names_does_not_hang_the_plan() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let mkfifo = |p: &Path| {
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    };
    let bounded = |ws: PathBuf| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(protect_plan(&ws));
        });
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("protect_plan hung on a FIFO")
    };
    let (_g, ws) = keep("fifo-modules");
    mkfifo(&ws.join(".gitmodules"));
    assert!(bounded(ws).unwrap_err().contains("not a regular file"));
    let (_g, ws) = keep("fifo-git");
    mkfifo(&ws.join(".git"));
    let plan = bounded(ws.clone()).unwrap();
    assert_eq!(plan, [ws.join(".git")], "bound, never read");
    let (_g, ws) = keep("fifo-sub");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    mkfifo(&ws.join("sub/.git"));
    std::fs::write(ws.join(".gitmodules"), "[submodule \"s\"]\n\tpath = sub\n").unwrap();
    assert!(bounded(ws.clone()).unwrap().contains(&ws.join("sub/.git")));
}

/// Git reads `.gitmodules` as a config file, and the file is writable from the
/// sandbox: whatever git would read differently is refused, not guessed.
#[test]
fn a_gitmodules_git_could_read_differently_is_refused() {
    let plan = |text: &str| {
        let (_g, ws) = keep("gm-syntax");
        std::fs::write(ws.join(".gitmodules"), text).unwrap();
        protect_plan(&ws)
    };
    for (bad, why) in [
        ("[submodule \"s\"]\n\tpath = sub # note\n", "inline #"),
        ("[submodule \"s\"]\n\tpath = sub ; note\n", "inline ;"),
        ("[submodule \"s\"]\n\tpath = \"sub\"\n", "quoted value"),
        ("[submodule \"s\"]\n\tpath = su\\\n\tb\n", "continuation"),
        (
            "[submodule \"s\"]\n\turl = http://h/x#frag\n",
            "# in another value",
        ),
        ("[core]\n\tpath = sub\n", "other section"),
        ("[submodule.s]\n\tpath = sub\n", "old header syntax"),
        ("[submodule \"a\\\"b\"]\n\tpath = sub\n", "escaped name"),
        ("path = sub\n", "key outside a section"),
        ("[submodule \"s\"]\n\tpath sub\n", "not key = value"),
    ] {
        let e = plan(bad).unwrap_err();
        assert!(
            e.contains("plain") && e.contains(".gitmodules"),
            "{why}: {e}"
        );
    }
    // The plain subset git reads the same way: case-insensitive names, CRLF,
    // whole-line comments, other plain keys.
    let (_g, ws) = keep("gm-plain");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/.git"), "gitdir: /elsewhere\n").unwrap();
    std::fs::write(
        ws.join(".gitmodules"),
        "# comment\r\n[Submodule \"s\"]\r\n\tPATH = sub\r\n\turl = https://h/x\r\n; more\r\n",
    )
    .unwrap();
    assert!(protect_plan(&ws).unwrap().contains(&ws.join("sub/.git")));
}

/// Layouts the plan cannot cover are refused, not half protected: a submodule
/// with submodules of its own, a gitdir that redirects to another directory
/// (`commondir`, `config.worktree`), a target that does not exist, a second
/// name for `.gitmodules`.
#[cfg(unix)]
#[test]
fn nested_submodules_commondir_redirects_and_missing_targets_are_refused() {
    // F1: a registered submodule that has its own `.gitmodules`.
    let (_g, ws) = keep("refuse-nested-submodules");
    std::fs::create_dir_all(ws.join("mid")).unwrap();
    std::fs::write(ws.join("mid/.git"), "gitdir: /elsewhere\n").unwrap();
    std::fs::write(ws.join(".gitmodules"), "[submodule \"m\"]\n\tpath = mid\n").unwrap();
    assert!(protect_plan(&ws).is_ok(), "a plain submodule is fine");
    std::fs::write(
        ws.join("mid/.gitmodules"),
        "[submodule \"i\"]\n\tpath = inner\n",
    )
    .unwrap();
    let e = protect_plan(&ws).unwrap_err();
    assert!(e.contains("nested submodules") && e.contains("mid"), "{e}");
    // A link named `.gitmodules` in the submodule is refused too (not followed).
    std::fs::remove_file(ws.join("mid/.gitmodules")).unwrap();
    std::os::unix::fs::symlink(ws.join("x"), ws.join("mid/.gitmodules")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));

    // F2: the in-workspace target of a gitdir file redirects with `commondir`.
    let (_g, ws) = keep("refuse-commondir");
    std::fs::create_dir_all(ws.join("repo.git/worktrees/ws2")).unwrap();
    std::fs::write(ws.join("repo.git/worktrees/ws2/commondir"), "../..\n").unwrap();
    std::fs::write(
        ws.join(".git"),
        format!("gitdir: {}\n", ws.join("repo.git/worktrees/ws2").display()),
    )
    .unwrap();
    let e = protect_plan(&ws).unwrap_err();
    assert!(e.contains("commondir") && e.contains("redirects"), "{e}");
    // The same for config.worktree, and for a top-level `.git` DIRECTORY.
    std::fs::remove_file(ws.join("repo.git/worktrees/ws2/commondir")).unwrap();
    std::fs::write(ws.join("repo.git/worktrees/ws2/config.worktree"), "").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("config.worktree"));
    let (_g, ws) = keep("refuse-commondir-dir");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    assert!(protect_plan(&ws).is_ok());
    std::fs::write(ws.join(".git/commondir"), "../other\n").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("commondir"));
    // Inside the already read-only `.git` a worktree's `commondir` is normal.
    let (_g, ws) = keep("commondir-covered");
    std::fs::create_dir_all(ws.join(".git/worktrees/w")).unwrap();
    std::fs::write(ws.join(".git/worktrees/w/commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(
        ws.join("sub/.git"),
        format!("gitdir: {}\n", ws.join(".git/worktrees/w").display()),
    )
    .unwrap();
    std::fs::write(ws.join(".gitmodules"), "[submodule \"s\"]\n\tpath = sub\n").unwrap();
    assert!(protect_plan(&ws).is_ok());

    // H2: an in-workspace gitdir target that does not exist (outside: unchanged).
    let (_g, ws) = keep("refuse-missing-target");
    std::fs::write(ws.join(".git"), "gitdir: .gd\n").unwrap();
    let e = protect_plan(&ws).unwrap_err();
    assert!(e.contains("does not exist"), "{e}");
    std::fs::write(ws.join(".git"), "gitdir: /elsewhere/none\n").unwrap();
    assert!(protect_plan(&ws).is_ok());

    // H1: a second hard link to `.gitmodules`.
    let (_g, ws) = keep("refuse-modules-twin");
    std::fs::write(ws.join(".gitmodules"), "[submodule \"s\"]\n\tpath = sub\n").unwrap();
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/.git"), "gitdir: /elsewhere\n").unwrap();
    std::fs::hard_link(ws.join(".gitmodules"), ws.join("modules-twin")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("hard links"));
}

/// A submodule listed in `.gitmodules` but not initialized has no `.git` to
/// bind; a command could create one. Refused until initialized or removed.
#[cfg(unix)]
#[test]
fn an_uninitialized_submodule_refuses_the_launch() {
    let listed = "[submodule \"s\"]\n\tpath = sub\n";
    // Missing path.
    let (_g, ws) = keep("uninit-missing");
    std::fs::write(ws.join(".gitmodules"), listed).unwrap();
    let e = protect_plan(&ws).unwrap_err();
    assert!(
        e.contains("not initialized") && e.contains("sub") && e.contains("submodule update --init"),
        "{e}"
    );
    // Empty directory (a clone without --recursive), and a directory with no .git.
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("not initialized"));
    std::fs::write(ws.join("sub/file"), "x").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("not initialized"));
    // A regular file where the submodule should be.
    let (_g, ws) = keep("uninit-file");
    std::fs::write(ws.join(".gitmodules"), listed).unwrap();
    std::fs::write(ws.join("sub"), "x").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("not initialized"));
    // Initialized: fine.
    let (_g, ws) = keep("uninit-ok");
    std::fs::write(ws.join(".gitmodules"), listed).unwrap();
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/.git"), "gitdir: /elsewhere\n").unwrap();
    assert!(protect_plan(&ws).is_ok());
}

/// A linked worktree inside the workspace but outside `.tole-worktrees` cannot
/// be protected: it is found through the registry of the gitdir in effect and
/// refused. A broken registry is refused, never skipped.
#[cfg(unix)]
#[test]
fn a_linked_worktree_elsewhere_in_the_workspace_is_refused() {
    let register = |gitdir: &Path, id: &str, wt_git: &Path| {
        std::fs::create_dir_all(gitdir.join("worktrees").join(id)).unwrap();
        std::fs::write(
            gitdir.join("worktrees").join(id).join("gitdir"),
            format!("{}\n", wt_git.display()),
        )
        .unwrap();
    };
    // Top-level `.git` directory.
    let (_g, ws) = keep("lwt-dir");
    std::fs::create_dir_all(ws.join("other/wt")).unwrap();
    std::fs::write(ws.join("other/wt/.git"), "gitdir: x\n").unwrap();
    register(&ws.join(".git"), "w1", &ws.join("other/wt/.git"));
    let e = protect_plan(&ws).unwrap_err();
    assert!(
        e.contains("linked worktree")
            && e.contains("other/wt/.git")
            && e.contains(".tole-worktrees"),
        "{e}"
    );
    // The target of a gitdir FILE (separate git dir) is read the same way.
    let (_g, ws) = keep("lwt-sep");
    std::fs::create_dir_all(ws.join("other/wt")).unwrap();
    std::fs::write(ws.join("other/wt/.git"), "gitdir: x\n").unwrap();
    std::fs::create_dir_all(ws.join(".gd")).unwrap();
    std::fs::write(ws.join(".git"), "gitdir: .gd\n").unwrap();
    register(&ws.join(".gd"), "w1", &ws.join("other/wt/.git"));
    assert!(protect_plan(&ws).unwrap_err().contains("linked worktree"));
    // Under `.tole-worktrees` (bound whole), outside the workspace (not in the
    // view) and stale (the path is gone) are fine.
    let (_g, ws) = keep("lwt-fine");
    std::fs::create_dir_all(ws.join(".tole-worktrees/a")).unwrap();
    std::fs::write(ws.join(".tole-worktrees/a/.git"), "gitdir: x\n").unwrap();
    register(&ws.join(".git"), "a", &ws.join(".tole-worktrees/a/.git"));
    register(&ws.join(".git"), "b", Path::new("/elsewhere/wt/.git"));
    register(&ws.join(".git"), "c", &ws.join("gone/.git"));
    assert!(protect_plan(&ws).is_ok());

    // Refused, never skipped: a link or non-directory `worktrees`, a link or
    // non-regular back-pointer, too many entries, an oversized pointer.
    let (_g, ws) = keep("lwt-link-registry");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::create_dir_all(ws.join("real")).unwrap();
    std::os::unix::fs::symlink(ws.join("real"), ws.join(".git/worktrees")).unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("not a directory"));
    let (_g, ws) = keep("lwt-file-registry");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(ws.join(".git/worktrees"), "x").unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("not a directory"));
    let (_g, ws) = keep("lwt-link-pointer");
    std::fs::create_dir_all(ws.join(".git/worktrees/w1")).unwrap();
    std::fs::write(ws.join("target"), "/elsewhere/.git\n").unwrap();
    std::os::unix::fs::symlink(ws.join("target"), ws.join(".git/worktrees/w1/gitdir")).unwrap();
    assert!(protect_plan(&ws)
        .unwrap_err()
        .contains("not a regular file"));
    let (_g, ws) = keep("lwt-dir-pointer");
    std::fs::create_dir_all(ws.join(".git/worktrees/w1/gitdir")).unwrap();
    assert!(protect_plan(&ws)
        .unwrap_err()
        .contains("not a regular file"));
    let (_g, ws) = keep("lwt-many");
    for i in 0..=MAX_NESTED {
        register(
            &ws.join(".git"),
            &format!("w{i}"),
            Path::new("/elsewhere/.git"),
        );
    }
    assert!(protect_plan(&ws).unwrap_err().contains("more than"));
    let (_g, ws) = keep("lwt-big");
    std::fs::create_dir_all(ws.join(".git/worktrees/w1")).unwrap();
    std::fs::write(
        ws.join(".git/worktrees/w1/gitdir"),
        format!("/elsewhere/{}\n", "x".repeat(5000)),
    )
    .unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("larger than"));
}

// The sessions directory in the plan (#866).

/// A workspace with a `.git` and the given extra directories.
fn sess_ws(tag: &str, dirs: &[&str]) -> (TestDir, PathBuf) {
    let (g, ws) = keep(tag);
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
    for d in dirs {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    (g, ws)
}

#[test]
fn a_direct_child_sessions_dir_is_bound_read_only_after_the_git_dir() {
    let (_g, ws) = sess_ws("sess-child", &["sessions"]);
    let plan = protect_plan_with(&ws, Some(&ws.join("sessions"))).unwrap();
    assert_eq!(plan, [ws.join(".git"), ws.join("sessions")]);
    // The same plan the argument builder uses reaches the bwrap argv.
    let a = argv_with_extra_for(&ws, &plan);
    let p = ws.join("sessions").display().to_string();
    assert!(has(&a, &["--ro-bind", &p, &p]), "{a:?}");
    // Without a sessions dir nothing changes (the existing plan).
    assert_eq!(protect_plan_with(&ws, None).unwrap(), [ws.join(".git")]);
}

/// Every protected path of a plan is one `--ro-bind p p` in the argv.
fn argv_with_extra_for(ws: &Path, plan: &[PathBuf]) -> Vec<String> {
    let mut profile = command_profile(Network::None);
    profile.protect = plan.to_vec();
    bwrap_args_for(&profile, Some(ws), None, Path::new("/bin/sh"), &[], true)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn a_sessions_dir_outside_missing_or_already_protected_adds_nothing() {
    let (_g, ws) = sess_ws("sess-none", &[".tole/sessions"]);
    let outside = tmp("sess-outside");
    // Outside the workspace: not in the sandbox's view.
    assert_eq!(
        protect_plan_with(&ws, Some(&outside)).unwrap(),
        [ws.join(".git"), ws.join(".tole")]
    );
    // Not created yet: nothing to bind (like `.tole`).
    assert_eq!(
        protect_plan_with(&ws, Some(&ws.join("later"))).unwrap(),
        [ws.join(".git"), ws.join(".tole")]
    );
    // The default location is under `.tole`, already read-only.
    assert_eq!(
        protect_plan_with(&ws, Some(&ws.join(".tole/sessions"))).unwrap(),
        [ws.join(".git"), ws.join(".tole")]
    );
    // A `..` that comes back inside is the same path.
    let (_g2, ws2) = sess_ws("sess-dots", &["sessions"]);
    let p = ws2.join("x/../sessions");
    assert_eq!(
        protect_plan_with(&ws2, Some(&p)).unwrap(),
        [ws2.join(".git"), ws2.join("sessions")]
    );
}

#[cfg(unix)]
#[test]
fn a_sessions_dir_that_cannot_be_protected_is_an_error() {
    let err = |ws: &Path, d: &Path| protect_plan_with(ws, Some(d)).unwrap_err();
    // Deeper than a direct child (its parent could be renamed and replaced).
    let (_g, ws) = sess_ws("sess-deep", &["a/sessions"]);
    let e = err(&ws, &ws.join("a/sessions"));
    assert!(
        e.contains("deeper than a direct child") && e.contains(".tole/sessions"),
        "{e}"
    );
    // The workspace itself.
    let (_g, ws) = sess_ws("sess-ws", &[]);
    assert!(err(&ws, &ws).contains("is the workspace itself"));
    // A symbolic link, a file.
    let (_g, ws) = sess_ws("sess-link", &["real"]);
    std::os::unix::fs::symlink(ws.join("real"), ws.join("sessions")).unwrap();
    assert!(err(&ws, &ws.join("sessions")).contains("symbolic link"));
    let (_g, ws) = sess_ws("sess-file", &[]);
    std::fs::write(ws.join("sessions"), "x").unwrap();
    assert!(err(&ws, &ws.join("sessions")).contains("not a directory"));
    // Outside by name but a link into the workspace: the same place.
    let (_g, ws) = sess_ws("sess-alias", &["sessions"]);
    let outside = tmp("sess-alias-out");
    std::os::unix::fs::symlink(ws.join("sessions"), outside.join("alias")).unwrap();
    assert!(err(&ws, &outside.join("alias")).contains("through a link"));
    // The existing refusals are intact: a symlinked `.tole` still refuses
    // whatever the sessions dir is.
    let (_g, ws) = sess_ws("sess-tole", &["real-tole", "sessions"]);
    std::os::unix::fs::symlink(ws.join("real-tole"), ws.join(".tole")).unwrap();
    assert!(protect_plan_with(&ws, Some(&ws.join("sessions")))
        .unwrap_err()
        .contains(".tole"));
}

#[test]
fn set_sessions_dir_wants_an_absolute_path() {
    assert!(set_sessions_dir(PathBuf::from("relative/sessions"))
        .unwrap_err()
        .contains("absolute"));
}

/// A sessions directory named through a link in its PARENT chain (a working
/// directory under a symlink) is the same place as the real path: bound by its
/// real path, not refused.
#[cfg(unix)]
#[test]
fn a_sessions_dir_named_through_a_linked_parent_is_bound_by_its_real_path() {
    let (_g, ws) = sess_ws("sess-parent-link", &["sessions"]);
    let holder = tmp("sess-parent-link-holder");
    std::os::unix::fs::symlink(&ws, holder.join("ws-link")).unwrap();
    let named = holder.join("ws-link/sessions");
    assert_eq!(
        protect_plan_with(&ws, Some(&named)).unwrap(),
        [ws.join(".git"), ws.join("sessions")]
    );
}

/// A `..` that comes after a symbolic link means the LINK TARGET's parent: the
/// host (git, the session writer) resolves it that way, so collapsing it as
/// text names a different directory and the plan would protect nothing.
#[cfg(unix)]
#[test]
fn a_dotdot_after_a_link_is_resolved_by_the_os_not_collapsed_as_text() {
    // base/out/lnk -> ws/a/b ; `-s base/out/lnk/../sessions` is ws/a/sessions,
    // deeper than a direct child: refused (it used to be Ok with nothing bound).
    let (_g, ws) = sess_ws("sess-dotdot", &["a/sessions", "a/b"]);
    let out = tmp("sess-dotdot-out");
    std::fs::create_dir_all(out.join("out")).unwrap();
    std::os::unix::fs::symlink(ws.join("a/b"), out.join("out/lnk")).unwrap();
    let named = out.join("out/lnk/../sessions");
    let e = protect_plan_with(&ws, Some(&named)).unwrap_err();
    assert!(e.contains("deeper than a direct child"), "{e}");
    // The same trick naming a direct child is bound by its real path.
    let (_g, ws) = sess_ws("sess-dotdot-ok", &["sessions", "x"]);
    let out = tmp("sess-dotdot-ok-out");
    std::os::unix::fs::symlink(ws.join("x"), out.join("lnk")).unwrap();
    assert_eq!(
        protect_plan_with(&ws, Some(&out.join("lnk/../sessions"))).unwrap(),
        [ws.join(".git"), ws.join("sessions")]
    );
}

/// The default location with no `.tole` yet and nothing created: no error (the
/// `deeper` rule applies to a directory that exists).
#[test]
fn an_absent_default_sessions_dir_is_not_an_error() {
    let (_g, ws) = sess_ws("sess-absent-default", &[]);
    assert_eq!(
        protect_plan_with(&ws, Some(&ws.join(".tole/sessions"))).unwrap(),
        [ws.join(".git")]
    );
}

/// The same `..` after a link, in the other places the plan reads a path from:
/// a gitdir file's target, and a linked worktree's back-pointer.
#[cfg(unix)]
#[test]
fn a_dotdot_after_a_link_in_a_gitdir_or_worktree_pointer_is_not_collapsed_either() {
    // `.git` is a file: `gitdir: lnk/../gd`, lnk -> ws/b/c. Git resolves it to
    // ws/b/gd (the real gitdir); as text it is ws/gd (a decoy that exists).
    let (_g, ws) = keep("dotdot-gitdir");
    for d in ["b/c", "b/gd/hooks", "gd/hooks"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    for g in ["b/gd", "gd"] {
        std::fs::write(ws.join(g).join("config"), "[core]\n").unwrap();
    }
    std::os::unix::fs::symlink(ws.join("b/c"), ws.join("lnk")).unwrap();
    std::fs::write(ws.join(".git"), "gitdir: lnk/../gd\n").unwrap();
    let e = protect_plan(&ws).unwrap_err();
    assert!(
        e.contains("symbolic link") && e.contains("lnk/../gd"),
        "{e}"
    );

    // A linked worktree whose back-pointer reads `<ws>/lnk/../wt/.git`: the
    // text is ws/wt/.git (absent), the real place is ws/b/wt/.git (inside the
    // workspace, outside .tole-worktrees): refused like any linked worktree.
    let (_g, ws) = sess_ws("dotdot-worktree", &["b/c", "b/wt", ".git/worktrees/w1"]);
    std::fs::write(ws.join("b/wt/.git"), "gitdir: x\n").unwrap();
    std::os::unix::fs::symlink(ws.join("b/c"), ws.join("lnk")).unwrap();
    std::fs::write(
        ws.join(".git/worktrees/w1/gitdir"),
        format!("{}/lnk/../wt/.git\n", ws.display()),
    )
    .unwrap();
    let e = protect_plan(&ws).unwrap_err();
    assert!(e.contains("linked worktree"), "{e}");
}

/// The doctor tells where the way out of a read-only `.git` is when any
/// command consumer is on, and says nothing about it when none is.
#[test]
fn doctor_points_at_the_git_tool_only_when_a_consumer_is_on() {
    let find = |c: &Consumers| {
        doctor_lines(c, &usable())
            .into_iter()
            .find(|l| l.contains("git writes"))
    };
    assert_eq!(find(&Consumers::default()), None);
    let l = find(&Consumers {
        turnend: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
    assert!(
        l.starts_with("sandbox consumers: git writes") && l.contains("`git` tool"),
        "{l}"
    );
}
