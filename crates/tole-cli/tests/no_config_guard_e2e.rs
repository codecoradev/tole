//! `--no-config` (#855) on the real binary: refused under an agent session
//! (`TOLE_UNDER_SESSION`, a child agent) and for the unattended faces, while
//! the same command without the marker still works. The test process has no
//! terminal and no pty, so the attended case without a marker is covered by
//! the unit tests of `no_config_guard` with a hand-built context; here only
//! the non-refusal of a plain run is checked.

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Command, Output};

const REQUIRED: &str = "[sandbox]\nmode = \"required\"\n";

fn setup(tag: &str) -> TestDir {
    let base = TestDir::new(&format!("tole-noconf-{tag}"));
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("proj")).unwrap();
    std::fs::write(base.join("home/tole/config.toml"), REQUIRED).unwrap();
    base
}

fn tole(base: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(base.join("proj"))
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_AGENT_DEPTH")
        .env_remove("TOLE_UNDER_SESSION")
        .stdin(std::process::Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn no_config_is_refused_under_an_agent_session_marker_or_child_depth() {
    let b = setup("sess");
    for env in [("TOLE_UNDER_SESSION", "1"), ("TOLE_AGENT_DEPTH", "1")] {
        let o = tole(&b, &[env], &["--no-config", "sessions"]);
        let out = text(&o);
        assert!(!o.status.success(), "{env:?}: {out}");
        assert!(
            out.contains("--no-config is refused under an agent session"),
            "{env:?}: {out}"
        );
        // The refusal comes before the command: no listing is printed.
        assert!(!out.contains("no sessions"), "{env:?}: {out}");
    }
}

#[test]
fn the_same_commands_work_without_the_flag_or_without_the_marker() {
    let b = setup("ok");
    // Under a session, WITHOUT the flag: unchanged.
    let o = tole(&b, &[("TOLE_UNDER_SESSION", "1")], &["sessions"]);
    assert!(!text(&o).contains("--no-config is refused"), "{}", text(&o));
    // No marker, with the flag, no terminal, and a user config that sets
    // `[sandbox]`: no longer refused as an agent session, but #867 asks for a
    // terminal then (the e2e in `terminal_guard_e2e` covers the rule; here the
    // marker-based text is only checked to be absent).
    let o = tole(&b, &[], &["--no-config", "sessions"]);
    assert!(!text(&o).contains("--no-config is refused"), "{}", text(&o));
    assert!(text(&o).contains("(issue #867)"), "{}", text(&o));
    // A user config that protects nothing: the flag works without a terminal.
    std::fs::write(b.join("home/tole/config.toml"), "model = \"m\"\n").unwrap();
    let o = tole(&b, &[], &["--no-config", "sessions"]);
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn the_flag_is_refused_before_a_session_command_reads_anything() {
    let b = setup("run");
    let o = tole(
        &b,
        &[("TOLE_UNDER_SESSION", "1")],
        &["--no-config", "run", "hi"],
    );
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("--no-config is refused"), "{out}");
}

#[cfg(feature = "shell-tools")]
#[test]
fn no_config_is_refused_for_serve_and_acp() {
    let b = setup("face");
    for face in ["serve", "acp"] {
        let o = tole(&b, &[], &["--no-config", face]);
        let out = text(&o);
        assert!(!o.status.success(), "{face}: {out}");
        assert!(out.contains("--no-config is refused for"), "{face}: {out}");
    }
}

#[cfg(all(feature = "shell-tools", feature = "mcp"))]
#[test]
fn no_config_is_refused_for_the_mcp_face() {
    let b = setup("mcp");
    let o = tole(&b, &[], &["--no-config", "mcp"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("--no-config is refused for"), "{out}");
}
