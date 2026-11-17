//! #740: `tole sessions` shows the serve profile a session recorded
//! (`fact/profile`), and `tole doctor` lists the user config's profiles by
//! name. Real binary, hermetic home.

mod common;
use common::TestDir;
use serde_json::json;
use std::path::Path;
use std::process::{Command, Stdio};
use tole_core::register::RegisterWrite;
use tole_core::storage::{Commit, JsonlStorage, Storage};

fn tole(args: &[&str], cwd: &Path, home: &Path) -> (String, bool) {
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(cwd)
        .env("CODECORA_HOME", home)
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .stdin(Stdio::null())
        .output()
        .expect("run tole");
    (
        String::from_utf8_lossy(&o.stdout).to_string(),
        o.status.success(),
    )
}

fn session(dir: &Path, id: &str, profile: Option<serde_json::Value>) {
    std::fs::create_dir_all(dir).unwrap();
    let mut s = JsonlStorage::create(dir, id, None).unwrap();
    if let Some(v) = profile {
        s.commit(Commit::new().register(RegisterWrite::set("fact", "profile", v)))
            .unwrap();
    }
}

#[test]
fn sessions_show_the_recorded_profile_and_stay_unchanged_without_one() {
    let work = TestDir::new("tole-sess-profile-work");
    let home = TestDir::new("tole-sess-profile-home");
    let dir = work.join(".tole/sessions");
    session(&dir, "plain", None);
    let (before, ok) = tole(&["sessions"], &work, &home);
    assert!(ok, "{before}");
    // Golden: header and row, byte for byte (the mtime tail is the clock).
    let mut lines = before.lines();
    assert_eq!(
        lines.next().unwrap(),
        "session                    name               pc             seq  turns  mtime",
        "{before}"
    );
    let row = lines.next().unwrap();
    let (cols, mtime) = row.split_at(row.len() - 16);
    assert_eq!(
        cols, "plain                      -                  Idle             0      0  ",
        "{before}"
    );
    assert!(mtime.starts_with("20"), "{before}");
    assert_eq!(lines.next(), None, "{before}");

    session(&dir, "with-kai", Some(json!({"name": "kai", "atMs": 1})));
    // A tampered name is a placeholder, never echoed.
    session(
        &dir,
        "tampered",
        Some(json!({"name": "\u{1b}[31mevil\n../x", "atMs": 1})),
    );
    let (after, ok) = tole(&["sessions"], &work, &home);
    assert!(ok, "{after}");
    let row = |id: &str| {
        after
            .lines()
            .find(|l| l.starts_with(id))
            .unwrap()
            .to_string()
    };
    assert!(after.lines().next().unwrap().contains("profile"), "{after}");
    assert!(row("with-kai").contains(" kai "), "{after}");
    assert!(row("tampered").contains(" ? "), "{after}");
    // The profile cell is its own column: 16 wide, between turns and mtime.
    let cell = |id: &str| {
        let r = row(id);
        r[r.len() - 33..r.len() - 17].trim_end().to_string()
    };
    assert_eq!(cell("with-kai"), "kai", "{after}");
    assert_eq!(cell("tampered"), "?", "{after}");
    assert_eq!(cell("plain"), "-", "{after}");
    assert_eq!(
        after.lines().next().unwrap().len(),
        row("plain").len() - 16 + "mtime".len(),
        "{after}"
    );
    assert!(
        !after.contains("evil") && !after.contains('\u{1b}'),
        "{after:?}"
    );
}

#[test]
fn doctor_lists_serve_profiles_by_name_and_is_silent_without_any() {
    let work = TestDir::new("tole-doc-profile-work");
    let home = TestDir::new("tole-doc-profile-home");
    let args = ["doctor", "--tier", "offline", "--no-ledger"];
    let (none, ok) = tole(&args, &work, &home);
    assert!(ok, "{none}");
    assert!(!none.contains("serve profiles"), "{none}");

    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(
        home.join("tole/config.toml"),
        "[profiles.kai]\nsystem_prompt_file = \"PERSONA-TEXT.md\"\nallow = [\"write_file\"]\n\
         [profiles.ana]\nmemory_room = \"research\"\n",
    )
    .unwrap();
    let (out, ok) = tole(&args, &work, &home);
    assert!(ok, "{out}");
    assert!(
        out.contains(
            "serve profiles: ana (room research, 0 allow), kai (room profile-kai, 1 allow)"
        ),
        "{out}"
    );
    assert!(
        !out.contains("PERSONA-TEXT") && !out.contains("write_file"),
        "{out}"
    );
}
