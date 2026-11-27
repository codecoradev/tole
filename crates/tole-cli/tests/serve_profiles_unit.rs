//! Unit tests of the serve profiles (moved out of `src/`, #1249: they use only the public API).

use tole_cli::serve_support::profiles::{ProfileSource, SessionProfile};
use tole_core::memory::MemoryConfig;

fn src(allow: &[&str], trust: &[&str], pinned: bool) -> ProfileSource {
    ProfileSource {
        config: None,
        allow: allow.iter().map(|s| s.to_string()).collect(),
        trust: trust.iter().map(|s| s.to_string()).collect(),
        allow_pinned: pinned,
    }
}

fn v(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_profile_allow_replaces_the_daemon_list_unless_the_flag_pins_it() {
    let open = src(&["env_tool"], &["todo_write"], false);
    assert_eq!(
        open.effective_allow(&v(&["write_file"])),
        v(&["write_file", "todo_write"])
    );
    // A profile without an allow list leaves the daemon's.
    assert_eq!(open.effective_allow(&[]), v(&["env_tool", "todo_write"]));
    // `--allow` pins: the profile can never widen it.
    let pinned = src(&["read_only_ok"], &[], true);
    assert_eq!(
        pinned.effective_allow(&v(&["write_file", "run_command"])),
        v(&["read_only_ok"])
    );
    assert_eq!(open.default_allow(), v(&["env_tool", "todo_write"]));
}

#[test]
fn a_hostile_name_is_refused_before_any_lookup() {
    let s = src(&[], &[], false);
    for bad in [
        "",
        "-x",
        "A",
        "a b",
        "../x",
        "x\ny",
        &"a".repeat(65),
        "a\u{1b}[31m",
    ] {
        let e = s.select(bad).unwrap_err();
        assert_eq!(e.status(), 400, "{bad:?}");
        assert!(
            !e.message().contains(bad) || bad.is_empty(),
            "echoed {bad:?}"
        );
    }
}

#[test]
fn no_config_means_no_profiles() {
    let e = src(&[], &[], false).select("kai").unwrap_err();
    assert_eq!(e.status(), 400);
    assert!(e.message().contains("no profiles"));
}

#[test]
fn the_memory_room_follows_the_profile_but_never_enables_memory() {
    let p = SessionProfile {
        name: "kai".into(),
        system_prompt: None,
        model: None,
        memory_room: "profile-kai".into(),
        allow: vec![],
    };
    assert!(p.memory(None, None).is_none());
    let base = MemoryConfig {
        bin: "uteke".into(),
        room: "repo-x".into(),
        limit: 3,
    };
    assert_eq!(p.memory(Some(&base), None).unwrap().room, "profile-kai");
    assert_eq!(
        p.memory(Some(&base), Some(" ")).unwrap().room,
        "profile-kai"
    );
    assert_eq!(
        p.memory(Some(&base), Some("mailbox")).unwrap().room,
        "repo-x"
    );
}
