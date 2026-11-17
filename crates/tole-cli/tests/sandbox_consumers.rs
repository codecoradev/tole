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
fn doctor_says_verify_and_turnend_are_sandboxed_by_default_and_the_rest_not() {
    let (code, out) = doctor(None);
    assert_eq!(code, Some(0), "{out}");
    let l = line(&out, "sandbox consumers:");
    assert!(l[0].contains("run_command off, job_start off"), "{out}");
    assert!(
        l[0].contains("verify optional (default), turnend optional (default)"),
        "verify and turnend are optional by default (#748), shown as the default: {out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("verify and turnend sandboxed by default")
                && x.contains("\"off\" in [sandbox.consumers] to opt out")),
        "{out}"
    );
    // This harness has no bwrap on PATH: the default degrades loudly and the
    // doctor says so (a WARNING, not an ERROR: it does not fail the doctor).
    for k in ["verify", "turnend"] {
        assert!(
            l.iter()
                .any(|x| x.contains(&format!("WARNING: {k} runs UNSANDBOXED"))
                    && x.contains("no sandbox is available on this host")
                    && x.contains(&format!("sandbox.consumers.{k} = \"off\""))),
            "{k}: {out}"
        );
    }
    assert!(
        out.contains("sandbox consumer verify: unsandboxed, no sandbox"),
        "{out}"
    );
    assert!(out.contains("(default)"), "{out}");
    // The `git` tool hardening is for a consumer the owner wrote, not the default.
    assert!(
        !l.iter().any(|x| x.contains("git writes (add, commit)")),
        "{out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("cron scripts and command: deliveries run unsandboxed")),
        "{out}"
    );
    assert!(
        l[0].contains("turnend optional (default), cron off"),
        "cron is a consumer now: {out}"
    );
    assert!(
        !l.iter().any(|x| x.contains("not sandboxed yet")),
        "nothing is left that is not a consumer yet: {out}"
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
        "cron = \"always\"",
        "crons = \"required\"",
    ] {
        let o = tole(
            Some(&format!("[sandbox.consumers]\n{bad}\n")),
            &["config", "check"],
        );
        assert!(!o.status.success(), "{bad}");
        let err = text(&o.stderr);
        assert!(
            err.contains("config.toml")
                && (err.contains("sandbox.consumers") || err.contains("unknown field `crons`")),
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

/// Slice 2: `verify` and `turnend` are consumers. A required one with no
/// sandbox fails the doctor; so does an optional stop gate or `verify` (both
/// run unattended, so optional is required).
#[test]
fn doctor_covers_verify_and_turnend() {
    let (code, out) = doctor(Some(
        "[sandbox.consumers]\nverify = \"required\"\nturnend = \"optional\"\n",
    ));
    assert_ne!(code, Some(0), "required verify cannot run: {out}");
    let l = line(&out, "sandbox consumers:");
    assert!(
        l[0].contains("verify required, turnend optional") && l[0].contains("network none"),
        "{out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("ERROR verify") && x.contains("refused (required)")),
        "{out}"
    );
    assert!(
        l.iter()
            .any(|x| x.contains("ERROR turnend") && x.contains("unattended")),
        "{out}"
    );

    let (code, out) = doctor(Some("[sandbox.consumers]\nturnend = \"optional\"\n"));
    assert_ne!(
        code,
        Some(0),
        "an optional stop gate cannot fall back: {out}"
    );
    assert!(out.contains("sandbox consumers: ERROR turnend"), "{out}");

    let (code, out) = doctor(Some("[sandbox.consumers]\nverify = \"optional\"\n"));
    assert_ne!(code, Some(0), "{out}");
    assert!(
        out.contains("sandbox consumers: ERROR verify") && out.contains("mission"),
        "{out}"
    );
}

#[test]
fn config_check_shows_the_verify_and_turnend_keys() {
    let o = tole(
        Some("[sandbox.consumers]\nverify = \"required\"\nturnend = \"optional\"\n"),
        &["config", "check"],
    );
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out} {}", text(&o.stderr));
    for want in [
        "sandbox.consumers.verify = \"required\"  [user config]",
        "sandbox.consumers.turnend = \"optional\"  [user config]",
    ] {
        assert!(out.contains(want), "{want} in {out}");
    }
}

/// Slice 3: `cron` is a consumer. It is always unattended, so a required or an
/// optional one with no sandbox fails the doctor, and `config check` shows it.
#[test]
fn doctor_and_config_check_cover_cron() {
    let (code, out) = doctor(Some("[sandbox.consumers]\ncron = \"optional\"\n"));
    assert_ne!(code, Some(0), "optional cron counts as required: {out}");
    let l = line(&out, "sandbox consumers:");
    assert!(l[0].contains("cron optional"), "{out}");
    assert!(
        l.iter()
            .any(|x| x.contains("ERROR cron") && x.contains("unattended")),
        "{out}"
    );
    assert!(
        l.iter().any(|x| x.contains("no unsandboxed fallback")),
        "{out}"
    );
    let o = tole(
        Some("[sandbox.consumers]\ncron = \"required\"\n"),
        &["config", "check"],
    );
    let out = text(&o.stdout);
    assert!(
        out.contains("sandbox.consumers.cron = \"required\"  [user config]"),
        "{out}"
    );
}
