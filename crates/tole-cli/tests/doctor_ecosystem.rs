//! #422: `tole doctor` reports the sibling binaries (cora, uteke) against
//! the supported floor — end to end with the REAL binary and fake
//! siblings on PATH. Reported, never a blocker.

#![cfg(unix)]

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Command, Stdio};

fn tmpdir(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-doctor-eco-{tag}"))
}

fn fake_bin(dir: &Path, name: &str, version_line: &str) {
    let p = dir.join(name);
    common::write_executable(&p, format!("#!/bin/sh\necho '{version_line}'\n"));
}

#[test]
fn doctor_reports_old_and_missing_siblings_without_blocking() {
    let bin = tmpdir("bin");
    let home = tmpdir("home");
    fake_bin(&bin, "cora", "cora 0.18.0");
    // No uteke on PATH at all.
    let out = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["doctor", "--tier", "offline", "--no-ledger"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .stdin(Stdio::null())
        .output()
        .expect("run tole doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "offline tier must still pass: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("ecosystem:"), "{stdout}");
    assert!(
        stdout.contains("✗ cora 0.18.0 is older than the supported 0.19.0"),
        "{stdout}"
    );
    assert!(
        stdout.contains("cargo install cora-code --locked"),
        "{stdout}"
    );
    assert!(stdout.contains("- uteke not installed"), "{stdout}");
    let _ = std::fs::remove_dir_all(&bin);
    let _ = std::fs::remove_dir_all(&home);
}

/// #587: the advisor tier is reported by profile name and cap, never by
/// model.
#[test]
fn doctor_names_the_advisor_profiles_but_not_their_models() {
    let home = tmpdir("advisor");
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(
        home.join("tole/config.toml"),
        "[profiles.coder]\nadvisor_max_uses = 2\n[profiles.coder.models]\n\
         standard = \"m\"\nadvisor = \"wise-model\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["doctor", "--tier", "offline", "--no-ledger"])
        .env("PATH", "/usr/bin:/bin")
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .stdin(Stdio::null())
        .output()
        .expect("run tole doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("advisor: coder (2 uses per run)"),
        "{stdout}"
    );
    assert!(!stdout.contains("wise-model"), "{stdout}");
    let _ = std::fs::remove_dir_all(&home);
}

/// #675: the doctor shows that the process guard is in force (Linux).
#[cfg(target_os = "linux")]
#[test]
fn doctor_reports_the_process_guard() {
    let home = tmpdir("guard");
    let out = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["doctor", "--tier", "offline", "--no-ledger"])
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .stdin(Stdio::null())
        .output()
        .expect("run tole doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("process guard: ✓ non-dumpable"), "{stdout}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn doctor_warns_when_codecora_home_leaves_an_existing_uteke_home_behind() {
    let bin = tmpdir("move-bin");
    let real_home = tmpdir("move-home");
    let cc = tmpdir("move-cc");
    std::fs::create_dir_all(real_home.join(".codecora/uteke")).unwrap();
    let run = |cc_home: &Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(["doctor", "--tier", "offline", "--no-ledger"])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &*real_home)
            .env("CODECORA_HOME", cc_home)
            .env_remove("UTEKE_HOME")
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
            .env("TOLE_MODEL", "m")
            .env("TOLE_API_KEY", "k")
            .stdin(Stdio::null())
            .output()
            .expect("run tole doctor");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let warned = run(&cc);
    assert!(
        warned.contains("! uteke: CODECORA_HOME is set") && warned.contains("export UTEKE_HOME="),
        "{warned}"
    );
    std::fs::create_dir_all(cc.join("uteke")).unwrap();
    assert!(
        !run(&cc).contains("! uteke: CODECORA_HOME"),
        "new home exists: no warning"
    );
}
