use super::*;

fn args_of(profile: &Profile, ws: Option<&Path>) -> Vec<String> {
    bwrap_args(
        profile,
        ws,
        None,
        Path::new("/usr/bin/true"),
        &["--flag".to_string()],
    )
    .iter()
    .map(|a| a.to_string_lossy().into_owned())
    .collect()
}

fn has(a: &[String], seq: &[&str]) -> bool {
    a.windows(seq.len())
        .any(|w| w.iter().zip(seq).all(|(x, y)| x == y))
}

fn server() -> Profile {
    builtin_profile("server").unwrap()
}

// Each hardening flag has a test that fails when the flag is dropped from
// `bwrap_args` (checked by mutation: delete the flag, the test fails).

#[test]
fn every_launch_unshares_everything_including_the_user_namespace() {
    let a = args_of(&server(), Some(Path::new("/w")));
    assert!(a.contains(&"--unshare-all".into()));
    assert!(a.contains(&"--unshare-user".into()));
}

#[test]
fn every_launch_disables_nested_user_namespaces() {
    assert!(args_of(&server(), None).contains(&"--disable-userns".into()));
}

#[test]
fn every_launch_drops_all_capabilities() {
    assert!(has(&args_of(&server(), None), &["--cap-drop", "ALL"]));
}

#[test]
fn every_launch_gets_a_new_session_and_dies_with_tole() {
    let a = args_of(&server(), None);
    assert!(a.contains(&"--new-session".into()));
    assert!(a.contains(&"--die-with-parent".into()));
}

#[test]
fn every_launch_has_a_private_proc_and_minimal_dev() {
    let a = args_of(&server(), None);
    assert!(has(&a, &["--proc", "/proc"]));
    assert!(has(&a, &["--dev", "/dev"]));
}

#[test]
fn the_system_is_read_only_and_home_is_not_mounted() {
    let a = args_of(&server(), None);
    assert!(has(&a, &["--ro-bind-try", "/usr", "/usr"]));
    assert!(
        !a.iter().any(|x| x == "--bind"),
        "no writable host bind by default"
    );
    assert!(has(&a, &["--tmpfs", "/scratch"]));
}

#[test]
fn network_none_shares_nothing_and_host_shares_the_net() {
    assert!(!args_of(&server(), None).contains(&"--share-net".into()));
    assert!(!args_of(&server(), None)
        .iter()
        .any(|x| x == "/etc/resolv.conf"));
    let mut p = server();
    p.network = Network::Host;
    let a = args_of(&p, None);
    assert!(a.contains(&"--share-net".into()));
    assert!(has(
        &a,
        &["--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"]
    ));
}

#[test]
fn only_home_and_tmpdir_use_setenv_and_never_a_secret() {
    let a = args_of(&server(), Some(Path::new("/w")));
    let names: Vec<_> = a
        .iter()
        .enumerate()
        .filter(|(_, x)| *x == "--setenv")
        .map(|(i, _)| (a[i + 1].clone(), a[i + 2].clone()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("HOME".to_string(), "/scratch".to_string()),
            ("TMPDIR".to_string(), "/scratch".to_string())
        ]
    );
}

#[cfg(unix)]
#[test]
fn workspace_policy_none_read_write() {
    let ws = Path::new("/w");
    let mut p = server();
    p.workspace = Workspace::None;
    let a = args_of(&p, Some(ws));
    assert!(!a.contains(&"/w".into()));
    assert!(has(&a, &["--chdir", "/scratch"]));

    p.workspace = Workspace::Read;
    let a = args_of(&p, Some(ws));
    assert!(has(&a, &["--ro-bind", "/w", "/w"]));
    assert!(has(&a, &["--chdir", "/w"]));

    p.workspace = Workspace::Write;
    let a = args_of(&p, Some(ws));
    assert!(has(&a, &["--bind", "/w", "/w"]));
    let w = a.iter().position(|x| x == "--bind").unwrap();
    for rel in ["/w/.git/hooks", "/w/.git/config", "/w/.tole"] {
        assert!(
            has(&a, &["--ro-bind-try", rel, rel]),
            "{rel} stays read-only"
        );
        // and comes after the writable bind, so it wins
        assert!(a.iter().rposition(|x| x == rel).unwrap() > w);
    }
}

