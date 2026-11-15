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
            for k in [Consumer::RunCommand, Consumer::JobStart] {
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
    assert_eq!((c.run_command, c.job_start), (Mode::Off, Mode::Off));
    assert_eq!(c.network, Network::None);
    assert_eq!(consumers(), c, "nothing set: everything off");
}

#[test]
fn doctor_says_what_is_there_and_that_off_is_off() {
    let off = doctor_lines(&Consumers::default(), &usable());
    assert!(off[0].contains("run_command off, job_start off"), "{off:?}");
    assert!(off
        .iter()
        .any(|l| l.contains("verify") && l.contains("not sandboxed yet")));
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

/// A linked worktree whose `.git` file lives anywhere in the workspace is found
/// through the back-pointer in the read-only `.git/worktrees/<id>/gitdir`.
#[cfg(unix)]
#[test]
fn a_linked_worktree_elsewhere_in_the_workspace_gets_its_git_file_bound() {
    let (_g, ws) = keep("linked-wt");
    std::fs::create_dir_all(ws.join(".git/worktrees/w1")).unwrap();
    std::fs::create_dir_all(ws.join("other/wt")).unwrap();
    std::fs::write(
        ws.join("other/wt/.git"),
        format!("gitdir: {}\n", ws.join(".git/worktrees/w1").display()),
    )
    .unwrap();
    std::fs::write(
        ws.join(".git/worktrees/w1/gitdir"),
        format!("{}\n", ws.join("other/wt/.git").display()),
    )
    .unwrap();
    std::fs::write(ws.join(".git/worktrees/w1/commondir"), "../..\n").unwrap();
    let plan = protect_plan(&ws).unwrap();
    assert!(plan.contains(&ws.join("other/wt/.git")), "{plan:?}");
    // A back-pointer outside the workspace is not in the view; a stale one
    // (the worktree is gone) binds nothing; a link in the chain is refused.
    std::fs::write(ws.join(".git/worktrees/w1/gitdir"), "/elsewhere/wt/.git\n").unwrap();
    assert_eq!(protect_plan(&ws).unwrap(), [ws.join(".git")]);
    std::fs::write(
        ws.join(".git/worktrees/w1/gitdir"),
        format!("{}\n", ws.join("gone/.git").display()),
    )
    .unwrap();
    assert_eq!(protect_plan(&ws).unwrap(), [ws.join(".git")]);
    std::os::unix::fs::symlink(ws.join("other"), ws.join("lnk")).unwrap();
    std::fs::write(
        ws.join(".git/worktrees/w1/gitdir"),
        format!("{}\n", ws.join("lnk/wt/.git").display()),
    )
    .unwrap();
    assert!(protect_plan(&ws).unwrap_err().contains("symbolic link"));
}
