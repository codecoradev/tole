//! #748 slice 2: the `on_turnend` stop gates and the verify command under
//! `[sandbox.consumers]` really run inside bubblewrap with the `command`
//! profile: the workspace is writable, `.git/hooks`, `.git/config` and
//! `.tole` are not, `$HOME` and the data dir are absent, and the exit
//! status, stdout and stdin payload behave as without a sandbox.
//!
//! Needs a working `bwrap`. Without one these tests print a `SKIPPED:` line
//! with the reason and return (`TOLE_REQUIRE_BWRAP=1` makes that a failure,
//! as the CI job `Sandbox (bubblewrap)` sets it). The consumer settings and
//! the working directory (the stop gates and verify run in it) are
//! process-wide and set once here, so every test uses its own file names.

#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tole_core::hooks::turnend_hook;
use tole_core::sandbox::command::{self, Consumer, Consumers};
use tole_core::sandbox::{self, Availability, Mode};

struct Setup {
    /// The workspace: the process's working directory.
    ws: PathBuf,
    /// The fake `$HOME` of this process, holding a canary file.
    home: PathBuf,
    /// A directory outside the workspace, holding an executable gate.
    bin: PathBuf,
}

fn setup() -> &'static Setup {
    static S: OnceLock<Setup> = OnceLock::new();
    S.get_or_init(|| {
        // Outside `/tmp` (a tmpfs inside the sandbox) and outside any system
        // dir, so "absent" means the sandbox hid it.
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shared-gatesbx");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let (home, ws, bin) = (base.join("home"), base.join("ws"), base.join("bin"));
        for d in [
            home.join(".codecora/tole"),
            ws.join(".git/hooks"),
            ws.join(".tole"),
            bin.clone(),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join("canary"), "x").unwrap();
        std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        std::env::set_var("HOME", &home);
        std::env::remove_var("CODECORA_HOME");
        std::env::set_current_dir(&ws).unwrap();
        sandbox::command::set_consumers(Consumers {
            verify: Mode::Required,
            turnend: Mode::Required,
            cron: Mode::Off,
            ..Consumers::default()
        })
        .unwrap();
        Setup { ws, home, bin }
    })
}

/// True (after printing the reason) when no sandbox works here.
fn skipped() -> bool {
    setup();
    match sandbox::detect() {
        Availability::Usable { .. } => false,
        Availability::Unavailable(why) => {
            assert!(
                std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
                "TOLE_REQUIRE_BWRAP is set but no sandbox works: {why}"
            );
            eprintln!("SKIPPED: no working sandbox here: {why}");
            true
        }
    }
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// The probe both consumers run: `$1` is a name unique to the test. Prints
/// what it could and could not do; the payload (stdin) is echoed by `cat`.
fn probe(tag: &str) -> String {
    let home = setup().home.display();
    format!(
        r#"echo "wsw=$( (echo x > made-{tag}) 2>/dev/null && echo writable || echo readonly)"
echo "hooks=$( (echo x > .git/hooks/evil-{tag}) 2>/dev/null && echo writable || echo readonly)"
echo "config=$( (echo x >> .git/config) 2>/dev/null && echo writable || echo readonly)"
echo "tole=$( (echo x > .tole/evil-{tag}) 2>/dev/null && echo writable || echo readonly)"
echo "home=$(test -e {home}/canary && echo visible || echo absent)"
echo "stdin=$(cat | head -c 40)""#
    )
}

fn kv(out: &str, key: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key}= in {out:?}"))
        .to_string()
}

fn assert_profile(out: &str, tag: &str) {
    let s = setup();
    assert_eq!(kv(out, "wsw"), "writable", "the workspace is writable");
    assert!(
        s.ws.join(format!("made-{tag}")).exists(),
        "reached the host"
    );
    assert_eq!(kv(out, "hooks"), "readonly", ".git/hooks");
    assert_eq!(kv(out, "config"), "readonly", ".git/config");
    assert_eq!(kv(out, "tole"), "readonly", ".tole");
    assert_eq!(kv(out, "home"), "absent", "the real $HOME is absent");
    for p in [
        format!(".git/hooks/evil-{tag}"),
        format!(".tole/evil-{tag}"),
    ] {
        assert!(!s.ws.join(&p).exists(), "{p} was not created on the host");
    }
    assert_eq!(
        std::fs::read_to_string(s.ws.join(".git/config")).unwrap(),
        "[core]\n",
        ".git/config is untouched"
    );
}

