use super::*;
use crate::sandbox::command::Consumers;
use crate::sandbox::{builtin_profile, plan, Env};
use crate::test_dir::TestDir;
use std::path::PathBuf;

/// A fake `bwrap` in a TestDir: the decision never runs it, only needs a
/// usable backend value.
fn usable(dir: &TestDir) -> Availability {
    let bwrap = dir.join("bwrap");
    std::fs::write(&bwrap, "#!/bin/sh\n").unwrap();
    Availability::Usable {
        bwrap,
        version: "0.9.0".into(),
    }
}

fn gone() -> Availability {
    Availability::Unavailable("bubblewrap (bwrap) is not on PATH".into())
}

fn policy(mode: Mode) -> Policy {
    Policy {
        mode,
        profile_name: "server".into(),
        profile: builtin_profile("server").unwrap(),
        servers: Default::default(),
        skip: vec!["trusted".into()],
        max_servers: 16,
        idle_secs: 900,
    }
}

#[test]
fn the_three_decisions() {
    let d = TestDir::new("tole-rep-dec");
    assert_eq!(decide(Mode::Off, &gone(), false), None);
    assert_eq!(
        decide(Mode::Required, &usable(&d), false),
        Some(Decision::Sandboxed)
    );
    assert!(matches!(
        decide(Mode::Optional, &gone(), false),
        Some(Decision::Unsandboxed(_))
    ));
    assert!(matches!(
        decide(Mode::Optional, &gone(), true),
        Some(Decision::Refused(_))
    ));
    assert!(matches!(
        decide(Mode::Required, &gone(), false),
        Some(Decision::Refused(_))
    ));
}

#[test]
fn the_startup_line_follows_what_plan_decided() {
    let d = TestDir::new("tole-rep-plan");
    let ws = d.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let env = Env {
        path: Some("/usr/bin:/bin".into()),
        home: Some("/home/owner".into()),
        data_dir: Some("/home/owner/.codecora/tole".into()),
        workspace: Some(ws),
    };
    let srv = ("s", "sh", &[][..]);
    let run = |p: &Policy, a: &Availability, un: bool, name: &str| {
        let l = plan(Some(p), a, un, &env, (name, srv.1, srv.2));
        (startup_line(Some(p), name, &l), l)
    };
    let (line, l) = run(&policy(Mode::Required), &usable(&d), false, "s");
    assert!(matches!(l, Launch::Sandboxed { .. }));
    assert_eq!(
        line.unwrap(),
        "mcp[s]: sandboxed (profile server, mode required)"
    );
    let (line, l) = run(&policy(Mode::Optional), &gone(), false, "s");
    assert!(matches!(l, Launch::Direct { warning: Some(_) }));
    assert!(line.unwrap().contains("starting unsandboxed"));
    let (line, l) = run(&policy(Mode::Optional), &gone(), true, "s");
    assert!(matches!(l, Launch::Refused(_)));
    assert!(line.unwrap().contains("unattended"));
    let (line, _) = run(&policy(Mode::Required), &gone(), false, "s");
    assert!(line.unwrap().contains("sandbox required but unavailable"));
    let (line, _) = run(&policy(Mode::Required), &usable(&d), false, "trusted");
    assert!(line.unwrap().contains("skip"));
    assert_eq!(startup_line(None, "s", &l), None);
    assert_eq!(startup_line(Some(&policy(Mode::Off)), "s", &l), None);
}

fn home_tree(tag: &str) -> (TestDir, PathBuf) {
    let d = TestDir::new(tag);
    let home = d.join("home/o");
    for p in [".ssh", ".config/gcloud", "proj", ".aws/sub"] {
        std::fs::create_dir_all(home.join(p)).unwrap();
    }
    (d, home)
}

#[test]
fn sensitive_paths_are_named() {
    let (_d, home) = home_tree("tole-rep-sens");
    let k = |p: &str| sensitive_kind(&home.join(p), Some(&home));
    assert_eq!(k(".ssh"), Some("ssh keys"));
    assert_eq!(k(".aws/sub"), Some("aws credentials"));
    assert_eq!(k(".config/gcloud"), Some("gcloud credentials"));
    assert_eq!(k("proj"), None);
    assert_eq!(sensitive_kind(&home, Some(&home)), Some("holds $HOME"));
    assert_eq!(
        sensitive_kind(home.parent().unwrap(), Some(&home)),
        Some("holds $HOME")
    );
}

#[test]
fn doctor_lists_grants_and_marks_the_sensitive_ones() {
    let (d, home) = home_tree("tole-rep-doc");
    let data = home.join(".codecora/tole");
    let mut p = policy(Mode::Required);
    p.profile.read = vec![home.join("proj"), home.join(".ssh")];
    p.profile.write = vec![home.clone()];
    let lines = doctor_grant_lines(
        Some(&p),
        &Consumers::default(),
        &usable(&d),
        false,
        &["files".into(), "trusted".into()],
        Some(&home),
        Some(&data),
    );
    let text = lines.join("\n");
    assert!(text.contains("sandbox mcp[files]: mode required, workspace read-only, network none"));
    assert!(text.contains("read ") && text.contains("proj\n"), "{text}");
    assert!(text.contains(".ssh SENSITIVE (ssh keys)"), "{text}");
    assert!(text.contains("REFUSED"), "home is refused: {text}");
    assert!(text.contains("sandbox mcp[files]: sandboxed"));
    assert!(text.contains("mcp[trusted]: unsandboxed (listed in [sandbox] skip)"));
}

#[test]
fn doctor_lists_the_grants_of_a_bound_servers_own_profile() {
    let (d, home) = home_tree("tole-rep-bound");
    let mut open = builtin_profile("server").unwrap();
    open.network = crate::sandbox::Network::Host;
    open.read = vec![home.join("proj")];
    let mut p = policy(Mode::Required);
    p.servers.insert(
        "gh".into(),
        crate::sandbox::ServerProfile {
            name: "open".into(),
            profile: open,
        },
    );
    let text = doctor_grant_lines(
        Some(&p),
        &Consumers::default(),
        &usable(&d),
        false,
        &["gh".into(), "files".into()],
        Some(&home),
        None,
    )
    .join("\n");
    assert!(
        text.contains("sandbox mcp[gh]: mode required, workspace read-only, network host"),
        "{text}"
    );
    assert!(
        text.contains("sandbox mcp[files]: mode required, workspace read-only, network none"),
        "{text}"
    );
    assert!(
        text.matches("proj\n").count() == 1,
        "only gh has the read grant: {text}"
    );
}

#[test]
fn doctor_lists_consumers_with_decision_and_nothing_for_off() {
    let d = TestDir::new("tole-rep-cons");
    let c = Consumers {
        run_command: Mode::Required,
        network: Network::Host,
        ..Consumers::default()
    };
    let lines = doctor_grant_lines(None, &c, &gone(), false, &[], None, None);
    let text = lines.join("\n");
    assert!(text
        .contains("sandbox consumer run_command: mode required, workspace writable, network host"));
    assert!(text.contains(".git/hooks, .git/config, .tole"));
    assert!(text.contains("run_command: refused, no sandbox"));
    assert!(!text.contains("job_start"));
    let none = doctor_grant_lines(
        None,
        &Consumers::default(),
        &usable(&d),
        false,
        &[],
        None,
        None,
    );
    assert!(none.is_empty());
}
