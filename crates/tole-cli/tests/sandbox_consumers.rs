//! #748: `[sandbox.consumers]` end to end with the REAL binary: `tole config
//! check` shows the keys and rejects bad values, and `tole doctor` says what
//! the consumers are and fails when a required one has no sandbox. The child
//! gets its own `CODECORA_HOME` and an empty directory as `PATH`, so there is
//! never a `bwrap` and no user config leaks in; nothing here needs bubblewrap.

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Command, Output};

fn tmpdir() -> TestDir {
    TestDir::new("tole-sbxc-e2e")
}

/// Runs `tole args` with this user config (None: no user config).
fn tole(user_config: Option<&str>, args: &[&str]) -> Output {
    let (home, cwd, path) = (tmpdir(), tmpdir(), tmpdir());
    if let Some(c) = user_config {
        std::fs::create_dir_all(home.join("tole")).unwrap();
        std::fs::write(home.join("tole/config.toml"), c).unwrap();
    }
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(&cwd)
        .env_clear()
        .env("PATH", &path)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", &home)
        // A configured (never contacted) provider, so the offline doctor
        // has nothing else to fail on.
        .env("TOLE_BASE_URL", "http://127.0.0.1:9")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn doctor(user_config: Option<&str>) -> (Option<i32>, String) {
    let o = tole(user_config, &["doctor", "--tier", "offline", "--no-ledger"]);
    (o.status.code(), text(&o.stdout))
}

fn line<'a>(out: &'a str, starts: &str) -> Vec<&'a str> {
    out.lines().filter(|l| l.starts_with(starts)).collect()
}

#[test]
fn doctor_says_model_commands_are_unsandboxed_by_default() {
    let (code, out) = doctor(None);
    assert_eq!(code, Some(0), "{out}");
    let l = line(&out, "sandbox consumers:");
    assert!(
        l[0].contains("run_command off, job_start off") && l[0].contains("unsandboxed"),
        "{out}"
    );
    assert!(
        l[0].contains("verify off"),
        "verify is a consumer now: {out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("turnend, cron scripts and deliveries")
                && x.contains("not sandboxed yet")),
        "{out}"
    );
    assert!(
        !l.iter()
            .any(|x| x.contains("verify,") && x.contains("not sandboxed yet")),
        "{out}"
    );
    assert!(!out.contains("sandbox consumers: ERROR"), "{out}");
}

#[test]
fn doctor_fails_when_a_required_consumer_has_no_sandbox_and_only_warns_for_optional() {
    let (code, out) = doctor(Some(
        "[sandbox.consumers]\nrun_command = \"required\"\njob_start = \"optional\"\n",
    ));
    assert_ne!(code, Some(0), "a required consumer cannot run: {out}");
    let l = line(&out, "sandbox consumers:");
    assert!(
        l[0].contains("run_command required, job_start optional") && l[0].contains("network none"),
        "{out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("ERROR run_command") && x.contains("refused (required)")),
        "{out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("WARNING job_start") && x.contains("unattended")),
        "{out}"
    );

    // Optional alone is not a failure.
    let (code, out) = doctor(Some("[sandbox.consumers]\njob_start = \"optional\"\n"));
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("sandbox consumers: WARNING job_start"),
        "{out}"
    );
    assert!(!out.contains("sandbox consumers: ERROR"), "{out}");
}

#[test]
fn config_check_shows_the_keys_and_rejects_bad_values() {
    let cfg = "[sandbox.consumers]\nrun_command = \"required\"\nnetwork = \"host\"\n\
               processes = 128\n";
    let o = tole(Some(cfg), &["config", "check"]);
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out} {}", text(&o.stderr));
    for want in [
        "sandbox.consumers.run_command = \"required\"  [user config]",
        "sandbox.consumers.network = \"host\"  [user config]",
        "sandbox.consumers.processes = 128  [user config]",
    ] {
        assert!(out.contains(want), "{want} in {out}");
    }
    for bad in [
        "run_command = \"always\"",
        "network = \"any\"",
        "memory_mb = 1",
        "verify = \"always\"",
        "turnend = \"required\"",
    ] {
        let o = tole(
            Some(&format!("[sandbox.consumers]\n{bad}\n")),
            &["config", "check"],
        );
        assert!(!o.status.success(), "{bad}");
        let err = text(&o.stderr);
        assert!(
            err.contains("config.toml")
                && (err.contains("sandbox.consumers") || err.contains("unknown field `turnend`")),
            "{bad}: {err}"
        );
    }
}

#[test]
fn a_project_config_cannot_set_consumers() {
    let (home, cwd, path) = (tmpdir(), tmpdir(), tmpdir());
    std::fs::create_dir_all(cwd.join(".tole")).unwrap();
    std::fs::write(
        cwd.join(".tole/config.toml"),
        "[sandbox.consumers]\nnetwork = \"host\"\n",
    )
    .unwrap();
    let run = |cwd: &Path| {
        Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(["config", "check"])
            .current_dir(cwd)
            .env_clear()
            .env("PATH", &path)
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("CODECORA_HOME", &home)
            .output()
            .unwrap()
    };
    let out = text(&run(&cwd).stdout);
    assert!(
        out.lines()
            .any(|l| l.contains("sandbox.consumers.network") && l.contains("IGNORED")),
        "{out}"
    );
}

/// Slice 2: `verify` is a consumer. A required one with no sandbox fails the
/// doctor; an optional one fails it too (a mission is unattended, so optional
/// counts as required).
#[test]
fn doctor_covers_verify() {
    let (code, out) = doctor(Some("[sandbox.consumers]\nverify = \"required\"\n"));
    assert_ne!(code, Some(0), "required verify cannot run: {out}");
    let l = line(&out, "sandbox consumers:");
    assert!(
        l[0].contains("verify required") && l[0].contains("network none"),
        "{out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("ERROR verify") && x.contains("refused (required)")),
        "{out}"
    );

    let (code, out) = doctor(Some("[sandbox.consumers]\nverify = \"optional\"\n"));
    assert_ne!(code, Some(0), "{out}");
    assert!(
        out.contains("sandbox consumers: ERROR verify") && out.contains("mission"),
        "{out}"
    );
}

#[test]
fn config_check_shows_the_verify_key() {
    let o = tole(
        Some("[sandbox.consumers]\nverify = \"required\"\n"),
        &["config", "check"],
    );
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out} {}", text(&o.stderr));
    assert!(
        out.contains("sandbox.consumers.verify = \"required\"  [user config]"),
        "{out}"
    );
}
