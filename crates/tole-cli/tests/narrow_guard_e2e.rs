//! #865: under an agent session (`TOLE_UNDER_SESSION`) a flag
//! or `TOLE_TRUST` may only NARROW the user config's policy lists. Real binary,
//! scrubbed environment, no provider: a run that gets past the guard stops at
//! the missing provider, so "not refused" is "the guard's text is absent".
//!
//! Keyed on the same environment marker as the other #684 guards, so it is not
//! a boundary (docs, threat model): these tests pin what it does, not more.
#![cfg(all(unix, feature = "shell-tools"))]

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

/// `tole <args>` with `config` as the user config and `env` on top of a
/// scrubbed environment.
fn tole(root: &Path, config: Option<&str>, env: &[(&str, &str)], args: &[&str]) -> Output {
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let cfg = home.join("tole/config.toml");
    let _ = std::fs::remove_file(&cfg);
    if let Some(c) = config {
        std::fs::write(&cfg, c).unwrap();
    }
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(&work)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", &home)
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    c.spawn().unwrap().wait_with_output().unwrap()
}

const SESSION: &[(&str, &str)] = &[("TOLE_UNDER_SESSION", "1")];

fn refused(o: &Output) -> bool {
    text(&o.stderr).contains("(issue #865)")
}

#[track_caller]
fn assert_refused(o: &Output, needles: &[&str]) {
    let err = text(&o.stderr);
    assert!(!o.status.success(), "must not run: {err}");
    assert!(err.contains("(issue #865)"), "{err}");
    for n in needles {
        assert!(err.contains(n), "{n:?} not in {err}");
    }
}

const USER: &str = "allow = [\"read_*\", \"write_*\", \"!write_secret\"]\n\
                    trust = [\"read_only\"]\n\
                    on_pretool = [\"a.sh\", \"b.sh\"]\n\
                    on_turnend = [\"gate.sh\"]\n";

#[test]
fn a_session_process_cannot_widen_allow_or_drop_a_deny() {
    let root = TestDir::new("tole-narrow-allow");
    let r = |args: &[&str]| tole(&root, Some(USER), SESSION, args);
    let o = r(&["run", "--allow", "edit_*", "--", "hi"]);
    assert_refused(&o, &["--allow", "edit_*", "read_*, write_*", "allow"]);
    // Replacing the list without the owner's deny drops it.
    let o = r(&["run", "--allow", "read_*", "--", "hi"]);
    assert_refused(&o, &["drops its deny", "!write_secret"]);
    // The way out is named.
    assert!(text(&o.stderr).contains("change the user config yourself"));
}

#[test]
fn a_session_process_cannot_widen_trust_by_flag_or_environment() {
    let root = TestDir::new("tole-narrow-trust");
    let o = tole(
        &root,
        Some(USER),
        SESSION,
        &["--trust", "workspace", "run", "--", "hi"],
    );
    assert_refused(&o, &["workspace", "[read_only]", "trust"]);
    let mut env = SESSION.to_vec();
    env.push(("TOLE_TRUST", "workspace"));
    let o = tole(&root, Some(USER), &env, &["run", "--", "hi"]);
    assert_refused(&o, &["TOLE_TRUST", "workspace"]);
}

#[test]
fn a_session_process_cannot_drop_a_hook() {
    let root = TestDir::new("tole-narrow-hooks");
    let o = tole(
        &root,
        Some(USER),
        SESSION,
        &["run", "--on-pretool", "a.sh", "--", "hi"],
    );
    assert_refused(&o, &["--on-pretool", "b.sh", "[a.sh, b.sh]"]);
    let o = tole(
        &root,
        Some(USER),
        SESSION,
        &["run", "--on-turnend", "other.sh", "--", "hi"],
    );
    assert_refused(&o, &["--on-turnend", "gate.sh"]);
}

#[test]
fn a_narrower_input_a_superset_of_hooks_and_a_key_the_user_does_not_set_pass() {
    let root = TestDir::new("tole-narrow-pass");
    let ok = |cfg: Option<&str>, args: &[&str]| {
        let o = tole(&root, cfg, SESSION, args);
        assert!(!refused(&o), "{args:?}: {}", text(&o.stderr));
    };
    // Narrower allow (keeping the deny, adding one), equal trust, more hooks.
    ok(
        Some(USER),
        &[
            "run",
            "--allow",
            "read_*",
            "--allow",
            "!write_secret",
            "--allow",
            "!x",
            "--",
            "hi",
        ],
    );
    ok(Some(USER), &["--trust", "read_only", "run", "--", "hi"]);
    ok(
        Some(USER),
        &[
            "run",
            "--on-pretool",
            "a.sh",
            "--on-pretool",
            "b.sh",
            "--on-pretool",
            "c.sh",
            "--",
            "hi",
        ],
    );
    // Nothing given.
    ok(Some(USER), &["run", "--", "hi"]);
    // The user config does not set the keys: nothing to narrow.
    ok(
        Some("model = \"m\"\n"),
        &[
            "run",
            "--allow",
            "edit_*",
            "--on-turnend",
            "x.sh",
            "--",
            "hi",
        ],
    );
    ok(None, &["run", "--allow", "edit_*", "--", "hi"]);
}

#[test]
fn the_exemptions_an_attended_owner_and_a_child_tole_started() {
    let root = TestDir::new("tole-narrow-exempt");
    let wide = ["run", "--allow", "edit_*", "--", "hi"];
    // An attended owner: no marker at all.
    let o = tole(&root, Some(USER), &[], &wide);
    assert!(!refused(&o), "{}", text(&o.stderr));
    // A child tole itself started (agent_start, a delegated child) carries the
    // child variables but NOT `TOLE_UNDER_SESSION`, which only `run_command`
    // and `job_start` set: its scope is enforced in the parent.
    for marker in [
        ("TOLE_AGENT_DEPTH", "1"),
        ("TOLE_AGENT_ID", "a-1-2-3"),
        ("TOLE_DELEGATION_NODE", "n1"),
        ("TOLE_DELEGATION_LOG", "/nonexistent/log"),
    ] {
        let o = tole(&root, Some(USER), &[marker], &wide);
        assert!(!refused(&o), "{marker:?}: {}", text(&o.stderr));
    }
}

/// A script that a child agent's `run_command` started carries the session
/// marker AND the child variables it inherited: the child variables do not
/// exempt it, it is guarded like any script-started tole.
#[test]
fn a_descendant_script_of_a_child_agent_is_guarded_despite_the_inherited_child_variables() {
    let root = TestDir::new("tole-narrow-descendant");
    for marker in [
        ("TOLE_AGENT_DEPTH", "1"),
        ("TOLE_AGENT_ID", "a-1-2-3"),
        ("TOLE_DELEGATION_NODE", "n1"),
    ] {
        let mut env = SESSION.to_vec();
        env.push(marker);
        let o = tole(
            &root,
            Some(USER),
            &env,
            &["run", "--allow", "edit_*", "--", "hi"],
        );
        assert_refused(&o, &["--allow", "edit_*"]);
    }
}

/// `acp` (like `serve` and `mcp`) is started by the owner with its own flags:
/// exempt even with the session marker.
#[test]
fn the_acp_face_is_exempt() {
    let root = TestDir::new("tole-narrow-acp");
    let o = tole(&root, Some(USER), SESSION, &["acp", "--allow", "edit_*"]);
    assert!(!refused(&o), "{}", text(&o.stderr));
}