/// A sandboxed stop gate: the profile, the JSON payload on stdin, the exit
/// code and stdout as the verdict.
#[test]
fn a_stop_gate_runs_in_the_command_profile_and_keeps_its_verdict() {
    if skipped() {
        return;
    }
    let s = setup();
    write_script(&s.ws, "gate-view.sh", &probe("gate"));
    let hook = turnend_hook("sh ./gate-view.sh");
    let (code, out) = hook
        .run_raw(&json!({"event": "turnend", "tools": []}))
        .unwrap();
    assert_eq!(code, 0, "{out}");
    assert_profile(&out, "gate");
    assert!(
        kv(&out, "stdin").starts_with("{\"event\":\"turnend\""),
        "the payload reaches the gate: {out}"
    );

    // A non-zero exit is the verdict, stdout its reason (cargo exits 101).
    write_script(&s.ws, "gate-no.sh", "echo tests failing; exit 101");
    let (code, out) = turnend_hook("sh ./gate-no.sh").run_raw(&json!({})).unwrap();
    assert_eq!((code, out.as_str()), (101, "tests failing"));
}

/// The owner's gate is usually a path: an absolute one outside the workspace
/// runs through its own read-only directory.
#[test]
fn a_stop_gate_given_by_absolute_path_runs_sandboxed() {
    if skipped() {
        return;
    }
    let s = setup();
    let gate = write_script(&s.bin, "abs-gate.sh", &probe("abs"));
    let (code, out) = turnend_hook(&gate.display().to_string())
        .run_raw(&json!({"event": "turnend"}))
        .unwrap();
    assert_eq!(code, 0, "{out}");
    assert_profile(&out, "abs");
    // The gate's own directory is read-only to it.
    write_script(
        &s.bin,
        "abs-tamper.sh",
        &format!(
            "echo x > {}/planted 2>/dev/null && echo planted || echo ok",
            s.bin.display()
        ),
    );
    let (_, out) = turnend_hook(&s.bin.join("abs-tamper.sh").display().to_string())
        .run_raw(&json!({}))
        .unwrap();
    assert_eq!(out, "ok");
    assert!(!s.bin.join("planted").exists());
}

/// A gate that is not there is a denial of the gate (a refusal), never a
/// pass.
#[test]
fn a_missing_stop_gate_is_a_deny_not_a_pass() {
    if skipped() {
        return;
    }
    let (code, out) = turnend_hook("./no-such-gate.sh")
        .run_raw(&json!({}))
        .unwrap();
    assert_ne!(code, 0);
    assert!(
        out.contains("not an executable file") && out.starts_with("turnend: "),
        "{out}"
    );
}

/// A sandboxed gate that sleeps past its budget is killed and DENIED (exit
/// 126, the reason says so), never passed (#860). A gate that finishes inside
/// the same budget keeps its own verdict.
#[test]
fn a_sandboxed_gate_that_times_out_is_denied_not_passed() {
    if skipped() {
        return;
    }
    let s = setup();
    write_script(&s.ws, "gate-slow.sh", "touch gate-slow-started; sleep 30");
    let hook = turnend_hook("sh ./gate-slow.sh").with_timeout(Duration::from_secs(2));
    let started = std::time::Instant::now();
    let (code, why) = hook
        .run_raw(&json!({}))
        .expect("a sandboxed timeout is a deny, not an Err");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "killed at the budget"
    );
    assert_eq!(code, tole_core::hooks::SANDBOX_REFUSED, "{why}");
    assert_eq!(
        why,
        "turnend: the stop gate timed out after 2s inside the sandbox and was killed; \
         a timeout is not a pass"
    );
    assert!(s.ws.join("gate-slow-started").exists(), "the gate did run");

    write_script(&s.ws, "gate-quick.sh", "echo fine");
    let (code, out) = turnend_hook("sh ./gate-quick.sh")
        .with_timeout(Duration::from_secs(20))
        .run_raw(&json!({}))
        .unwrap();
    assert_eq!((code, out.as_str()), (0, "fine"));
}

/// The verify command: `sh -c` in the same profile, exit status and output
/// as without a sandbox.
#[test]
fn the_verify_command_runs_in_the_command_profile() {
    if skipped() {
        return;
    }
    let command::Launched { mut cmd, report } = command::command_in_cwd(
        Consumer::Verify,
        "sh",
        &["-c".into(), format!("{}; exit 3", probe("verify"))],
    )
    .unwrap();
    assert!(
        report.is_some(),
        "wrapped, so a launcher report comes with it"
    );
    tole_core::subprocess::scrub_env_for(&mut cmd, tole_core::secret_release::VERIFY);
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, Duration::from_secs(60)).unwrap();
    assert_eq!(out.status.code(), Some(3), "the exit status is the verdict");
    assert_profile(&String::from_utf8_lossy(&out.stdout), "verify");
}
