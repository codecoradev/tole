//! #867: `--no-config` and the #865 loosening path need a TERMINAL (stdin and
//! stderr) when the USER config sets `[sandbox]` or `[secrets]`, marker or not
//! (a script that strips `TOLE_UNDER_SESSION` has no terminal). When the user
//! config sets neither, owner scripts and CI work exactly as before. Real
//! binary, scrubbed environment. The attended owner is a real pty (python3).
//!
//! Not a boundary: a deliberate pty gets past it, and the sandbox is the
//! boundary (docs, threat model); these tests pin what the rule does.
#![cfg(unix)]

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

const SANDBOX: &str = "[sandbox]\nmode = \"required\"\n";
const CONSUMERS: &str = "[sandbox.consumers]\nrun_command = \"required\"\n";
const SECRETS: &str = "[secrets]\nGITHUB_TOKEN = { tools = [\"gh\"] }\n";

fn scrubbed(root: &Path, config: Option<&str>, env: &[(&str, &str)]) -> Command {
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let cfg = home.join("tole/config.toml");
    let _ = std::fs::remove_file(&cfg);
    if let Some(c) = config {
        std::fs::write(&cfg, c).unwrap();
    }
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.env_clear();
    common::apply_hermetic_env(&mut c, &home, &[("PATH", "/usr/bin:/bin")]);
    c.current_dir(&work)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// `tole <args>` with no terminal.
fn tole(root: &Path, config: Option<&str>, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut c = scrubbed(root, config, env);
    c.args(args);
    c.spawn().unwrap().wait_with_output().unwrap()
}

/// `tole <args>` on a real pty (stdin and stderr are terminals), or `None`
/// when python3 is not there.
fn tole_on_a_pty(
    root: &Path,
    config: Option<&str>,
    env: &[(&str, &str)],
    args: &[&str],
) -> Option<(i32, String)> {
    let mut c = scrubbed(root, config, env);
    // The child is the python helper, which runs tole on a pty.
    let py = Command::new("python3").arg("--version").output().ok()?;
    if !py.status.success() {
        return None;
    }
    let mut d = Command::new("python3");
    d.args([
        "-c",
        "import os,pty,sys\nst=pty.spawn(sys.argv[1:])\nsys.exit(os.WEXITSTATUS(st))",
        env!("CARGO_BIN_EXE_tole"),
    ])
    .args(args);
    // Same scrubbed environment and cwd as `scrubbed`.
    d.current_dir(c.get_current_dir().unwrap())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in c.get_envs() {
        if let Some(v) = v {
            d.env(k, v);
        }
    }
    let _ = &mut c;
    let o = d.output().ok()?;
    Some((
        o.status.code().unwrap_or(-1),
        format!("{}{}", text(&o.stdout), text(&o.stderr)),
    ))
}

#[track_caller]
fn assert_refused_867(o: &Output, needles: &[&str]) {
    let err = text(&o.stderr);
    assert!(!o.status.success(), "must not run: {err}");
    assert!(err.trim_end().ends_with("(issue #867)"), "{err}");
    for n in needles {
        assert!(err.contains(n), "{n:?} not in {err}");
    }
}

#[test]
fn no_config_without_a_terminal_is_refused_when_the_user_config_protects_something() {
    let root = TestDir::new("tole-term-noconf");
    for (cfg, key) in [
        (SANDBOX, "[sandbox]"),
        (CONSUMERS, "[sandbox]"),
        (SECRETS, "[secrets]"),
        (
            "[sandbox]\nmode = \"required\"\n[secrets]\nGITHUB_TOKEN = { tools = [\"gh\"] }\n",
            "[sandbox] and [secrets]",
        ),
    ] {
        // No marker at all: a script that stripped it.
        let o = tole(&root, Some(cfg), &[], &["--no-config", "sessions"]);
        assert_refused_867(
            &o,
            &[
                "--no-config needs a terminal",
                key,
                "run it yourself from a terminal",
            ],
        );
        // The refusal comes before the command: no listing.
        assert!(!text(&o.stdout).contains("no sessions"));
    }
}

#[test]
fn a_user_config_that_cannot_be_read_keeps_the_strict_rule() {
    let root = TestDir::new("tole-term-bad");
    let o = tole(
        &root,
        Some("this is [not toml\n"),
        &[],
        &["--no-config", "sessions"],
    );
    assert_refused_867(
        &o,
        &["--no-config needs a terminal", "cannot be read or parsed"],
    );
    // A path that cannot be inspected (CODECORA_HOME is a file).
    let file = root.join("a-file");
    std::fs::write(&file, "x").unwrap();
    let o = tole(
        &root,
        None,
        &[("CODECORA_HOME", file.to_str().unwrap())],
        &["--no-config", "sessions"],
    );
    assert_refused_867(&o, &["cannot be inspected"]);
}

#[test]
fn no_config_is_unchanged_when_the_user_config_protects_nothing() {
    let root = TestDir::new("tole-term-plain");
    for cfg in [
        None,
        Some("model = \"m\"\n"),
        Some("allow = [\"read_*\"]\n"),
    ] {
        let o = tole(&root, cfg, &[], &["--no-config", "sessions"]);
        let err = text(&o.stderr);
        assert!(o.status.success(), "{cfg:?}: {err}");
        assert!(!err.contains("#867"), "{cfg:?}: {err}");
    }
    // Under the session marker the #855 rule is exactly as before (refused
    // whatever the config says).
    let o = tole(
        &root,
        None,
        &[("TOLE_UNDER_SESSION", "1")],
        &["--no-config", "sessions"],
    );
    let err = text(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(
        err.contains("refused under an agent session") && err.contains("#855"),
        "{err}"
    );
}

#[test]
fn an_owner_on_a_terminal_keeps_no_config_and_the_flags() {
    let root = TestDir::new("tole-term-pty");
    let cfg = format!("allow = [\"read_*\"]\n{SANDBOX}");
    let Some((code, out)) = tole_on_a_pty(&root, Some(&cfg), &[], &["--no-config", "sessions"])
    else {
        eprintln!("SKIPPED: no python3 for a pty here");
        return;
    };
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("#867"), "{out}");
    // A wider --allow on a terminal: not refused by the #865 rule either.
    let (_, out) = tole_on_a_pty(
        &root,
        Some(&cfg),
        &[],
        &["run", "--allow", "edit_*", "--", "hi"],
    )
    .unwrap();
    assert!(!out.contains("#865"), "{out}");
}

#[test]
fn the_loosening_path_needs_a_terminal_when_the_user_config_protects_something() {
    let root = TestDir::new("tole-term-narrow");
    let cfg = format!("allow = [\"read_*\"]\n{SANDBOX}");
    // No marker, no terminal, a wider allow: refused (the #865 text).
    let o = tole(
        &root,
        Some(&cfg),
        &[],
        &["run", "--allow", "edit_*", "--", "hi"],
    );
    let err = text(&o.stderr);
    assert!(
        !o.status.success() && err.contains("(issue #865)") && err.contains("edit_*"),
        "{err}"
    );
    // A narrower or equal allow passes.
    let o = tole(
        &root,
        Some(&cfg),
        &[],
        &["run", "--allow", "read_*", "--", "hi"],
    );
    assert!(!text(&o.stderr).contains("#865"), "{}", text(&o.stderr));
    // A user config that protects nothing: unchanged, a wider allow is fine.
    let o = tole(
        &root,
        Some("allow = [\"read_*\"]\n"),
        &[],
        &["run", "--allow", "edit_*", "--", "hi"],
    );
    assert!(!text(&o.stderr).contains("#865"), "{}", text(&o.stderr));
    // A child tole itself started (child variables, no terminal, no marker).
    let o = tole(
        &root,
        Some(&cfg),
        &[("TOLE_AGENT_DEPTH", "1")],
        &["run", "--allow", "edit_*", "--", "hi"],
    );
    assert!(!text(&o.stderr).contains("#865"), "{}", text(&o.stderr));
}

/// serve, acp and mcp: `--no-config` stays refused with the #855 text, and the
/// flags stay exempt, whatever the terminal says.
#[cfg(feature = "shell-tools")]
#[test]
fn the_faces_are_unchanged() {
    let root = TestDir::new("tole-term-face");
    let cfg = format!("allow = [\"read_*\"]\n{SANDBOX}");
    let o = tole(&root, Some(&cfg), &[], &["--no-config", "acp"]);
    let err = text(&o.stderr);
    assert!(
        !o.status.success() && err.contains("--no-config is refused for") && err.contains("#855"),
        "{err}"
    );
    assert!(!err.contains("#867"), "{err}");
    let o = tole(&root, Some(&cfg), &[], &["acp", "--allow", "edit_*"]);
    assert!(!text(&o.stderr).contains("#865"), "{}", text(&o.stderr));
}
