use super::*;
use crate::sandbox::command::Consumers;
use crate::sandbox::{builtin_profile, plan, Env};
use crate::test_dir::TestDir;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

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
    let hash = builtin_profile("server").unwrap().hash();
    assert_eq!(
        line.unwrap(),
        format!(
            "mcp[s]: sandboxed (profile server {}, mode required)",
            short_hash(&hash)
        )
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
    assert!(text.contains(".git, .tole"));
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

#[test]
fn the_short_hash_is_a_prefix_of_the_full_one() {
    let full = builtin_profile("server").unwrap().hash();
    let short = short_hash(&full);
    assert_eq!(short.len(), "sha256:".len() + 12, "{short}");
    assert!(full.starts_with(&short));
    // A value that is not a hash is still cut, never panics.
    assert_eq!(short_hash("abc"), "sha256:abc");
}

#[test]
fn the_startup_line_names_the_profile_and_hash_the_server_runs_in() {
    let d = TestDir::new("tole-rep-own");
    let mut open = builtin_profile("server").unwrap();
    open.network = Network::Host;
    let mut p = policy(Mode::Required);
    p.servers.insert(
        "gh".into(),
        crate::sandbox::ServerProfile {
            name: "open".into(),
            profile: open.clone(),
        },
    );
    let sandboxed = Launch::Sandboxed {
        program: "/usr/bin/bwrap".into(),
        args: vec![],
    };
    let _ = usable(&d);
    let own = startup_line(Some(&p), "gh", &sandboxed).unwrap();
    assert_eq!(
        own,
        format!(
            "mcp[gh]: sandboxed (profile open {}, mode required)",
            short_hash(&open.hash())
        )
    );
    let dflt = startup_line(Some(&p), "files", &sandboxed).unwrap();
    assert!(dflt.contains("profile server sha256:"), "{dflt}");
    assert_ne!(own.split("profile ").nth(1), dflt.split("profile ").nth(1));
}

#[test]
fn the_startup_line_is_one_line_whatever_the_names_hold() {
    let mut p = policy(Mode::Required);
    p.profile_name = "evil\n\x1b[31mprofile".into();
    let sandboxed = Launch::Sandboxed {
        program: "/usr/bin/bwrap".into(),
        args: vec![],
    };
    let line = startup_line(Some(&p), "a\nb\x1b]0;x\x07", &sandboxed).unwrap();
    assert!(!line.contains(['\n', '\r', '\x1b', '\x07']), "{line:?}");
    assert!(
        line.contains("\\u{a}") && line.contains("\\u{1b}"),
        "{line}"
    );
    // The refused and unsandboxed texts carry the same names and are cut
    // the same way.
    let refused = Launch::Refused("mcp[a\nb]: not started\x1b[2J".into());
    let line = startup_line(Some(&p), "a", &refused).unwrap();
    assert!(!line.contains(['\n', '\x1b']), "{line:?}");
    let warned = Launch::Direct {
        warning: Some("mcp[x]: no sandbox (why\r\nnext line)".into()),
    };
    let line = startup_line(Some(&p), "x", &warned).unwrap();
    assert!(!line.contains(['\r', '\n']), "{line:?}");
}

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

#[test]
fn the_verbose_argv_hides_secret_flags_and_known_values() {
    let set = crate::secrets::SecretSet::default().with("MY_TOKEN", "hunter2-secret-value");
    let shown = redact_argv_with(
        &os(&[
            "/usr/bin/bwrap",
            "--bind",
            "/work",
            "/work",
            "--",
            "server",
            "--token",
            "tok-123456789",
            "--api-key=key-123456789",
            "--password",
            "pw-123456789",
            "--name",
            "plain",
            "carries-hunter2-secret-value-inside",
        ]),
        &set,
    );
    for leaked in [
        "tok-123456789",
        "key-123456789",
        "pw-123456789",
        "hunter2-secret-value",
    ] {
        assert!(!shown.contains(leaked), "{leaked} leaked: {shown}");
    }
    // What is not secret stays readable.
    assert!(shown.starts_with("/usr/bin/bwrap --bind /work /work -- server --token <redacted>"));
    assert!(shown.contains("--api-key=<redacted>") && shown.contains("--name plain"));
    // A flag that merely ends the list does not hide anything after it.
    assert_eq!(redact_argv_with(&os(&["--token"]), &set), "--token");
}

#[test]
fn the_verbose_argv_hides_the_value_of_a_secret_named_assignment() {
    let set = crate::secrets::SecretSet::default();
    let shown = redact_argv_with(
        &os(&[
            "API_KEY=xyz-positional",
            "-e",
            "GITHUB_TOKEN=xyz-after-e",
            "--env=DB_PASSWORD=xyz-nested",
            "--setenv",
            "my-api-key=xyz-dashed",
            "PATH=/usr/bin",
            "-e",
            "LANG=C",
            "--env=HOME=/home/o",
            "--name=plain",
            "a=b=c",
        ]),
        &set,
    );
    for leaked in ["xyz-positional", "xyz-after-e", "xyz-nested", "xyz-dashed"] {
        assert!(!shown.contains(leaked), "{leaked} leaked: {shown}");
    }
    assert_eq!(
        shown,
        "API_KEY=<redacted> -e GITHUB_TOKEN=<redacted> --env=DB_PASSWORD=<redacted> \
         --setenv my-api-key=<redacted> PATH=/usr/bin -e LANG=C --env=HOME=/home/o \
         --name=plain a=b=c"
    );
}

#[test]
fn the_verbose_argv_is_one_line_and_quotes_what_needs_it() {
    let set = crate::secrets::SecretSet::default();
    let shown = redact_argv_with(&os(&["a b", "", "it's", "x\ny\x1b[31m", "plain"]), &set);
    assert!(!shown.contains(['\n', '\x1b']), "{shown:?}");
    assert_eq!(shown, "'a b' '' 'it'\\''s' x\\u{a}y\\u{1b}[31m plain");
}

#[test]
fn only_the_documented_values_turn_verbose_on() {
    for on in ["1", "true", "yes", " 1 "] {
        assert!(verbose_value(on), "{on}");
    }
    for off in ["", "0", "false", "no", "on", "2"] {
        assert!(!verbose_value(off), "{off}");
    }
    assert_eq!(VERBOSE_ENV, "TOLE_VERBOSE");
}

#[test]
fn credential_directories_are_marked_sensitive() {
    let d = TestDir::new("tole-rep-creds");
    let home = d.join("home/o");
    let cases = [
        (".ssh", "ssh keys"),
        (".gnupg/private-keys-v1.d", "gpg keys"),
        (".aws", "aws credentials"),
        (".azure", "azure credentials"),
        (".kube", "kube credentials"),
        (".docker", "docker credentials"),
        (".netrc", "netrc credentials"),
        (".config/gcloud", "gcloud credentials"),
        (".config/gh", "github cli credentials"),
        (".config/gh/hosts.yml", "github cli credentials"),
        (".config/glab-cli", "gitlab cli credentials"),
        (".config/op", "1password cli credentials"),
        (".password-store", "password store"),
        (".npmrc", "npm credentials"),
        (".pypirc", "pypi credentials"),
        (".cargo/credentials", "cargo credentials"),
        (".cargo/credentials.toml", "cargo credentials"),
    ];
    for (rel, what) in cases {
        assert_eq!(
            sensitive_kind(&home.join(rel), Some(&home)),
            Some(what),
            "{rel}"
        );
    }
    // Neighbours that are not credentials stay unmarked: a sibling under
    // `.config`, the cargo registry cache, an unrelated name.
    for rel in [
        ".config",
        ".config/ghost",
        ".config/helix",
        ".cargo",
        ".cargo/registry",
        ".cargo/bin",
        "gh",
        "proj/.npmrc.d",
    ] {
        assert_eq!(sensitive_kind(&home.join(rel), Some(&home)), None, "{rel}");
    }
}

#[test]
fn the_verbose_line_names_the_launch_and_carries_the_redacted_argv() {
    let set = crate::secrets::SecretSet::default();
    let line = launch_argv_line(
        "mcp[a\nb]",
        Path::new("/usr/bin/bwrap"),
        &os(&["--unshare-all", "--", "srv", "--token=abcdefghij"]),
        &set,
    );
    assert_eq!(
        line,
        "mcp[a\\u{a}b]: sandbox command line: /usr/bin/bwrap --unshare-all -- srv --token=<redacted>"
    );
}
