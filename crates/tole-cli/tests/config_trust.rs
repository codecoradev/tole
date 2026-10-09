//! `tole config trust|untrust|check` end-to-end (#208 part 2) with the REAL
//! binary. `CODECORA_HOME` is set on the CHILD only; the test process
//! environment is never touched. Stdin is null and stdout/stderr are
//! pipes, so the child is non-interactive.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-trust-e2e-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tole(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(cwd)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

fn project(content: &str) -> (PathBuf, PathBuf) {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(".tole/config.toml"), content).unwrap();
    (d, tmpdir())
}

#[test]
fn trust_lifecycle() {
    let (d, home) = project("model = \"m1\"\n");

    let o = tole(&d, &home, &["config", "check"]);
    assert!(o.status.success());
    assert!(
        text(&o.stdout).contains("trust: NOT trusted — run: tole config trust"),
        "{}",
        text(&o.stdout)
    );

    let o = tole(&d, &home, &["config", "trust", "--yes"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let shown = text(&o.stderr);
    assert!(shown.contains("model = \"m1\""), "content printed: {shown}");
    assert!(shown.contains("trusted:"), "{shown}");
    let store = home.join("tole/trusted-configs.json");
    assert!(store.exists());

    let o = tole(&d, &home, &["config", "check"]);
    assert!(o.status.success());
    assert!(text(&o.stdout).contains("trust: trusted"));

    std::fs::write(d.join(".tole/config.toml"), "model = \"m2\"\n").unwrap();
    let o = tole(&d, &home, &["config", "check"]);
    assert!(o.status.success(), "validity != trust");
    assert!(
        text(&o.stdout).contains("trust: CHANGED since it was trusted — run: tole config trust"),
        "{}",
        text(&o.stdout)
    );

    // Re-trusting the changed file shows the diff.
    let o = tole(&d, &home, &["config", "trust", "--yes"]);
    assert!(o.status.success());
    let shown = text(&o.stderr);
    assert!(shown.contains("- model = \"m1\""), "{shown}");
    assert!(shown.contains("+ model = \"m2\""), "{shown}");

    let o = tole(&d, &home, &["config", "untrust"]);
    assert!(o.status.success());
    assert!(text(&o.stderr).contains("untrusted:"));
    let o = tole(&d, &home, &["config", "check"]);
    assert!(text(&o.stdout).contains("trust: NOT trusted"));

    // Idempotent.
    let o = tole(&d, &home, &["config", "untrust"]);
    assert!(o.status.success());
    assert!(text(&o.stderr).contains("nothing to remove"));
}

#[test]
fn trust_without_yes_on_non_tty_fails() {
    let (d, home) = project("model = \"m1\"\n");
    let o = tole(&d, &home, &["config", "trust"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("--yes"), "{}", text(&o.stderr));
    assert!(!home.join("tole/trusted-configs.json").exists());
}

#[test]
fn trust_refuses_invalid_file() {
    let (d, home) = project("plan_mode = \"yes\"\n");
    let o = tole(&d, &home, &["config", "trust", "--yes"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("config.toml:1:"));
    assert!(!home.join("tole/trusted-configs.json").exists());
}

#[test]
fn corrupt_store_is_reported_and_left_alone() {
    let (d, home) = project("model = \"m1\"\n");
    std::fs::create_dir_all(home.join("tole")).unwrap();
    let store = home.join("tole/trusted-configs.json");
    std::fs::write(&store, "oops").unwrap();
    let o = tole(&d, &home, &["config", "trust", "--yes"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("trusted-configs.json"));
    assert_eq!(std::fs::read_to_string(&store).unwrap(), "oops");
}
