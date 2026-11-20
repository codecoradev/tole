//! #435: probe misses (no uteke, no cora, no GitHub origin) are expected on
//! most machines, so `tole run` stays quiet about them unless
//! `TOLE_VERBOSE=1`. End to end with the REAL binary; the provider is
//! unreachable, so the run stops right after startup.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use std::process::Stdio;

const NOTICES: [&str; 3] = [
    "uteke binary not found",
    "cora binary not found",
    "no GitHub origin remote detected",
];

fn startup_stderr(verbose: Option<&str>) -> String {
    // Outside the checkout (cargo's target dir is inside it in CI): the test
    // is about a cwd that is not a git checkout.
    let dir = TestDir::in_system_temp(&format!("tole-quiet-{}", verbose.unwrap_or("default")));
    let home =
        TestDir::in_system_temp(&format!("tole-quiet-home-{}", verbose.unwrap_or("default")));
    // No uteke / cora on PATH, and the temp dir is not a git checkout.
    let path = common::path_without_sibling_binaries_from("/usr/bin:/bin".as_ref());
    let mut cmd = common::hermetic_command(
        env!("CARGO_BIN_EXE_tole"),
        &home,
        &[("PATH", path.to_str().unwrap())],
    );
    cmd.args(["run", "-s", dir.join("sessions").to_str().unwrap(), "hi"])
        .current_dir(&dir)
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_FALLBACK_BASE_URL")
        .env_remove("TOLE_MEMORY")
        .env_remove("TOLE_TRUST")
        .env_remove("TOLE_AGENT_DEPTH")
        .stdin(Stdio::null());
    match verbose {
        Some(v) => cmd.env("TOLE_VERBOSE", v),
        None => cmd.env_remove("TOLE_VERBOSE"),
    };
    let out = cmd.output().expect("run tole");
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn probe_misses_are_silent_by_default() {
    let err = startup_stderr(None);
    for n in NOTICES {
        assert!(!err.contains(n), "unexpected notice {n:?}: {err}");
    }
}

#[test]
fn tole_verbose_shows_the_probe_misses() {
    let err = startup_stderr(Some("1"));
    for n in NOTICES {
        assert!(err.contains(n), "missing notice {n:?}: {err}");
    }
}
