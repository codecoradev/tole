//! #864: a user config that is missing, unreadable or never looked for must not
//! look the same as "no user config". Real binary, `tole sessions` (no
//! provider needed; it loads the user config like every non-skip command).
//!
//! * A path that cannot be inspected (here: a parent that is a file) is a
//!   startup error naming the path, never silent defaults (option A).
//! * An absent file prints one notice on stderr when the situation is unusual:
//!   the root comes from `CODECORA_HOME`, the data directory cannot be
//!   resolved, the run is under an agent session, or it is an unattended face
//!   (option B). An owner on the default path with no config stays silent, and
//!   so does a `cron tick` there (an OS cron mails stderr).
#![cfg(unix)]

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

/// `tole <args>` with a scrubbed environment: only what the test sets.
fn tole(work: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(work)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    c.spawn().unwrap().wait_with_output().unwrap()
}

#[test]
fn an_uninspectable_user_config_path_fails_closed_naming_the_path() {
    let root = TestDir::new("tole-ucfg-abs");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // CODECORA_HOME is a regular FILE: `<file>/tole/config.toml` cannot be
    // inspected (ENOTDIR), which `Path::exists` reports as "not there".
    let file = root.join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let o = tole(
        &work,
        &[("CODECORA_HOME", file.to_str().unwrap())],
        &["sessions"],
    );
    let err = text(&o.stderr);
    assert!(!o.status.success(), "must not run on defaults: {err}");
    assert!(
        err.contains("user config") && err.contains("not-a-dir"),
        "the error names the path: {err}"
    );
}

#[test]
fn an_absent_config_under_a_codecora_home_override_says_so() {
    let root = TestDir::new("tole-ucfg-ovr");
    let (work, home) = (root.join("work"), root.join("home"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let o = tole(
        &work,
        &[("CODECORA_HOME", home.to_str().unwrap())],
        &["sessions"],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert!(
        err.contains("no user config at") && err.contains("use their defaults"),
        "{err}"
    );
}

#[test]
fn an_absent_config_under_an_agent_session_says_so() {
    let root = TestDir::new("tole-ucfg-sess");
    let (work, home) = (root.join("work"), root.join("home"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    // Default root (HOME), but the marker says an agent session started us.
    let o = tole(
        &work,
        &[
            ("HOME", home.to_str().unwrap()),
            ("TOLE_UNDER_SESSION", "1"),
        ],
        &["sessions"],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("no user config at"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn an_unresolvable_data_directory_says_so() {
    let root = TestDir::new("tole-ucfg-nohome");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // No HOME, no CODECORA_HOME: the data directory cannot be resolved.
    let o = tole(&work, &[], &["sessions"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert!(
        err.contains("cannot resolve") && err.contains("use their defaults"),
        "{err}"
    );
}

/// The other side: an owner on the default path with no config, and a cron
/// tick there, say nothing (no mail per minute).
#[cfg(feature = "shell-tools")]
#[test]
fn the_default_root_without_a_config_is_silent_including_cron_tick() {
    let root = TestDir::new("tole-ucfg-quiet");
    let (work, home) = (root.join("work"), root.join("home"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let cron = root.join("cron");
    let env = [
        ("HOME", home.to_str().unwrap()),
        ("TOLE_CRON_DIR", cron.to_str().unwrap()),
    ];
    for args in [&["sessions"][..], &["cron", "tick"]] {
        let o = tole(&work, &env, args);
        assert!(o.status.success(), "{args:?}: {}", text(&o.stderr));
        assert_eq!(text(&o.stderr), "", "{args:?} must be silent");
    }
}

/// A valid config at the default root: silent too (the line announcing it is
/// the existing behaviour; `cron tick` drops it without a terminal, #868).
#[cfg(feature = "shell-tools")]
#[test]
fn a_valid_config_at_the_default_root_makes_cron_tick_silent() {
    let root = TestDir::new("tole-ucfg-valid");
    let (work, home) = (root.join("work"), root.join("home"));
    std::fs::create_dir_all(work.clone()).unwrap();
    std::fs::create_dir_all(home.join(".codecora/tole")).unwrap();
    std::fs::write(
        home.join(".codecora/tole/config.toml"),
        "[sandbox.consumers]\nnetwork = \"none\"\n",
    )
    .unwrap();
    let cron = root.join("cron");
    let o = tole(
        &work,
        &[
            ("HOME", home.to_str().unwrap()),
            ("TOLE_CRON_DIR", cron.to_str().unwrap()),
        ],
        &["cron", "tick"],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stderr), "");
}
