//! #435: probe misses (no uteke, no cora, no GitHub origin) are expected on
//! most machines, so `tole run` stays quiet about them unless
//! `TOLE_VERBOSE=1`. End to end with the REAL binary; the provider is
//! unreachable, so the run stops right after startup.

#![cfg(feature = "shell-tools")]

use std::process::{Command, Stdio};

const NOTICES: [&str; 3] = [
    "uteke binary not found",
    "cora binary not found",
    "no GitHub origin remote detected",
];

fn startup_stderr(verbose: Option<&str>) -> String {
    let dir = std::env::temp_dir().join(format!(
        "tole-quiet-{}-{}",
        verbose.unwrap_or("default"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args(["run", "-s", dir.join("sessions").to_str().unwrap(), "hi"])
        .current_dir(&dir)
        // No uteke / cora on PATH, and the temp dir is not a git checkout.
        .env("PATH", "/usr/bin:/bin")
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
