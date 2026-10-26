//! #422: `tole doctor` reports the sibling binaries (cora, uteke) against
//! the supported floor — end to end with the REAL binary and fake
//! siblings on PATH. Reported, never a blocker.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-doctor-eco-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fake_bin(dir: &Path, name: &str, version_line: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
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
