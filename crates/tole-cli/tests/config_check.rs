//! `tole config check` end-to-end (#208 part 1) with the REAL binary:
//! exit codes and key output lines. Nothing here touches the process
//! environment (the child gets an explicit cwd and its own env, with an
//! empty `CODECORA_HOME` so no user config leaks in).

mod common;
use common::TestDir;
use std::path::Path;
use std::process::Output;

fn tmpdir() -> TestDir {
    TestDir::new("tole-cfg-e2e")
}

fn tole(cwd: &Path, args: &[&str]) -> Output {
    common::hermetic_command(env!("CARGO_BIN_EXE_tole"), tmpdir().path(), &[])
        .args(args)
        .current_dir(cwd)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

#[test]
fn no_config_exits_zero_with_notice() {
    let d = tmpdir();
    let o = tole(&d, &["config", "check"]);
    assert!(o.status.success());
    let out = text(&o.stdout);
    assert!(out.starts_with("no user config at "), "{out}");
    assert!(out.contains("\nno .tole/config.toml in "), "{out}");
}

#[test]
fn valid_config_prints_path_keys_and_source() {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(
        d.join(".tole/config.toml"),
        "model = \"m1\"\ntrust = [\"internal\"]\nplan_mode = true\n[mission]\nmax_steps = 7\n",
    )
    .unwrap();
    let o = tole(&d, &["config", "check"]);
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out} {}", text(&o.stderr));
    assert!(out.contains("config.toml"), "{out}");
    assert!(out.contains("model = \"m1\"  [config]"), "{out}");
    assert!(out.contains("plan_mode = true  [config]"), "{out}");
    assert!(out.contains("  - \"internal\""), "{out}");
    assert!(out.contains("mission.max_steps = 7  [config]"), "{out}");
    assert!(
        !out.contains("base_url"),
        "unset keys are not listed: {out}"
    );
}

#[test]
fn invalid_config_exits_nonzero_with_path_line_col() {
    let d = tmpdir();
    let f = d.join("bad.toml");
    std::fs::write(&f, "model = \"m\"\nplan_mode = \"yes\"\n").unwrap();
    let o = tole(&d, &["config", "check", "--config", f.to_str().unwrap()]);
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(err.contains("bad.toml:2:"), "{err}");
}

#[test]
fn secret_key_exits_nonzero_naming_it() {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(".tole/config.toml"), "api_key = \"sk-x\"\n").unwrap();
    let o = tole(&d, &["config", "check"]);
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(err.contains("api_key"), "{err}");
    assert!(
        err.contains("secrets must never live in a config file"),
        "{err}"
    );
    assert!(
        !err.contains("sk-x"),
        "the value must never be echoed: {err}"
    );
}

#[test]
fn missing_explicit_config_is_an_error() {
    let d = tmpdir();
    let o = tole(&d, &["config", "check", "--config", "nope.toml"]);
    assert!(!o.status.success());
}
