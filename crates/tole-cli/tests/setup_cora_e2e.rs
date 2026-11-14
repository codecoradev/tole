//! #535 `tole setup cora` — end to end with the REAL binary. PATH holds only
//! fake binaries (never the real `cargo`, which would install the real crate):
//! a fake `cargo` that "installs" a fake `cora` into `$CARGO_HOME/bin`, and
//! optionally a fake `cora` already on PATH.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-setup-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\nPATH=/usr/bin:/bin\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake `cargo` that records its argv and, for `install cora-code`, drops a
/// fake `cora` (version from `$FAKE_CORA_VERSION`) into `$CARGO_HOME/bin`.
fn fake_cargo(bin: &Path, log: &Path) {
    script(
        &bin.join("cargo"),
        &format!(
            "echo \"$@\" >> {log}\n\
             if [ \"$1 $2 $3\" = \"install cora-code --locked\" ]; then\n\
               mkdir -p \"$CARGO_HOME/bin\"\n\
               printf '#!/bin/sh\\necho cora %s\\n' \"$FAKE_CORA_VERSION\" > \"$CARGO_HOME/bin/cora\"\n\
               chmod +x \"$CARGO_HOME/bin/cora\"\n\
               exit 0\n\
             fi\n\
             exit 3\n",
            log = log.display()
        ),
    );
}

fn fake_cora(bin: &Path, version: &str) {
    script(&bin.join("cora"), &format!("echo 'cora {version}'\n"));
}

struct Run {
    out: String,
    err: String,
    code: i32,
}

fn tole_setup(bin: &Path, cargo_home: &Path, args: &[&str]) -> Run {
    let home = tmpdir("home");
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .arg("setup")
        .args(args)
        .stdin(Stdio::null())
        .env("PATH", bin)
        .env("HOME", &home)
        .env("CARGO_HOME", cargo_home)
        .env("FAKE_CORA_VERSION", "0.20.0")
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    Run {
        out: String::from_utf8_lossy(&o.stdout).to_string(),
        err: String::from_utf8_lossy(&o.stderr).to_string(),
        code: o.status.code().unwrap_or(-1),
    }
}

fn cargo_calls(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

#[test]
fn setup_installs_a_missing_cora_after_yes_and_verifies_it() {
    let (bin, ch) = (tmpdir("bin"), tmpdir("cargohome"));
    let log = bin.join("cargo.log");
    fake_cargo(&bin, &log);
    let r = tole_setup(&bin, &ch, &["cora", "--yes"]);
    assert_eq!(r.code, 0, "{} / {}", r.out, r.err);
    assert!(r.out.contains("installed: cora 0.20.0"), "{}", r.out);
    assert_eq!(cargo_calls(&log).trim(), "install cora-code --locked");
    // The binary landed in CARGO_HOME, which is not on PATH here: say so.
    assert!(r.err.contains("first on PATH"), "{}", r.err);
}

#[test]
fn setup_upgrades_a_too_old_cora() {
    let (bin, ch) = (tmpdir("bin"), tmpdir("cargohome"));
    let log = bin.join("cargo.log");
    fake_cargo(&bin, &log);
    fake_cora(&bin, "0.18.0");
    let r = tole_setup(&bin, &ch, &["cora", "--yes"]);
    assert_eq!(r.code, 0, "{} / {}", r.out, r.err);
    assert!(
        r.out.contains("older than the supported 0.19.0"),
        "{}",
        r.out
    );
    assert!(r.out.contains("installed: cora 0.20.0"), "{}", r.out);
    assert_eq!(cargo_calls(&log).trim(), "install cora-code --locked");
}

#[test]
fn setup_is_a_no_op_when_cora_is_supported() {
    let (bin, ch) = (tmpdir("bin"), tmpdir("cargohome"));
    let log = bin.join("cargo.log");
    fake_cargo(&bin, &log);
    fake_cora(&bin, "0.20.0");
    let r = tole_setup(&bin, &ch, &["cora", "--yes"]);
    assert_eq!(r.code, 0, "{} / {}", r.out, r.err);
    assert!(r.out.contains("nothing to do"), "{}", r.out);
    assert_eq!(cargo_calls(&log), "", "cargo must not run");
    assert!(!ch.join("bin/cora").exists());
}

#[test]
fn setup_refuses_to_install_without_a_terminal_or_yes() {
    let (bin, ch) = (tmpdir("bin"), tmpdir("cargohome"));
    let log = bin.join("cargo.log");
    fake_cargo(&bin, &log);
    let r = tole_setup(&bin, &ch, &["cora"]);
    assert_eq!(r.code, 2, "{} / {}", r.out, r.err);
    assert!(r.err.contains("--yes"), "{}", r.err);
    assert_eq!(cargo_calls(&log), "", "never silent, never unprompted");
}

#[test]
fn setup_check_reports_and_never_installs() {
    let (bin, ch) = (tmpdir("bin"), tmpdir("cargohome"));
    let log = bin.join("cargo.log");
    fake_cargo(&bin, &log);
    let r = tole_setup(&bin, &ch, &["cora", "--check", "--yes"]);
    assert_eq!(r.code, 0, "{} / {}", r.out, r.err);
    assert!(
        r.out.contains("cargo install cora-code --locked"),
        "{}",
        r.out
    );
    assert!(r.out.contains("tole setup cora"), "{}", r.out);
    assert_eq!(cargo_calls(&log), "");
}

#[test]
fn setup_without_cargo_prints_the_manual_command() {
    // PATH is an empty directory: no cargo, no cora.
    let (bin, ch) = (tmpdir("empty"), tmpdir("cargohome"));
    let r = tole_setup(&bin, &ch, &["cora", "--yes"]);
    assert_eq!(r.code, 1, "{} / {}", r.out, r.err);
    assert!(r.err.contains("cargo not found"), "{}", r.err);
    assert!(
        r.err.contains("cargo install cora-code --locked"),
        "{}",
        r.err
    );
}

#[test]
fn setup_fails_loudly_when_the_install_does_not_produce_a_usable_cora() {
    let (bin, ch) = (tmpdir("bin"), tmpdir("cargohome"));
    let log = bin.join("cargo.log");
    fake_cargo(&bin, &log);
    // The fake install drops a cora that is still too old.
    let home = tmpdir("home");
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["setup", "cora", "--yes"])
        .stdin(Stdio::null())
        .env("PATH", &bin)
        .env("HOME", &home)
        .env("CARGO_HOME", &ch)
        .env("FAKE_CORA_VERSION", "0.18.9")
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("not usable"), "{err}");
}
