//! `tole sandbox list|show|set` (#810) on the real binary: profiles and
//! bindings live in the USER config only, `set` keeps the rest of the file,
//! and a project config can never change the effective profile.

mod common;
use common::TestDir;
use std::path::Path;
use std::process::Output;

const USER: &str = "# my settings\nmodel = \"m\"\n\n[sandbox]\nmode = \"required\"\n\
    # keep this comment\n\n[sandbox.profiles.net]\nnetwork = \"host\"\n";

/// A home (user config) and a project dir with its own config.
fn setup(tag: &str, user: &str) -> TestDir {
    let base = TestDir::new(&format!("tole-sbxcli-{tag}"));
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("proj/.tole")).unwrap();
    std::fs::write(base.join("home/tole/config.toml"), user).unwrap();
    std::fs::write(base.join("proj/.tole/config.toml"), "model = \"p\"\n").unwrap();
    base
}

fn tole(base: &Path, args: &[&str]) -> Output {
    common::hermetic_command(env!("CARGO_BIN_EXE_tole"), base.join("home").as_path(), &[])
        .args(args)
        .current_dir(base.join("proj"))
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_UNDER_SESSION")
        .env_remove("TOLE_AGENT_DEPTH")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn user(base: &Path) -> String {
    std::fs::read_to_string(base.join("home/tole/config.toml")).unwrap()
}

#[test]
fn set_binds_a_server_keeps_the_file_and_show_and_list_report_it() {
    let b = setup("set", USER);
    let o = tole(&b, &["sandbox", "set", "github", "--profile", "net"]);
    assert!(o.status.success(), "{}", text(&o));
    let after = user(&b);
    assert!(
        after.starts_with(USER),
        "the rest of the file is kept:\n{after}"
    );
    assert!(
        after.contains("[sandbox.servers.github]\nprofile = \"net\"\n"),
        "{after}"
    );
    let o = tole(&b, &["sandbox", "show", "github"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("profile: net [user config [sandbox.servers]]"),
        "{out}"
    );
    assert!(out.contains("network host"), "{out}");
    // A connection key is reduced to the server name.
    let out = text(&tole(&b, &["sandbox", "show", "github@/some/ws"]));
    assert!(out.contains("server: github"), "{out}");
    // Another server keeps the default.
    let out = text(&tole(&b, &["sandbox", "show", "other"]));
    assert!(out.contains("profile: server [default]"), "{out}");
    let out = text(&tole(&b, &["sandbox", "list"]));
    assert!(out.contains("sandbox mode: required"), "{out}");
    assert!(out.contains("strict (built in)"), "{out}");
    assert!(out.contains("  net: workspace read, network host"), "{out}");
    assert!(out.contains("  github -> net"), "{out}");
}

#[test]
fn set_again_edits_the_table_in_place_and_a_repeat_is_a_no_op() {
    let b = setup("again", USER);
    for p in ["net", "strict"] {
        assert!(tole(&b, &["sandbox", "set", "gh", "--profile", p])
            .status
            .success());
    }
    let after = user(&b);
    assert_eq!(after.matches("[sandbox.servers.gh]").count(), 1, "{after}");
    assert!(after.contains("profile = \"strict\""), "{after}");
    assert!(!after.contains("profile = \"net\""), "{after}");
    let o = tole(&b, &["sandbox", "set", "gh", "--profile", "strict"]);
    assert!(text(&o).contains("already bound"), "{}", text(&o));
    assert_eq!(user(&b), after);
}

#[test]
fn set_refuses_an_unknown_profile_and_writes_nothing() {
    let b = setup("unknown", USER);
    let o = tole(&b, &["sandbox", "set", "gh", "--profile", "nope"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("unknown sandbox profile"), "{}", text(&o));
    assert_eq!(user(&b), USER);
}

#[test]
fn set_creates_the_user_config_when_there_is_none() {
    let b = setup("fresh", "");
    std::fs::remove_file(b.join("home/tole/config.toml")).unwrap();
    let o = tole(&b, &["sandbox", "set", "gh", "--profile", "strict"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("mode is off"), "{}", text(&o));
    assert!(user(&b).contains("[sandbox.servers.gh]"));
}

#[test]
fn set_refuses_under_an_agent_session() {
    let b = setup("session", USER);
    let o = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), b.join("home").as_path(), &[])
        .args(["sandbox", "set", "gh", "--profile", "strict"])
        .current_dir(b.join("proj"))
        .env("TOLE_UNDER_SESSION", "1")
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(text(&o).contains("owner decision"), "{}", text(&o));
    assert_eq!(user(&b), USER);
}

#[test]
fn a_project_config_cannot_change_the_effective_profile() {
    let b = setup("project", USER);
    assert!(tole(&b, &["sandbox", "set", "gh", "--profile", "strict"])
        .status
        .success());
    std::fs::write(
        b.join("proj/.tole/config.toml"),
        "[sandbox.profiles.wide]\nnetwork = \"host\"\nworkspace = \"write\"\n\
         [sandbox.servers.gh]\nprofile = \"wide\"\n",
    )
    .unwrap();
    // `show` and `list` read the user config only.
    let out = text(&tole(&b, &["sandbox", "show", "gh"]));
    assert!(out.contains("profile: strict"), "{out}");
    // `config check` shows the effective profile and marks the project lines ignored.
    let out = text(&tole(&b, &["config", "check"]));
    assert!(
        out.contains(
            "sandbox effective: server \"gh\" profile \"strict\" [user config [sandbox.servers]]"
        ),
        "{out}"
    );
    let line = out
        .lines()
        .find(|l| l.starts_with("sandbox.servers.gh.profile = \"wide\""))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(line.contains("IGNORED"), "{line}");
}