#[test]
fn the_server_command_comes_last_after_a_separator() {
    let a = args_of(&server(), None);
    let n = a.len();
    assert_eq!(&a[n - 3..], ["--", "/usr/bin/true", "--flag"]);
}

#[test]
fn extra_read_and_write_paths_are_bound_as_named() {
    let mut p = server();
    p.read = vec!["/opt/r".into()];
    p.write = vec!["/opt/w".into()];
    let a = args_of(&p, None);
    assert!(has(&a, &["--ro-bind", "/opt/r", "/opt/r"]));
    assert!(has(&a, &["--bind", "/opt/w", "/opt/w"]));
}

/// A real tree: `home/o/{proj,.codecora/tole}`, a data dir outside the
/// home (`out/data/tole`, `out/other`) and a symlink to the home.
struct Tree {
    base: PathBuf,
    home: PathBuf,
    data: PathBuf,
}

fn tree(tag: &str) -> Tree {
    let base = std::env::temp_dir().join(format!("tole-sbx-unit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let base = {
        std::fs::create_dir_all(&base).unwrap();
        std::fs::canonicalize(&base).unwrap()
    };
    for d in [
        "home/o/proj",
        "home/o/.codecora/tole",
        "out/data/tole",
        "out/other",
    ] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(base.join("home/o"), base.join("lnk")).unwrap();
    Tree {
        home: base.join("home/o"),
        data: base.join("home/o/.codecora/tole"),
        base,
    }
}

fn refused(t: &Tree, p: &Path, data: &Path) -> bool {
    check_grant(p, Some(&t.home), Some(data)).is_err()
}

#[cfg(unix)]
#[test]
fn grants_of_root_proc_sys_dev_are_refused() {
    let t = tree("sys");
    for bad in ["/", "/proc", "/proc/1", "/sys", "/dev"] {
        assert!(refused(&t, Path::new(bad), &t.data), "{bad}");
    }
}

#[test]
fn home_and_every_ancestor_of_home_is_refused_but_a_project_under_it_is_not() {
    let t = tree("home");
    // The data dir is outside $HOME, so only the home rule can refuse these
    // (an equality-only home check let an ancestor of $HOME through).
    let d = t.base.join("out/data/tole");
    assert!(refused(&t, &t.home, &d), "$HOME itself");
    assert!(refused(&t, &t.base.join("home"), &d), "an ancestor: /home");
    assert!(refused(&t, &t.base, &d), "a higher ancestor");
    assert!(!refused(&t, &t.home.join("proj"), &d), "a project dir");
}

#[test]
fn an_ancestor_of_a_data_dir_outside_home_is_refused() {
    let t = tree("data");
    let data = t.base.join("out/data/tole");
    assert!(
        refused(&t, &t.base.join("out"), &data),
        "ancestor of the data dir"
    );
    assert!(refused(&t, &t.base.join("out/data"), &data), "its parent");
    assert!(refused(&t, &data, &data), "the data dir itself");
    assert!(!refused(&t, &t.base.join("out/other"), &data), "a sibling");
    // inside the home, the data dir and its inside are refused too
    assert!(refused(&t, &t.home.join(".codecora"), &t.data));
    assert!(refused(&t, &t.data, &t.data));
    // and anything inside it
    std::fs::create_dir_all(t.data.join("sessions")).unwrap();
    assert!(refused(&t, &t.data.join("sessions"), &t.data), "inside it");
}

#[cfg(unix)]
#[test]
fn a_symlink_to_home_is_resolved_and_refused() {
    let t = tree("link");
    assert!(
        refused(&t, &t.base.join("lnk"), &t.data),
        "symlink to $HOME"
    );
    assert!(
        !refused(&t, &t.base.join("lnk/proj"), &t.data),
        "a project through it"
    );
}

#[test]
fn dotdot_and_trailing_slash_cannot_hide_a_home_grant() {
    let t = tree("dots");
    let up = t.home.join("proj/..");
    assert!(refused(&t, &up, &t.data), "proj/.. is $HOME");
    let up2 = t.home.join("proj/../..");
    assert!(refused(&t, &up2, &t.data), "proj/../.. is /home");
    let slash = PathBuf::from(format!("{}/", t.home.display()));
    assert!(refused(&t, &slash, &t.data), "trailing slash");
    assert!(
        !refused(&t, &t.home.join("proj/"), &t.data),
        "proj/ is fine"
    );
}

#[test]
fn a_grant_that_cannot_be_resolved_is_refused() {
    let t = tree("gone");
    assert!(refused(&t, &t.home.join("proj/nope"), &t.data));
}

#[cfg(unix)]
#[test]
fn with_no_home_an_ancestor_of_home_or_root_is_refused() {
    let t = tree("nohome");
    for p in ["/", "/home"] {
        assert!(check_grant(Path::new(p), None, None).is_err(), "{p}");
    }
    if Path::new("/root").exists() {
        assert!(check_grant(Path::new("/root"), None, None).is_err());
    }
    assert!(check_grant(&t.home.join("proj"), None, None).is_ok());
}

#[test]
fn version_parsing_and_which() {
    assert_eq!(
        parse_version("bubblewrap 0.9.0\n"),
        Some((0, 9, "0.9.0".into()))
    );
    assert_eq!(parse_version("nonsense"), None);
    assert_eq!(which("/x/y", None), Some("/x/y".into()));
    assert_eq!(which("nope-not-a-program", Some("/usr/bin:/bin")), None);
}

fn policy(mode: Mode) -> Policy {
    Policy {
        mode,
        profile_name: "server".into(),
        profile: server(),
        skip: vec!["cora".into()],
    }
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

fn run(p: Option<&Policy>, a: &Availability, unattended: bool, name: &str) -> Launch {
    let env = Env {
        path: Some("/usr/bin:/bin".into()),
        home: Some("/home/o".into()),
        data_dir: Some("/home/o/.codecora/tole".into()),
        workspace: Some("/tmp".into()),
    };
    plan(p, a, unattended, &env, (name, "sh", &[]))
}

#[test]
fn off_or_unconfigured_is_the_unchanged_direct_path() {
    let direct = Launch::Direct { warning: None };
    assert_eq!(run(None, &usable(), false, "s"), direct);
    assert_eq!(run(Some(&policy(Mode::Off)), &usable(), false, "s"), direct);
    assert_eq!(run(Some(&policy(Mode::Off)), &gone(), true, "s"), direct);
}

#[test]
fn a_skipped_server_is_never_wrapped() {
    assert_eq!(
        run(Some(&policy(Mode::Required)), &gone(), true, "cora"),
        Launch::Direct { warning: None }
    );
}

#[cfg(unix)]
#[test]
fn a_usable_backend_wraps() {
    match run(Some(&policy(Mode::Required)), &usable(), false, "s") {
        Launch::Sandboxed { program, args } => {
            assert_eq!(program, Path::new("/usr/bin/bwrap"));
            assert!(args.iter().any(|a| a == "--disable-userns"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn required_without_a_backend_refuses() {
    assert!(matches!(
        run(Some(&policy(Mode::Required)), &gone(), false, "s"),
        Launch::Refused(_)
    ));
}

#[test]
fn optional_without_a_backend_runs_with_a_warning_only_when_attended() {
    let p = policy(Mode::Optional);
    assert!(matches!(
        run(Some(&p), &gone(), false, "s"),
        Launch::Direct { warning: Some(_) }
    ));
    // owner decision 2: unattended treats optional as required
    assert!(matches!(
        run(Some(&p), &gone(), true, "s"),
        Launch::Refused(_)
    ));
}

#[cfg(unix)]
fn env_for(t: &Tree, ws: &Path) -> Env {
    Env {
        path: Some("/usr/bin:/bin".into()),
        home: None,
        data_dir: Some(t.data.clone()),
        workspace: Some(ws.to_path_buf()),
    }
}

#[cfg(unix)]
#[test]
fn a_workspace_that_holds_the_data_dir_refuses_instead_of_exposing_it() {
    let t = tree("ws");
    let p = policy(Mode::Required);
    let l = plan(
        Some(&p),
        &usable(),
        false,
        &env_for(&t, &t.home),
        ("s", "sh", &[]),
    );
    assert!(
        matches!(l, Launch::Refused(ref m) if m.contains("data dir")),
        "{l:?}"
    );
    let l = plan(
        Some(&p),
        &usable(),
        false,
        &env_for(&t, &t.home.join("proj")),
        ("s", "sh", &[]),
    );
    assert!(
        matches!(l, Launch::Sandboxed { .. }),
        "a project dir is fine: {l:?}"
    );
}

/// The workspace is an argument of each launch, never shared state: two
/// sessions planning at once each get their own bind, and planning the
/// second never changes what the first gets.
#[cfg(unix)]
#[test]
fn each_launch_binds_the_workspace_it_was_given_even_concurrently() {
    let t = std::sync::Arc::new(tree("conc"));
    let wss: Vec<PathBuf> = (0..6)
        .map(|i| {
            let d = t.home.join(format!("proj/s{i}"));
            std::fs::create_dir_all(&d).unwrap();
            d
        })
        .collect();
    let args_of = |t: &Tree, ws: &Path| match plan(
        Some(&policy(Mode::Required)),
        &usable(),
        false,
        &env_for(t, ws),
        ("s", "sh", &[]),
    ) {
        Launch::Sandboxed { args, .. } => args,
        other => panic!("{other:?}"),
    };
    let first = args_of(&t, &wss[0]);
    let _ = args_of(&t, &wss[1]);
    assert_eq!(first, args_of(&t, &wss[0]), "another launch changed it");
    let handles: Vec<_> = wss
        .iter()
        .cloned()
        .map(|ws| {
            let t = std::sync::Arc::clone(&t);
            let wss = wss.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let a = match plan(
                        Some(&policy(Mode::Required)),
                        &usable(),
                        false,
                        &env_for(&t, &ws),
                        ("s", "sh", &[]),
                    ) {
                        Launch::Sandboxed { args, .. } => args,
                        other => panic!("{other:?}"),
                    };
                    assert!(a.iter().any(|x| x == ws.as_os_str()), "own workspace");
                    for o in wss.iter().filter(|o| **o != ws) {
                        assert!(!a.iter().any(|x| x == o.as_os_str()), "another's workspace");
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn doctor_says_what_is_there_and_that_off_is_off() {
    let lines = doctor_lines(None, &gone());
    assert!(lines[0].contains("unavailable") && lines[1].contains("off"));
    let lines = doctor_lines(Some(&policy(Mode::Required)), &usable());
    assert!(lines[0].contains("bubblewrap 0.9.0 usable"));
    assert!(lines[1].contains("mode required") && lines[1].contains("network none"));
}

/// The bind source is the resolved path that was validated, never the
/// original: a link swapped in after the check cannot redirect it (review).
#[cfg(unix)]
#[test]
fn binds_use_the_resolved_path_not_the_link() {
    let t = tree("resolved");
    let real = t.home.join("proj");
    let link = t.base.join("link-to-proj");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut p = policy(Mode::Required);
    p.profile.read = vec![link.clone()];
    p.profile.write = vec![link.clone()];
    let l = plan(
        Some(&p),
        &usable(),
        false,
        &env_for(&t, &link),
        ("s", "sh", &[]),
    );
    let Launch::Sandboxed { args, .. } = l else {
        panic!("{l:?}")
    };
    assert!(
        args.iter().any(|a| a == real.as_os_str()),
        "binds the target"
    );
    assert!(
        !args.iter().any(|a| a == link.as_os_str()),
        "never the link itself"
    );
}

/// Property over a temp tree with links, `..` and trailing slashes: every
/// path in the argv under the tree is the resolved path check_grant
/// accepted, never the spelling that was given.
#[cfg(unix)]
#[test]
fn the_argv_never_holds_a_path_other_than_the_validated_one() {
    let t = tree("prop");
    let real = t.home.join("proj");
    let link = t.base.join("l1");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let spellings = [
        link.clone(),
        PathBuf::from(format!("{}/", link.display())),
        real.join("../proj"),
        t.base.join("l1/../proj"),
        real.clone(),
    ];
    let root = t.base.to_str().unwrap().to_string();
    for given in &spellings {
        let mut p = policy(Mode::Required);
        p.profile.read = vec![given.clone()];
        p.profile.write = vec![given.clone()];
        let l = plan(
            Some(&p),
            &usable(),
            false,
            &env_for(&t, given),
            ("s", "sh", &[]),
        );
        let Launch::Sandboxed { args, .. } = l else {
            panic!("{given:?}: {l:?}")
        };
        let resolved = std::fs::canonicalize(given).unwrap();
        for a in args
            .iter()
            .filter(|a| a.to_string_lossy().starts_with(&root))
        {
            assert_eq!(Path::new(a), resolved, "{given:?} leaked as {a:?}");
        }
    }
}

/// Every launch resolves again: a link swapped after an earlier plan is
/// followed to its NEW target (and refused when that is $HOME), never to a
/// cached one.
#[cfg(unix)]
#[test]
fn a_link_swapped_between_launches_is_resolved_again_or_refused() {
    let t = tree("swap");
    let (a, b) = (t.home.join("proj"), t.base.join("out/other"));
    let link = t.base.join("swap-link");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    let go = |ws: &Path| {
        plan(
            Some(&policy(Mode::Required)),
            &usable(),
            false,
            &env_for(&t, ws),
            ("s", "sh", &[]),
        )
    };
    let has = |l: &Launch, p: &Path| match l {
        Launch::Sandboxed { args, .. } => args.iter().any(|x| x == p.as_os_str()),
        _ => false,
    };
    assert!(has(&go(&link), &a));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&b, &link).unwrap();
    let l = go(&link);
    assert!(has(&l, &b) && !has(&l, &a), "followed the new target");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&t.home, &link).unwrap();
    assert!(
        matches!(go(&link), Launch::Refused(_)),
        "a link swapped to $HOME is refused"
    );
}

/// The server's install dir is bound by its resolved path too, and the
/// program is addressed through it.
#[cfg(unix)]
#[test]
fn the_program_prefix_is_bound_resolved_and_the_program_runs_through_it() {
    let t = tree("prefix");
    let bin = t.base.join("out/other/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("srv"), "#!/bin/sh\n").unwrap();
    let link = t.base.join("binlink");
    std::os::unix::fs::symlink(&bin, &link).unwrap();
    let env = Env {
        path: Some(link.display().to_string()),
        ..env_for(&t, &t.home.join("proj"))
    };
    let l = plan(
        Some(&policy(Mode::Required)),
        &usable(),
        false,
        &env,
        ("s", "srv", &[]),
    );
    let Launch::Sandboxed { args, .. } = l else {
        panic!("{l:?}")
    };
    assert!(
        args.iter().any(|a| a == bin.as_os_str()),
        "binds the target"
    );
    assert!(args.iter().any(|a| a == bin.join("srv").as_os_str()));
    assert!(
        !args.iter().any(|a| a == link.as_os_str()),
        "never the link"
    );
    assert!(!args.iter().any(|a| a == link.join("srv").as_os_str()));
}
