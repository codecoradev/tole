//! `--sandbox <profile>` and `--no-sandbox` (#750) on the real binary: every
//! refusal path, and the doctor line that says a flag overrode the config.
//! The test process has no terminal, so it is an unattended run.

mod common;
use common::TestDir;
use std::path::Path;
use std::process::{Command, Output};

const REQUIRED: &str =
    "[sandbox]\nmode = \"required\"\n[sandbox.profiles.net]\nnetwork = \"host\"\n";

fn setup(tag: &str, user: &str) -> TestDir {
    let base = TestDir::new(&format!("tole-sbxflag-{tag}"));
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("proj")).unwrap();
    std::fs::write(base.join("home/tole/config.toml"), user).unwrap();
    base
}

fn tole(base: &Path, session: bool, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(base.join("proj"))
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("TOLE_AGENT_DEPTH")
        .env_remove("TOLE_UNDER_SESSION")
        .stdin(std::process::Stdio::null());
    if session {
        c.env("TOLE_UNDER_SESSION", "1");
    }
    c.output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn no_sandbox_is_refused_without_a_terminal_and_under_a_session() {
    let b = setup("no", REQUIRED);
    let o = tole(&b, false, &["--no-sandbox", "doctor"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("refused in an unattended run"), "{out}");
    let o = tole(&b, true, &["--no-sandbox", "doctor"]);
    let out = text(&o);
    assert!(
        !o.status.success() && out.contains("agent session"),
        "{out}"
    );
    // `serve` (a `shell-tools` subcommand) is unattended even at a terminal:
    // the same refusal, before it binds.
    #[cfg(feature = "shell-tools")]
    {
        let out = text(&tole(&b, false, &["--no-sandbox", "serve"]));
        assert!(out.contains("refused in an unattended run"), "{out}");
    }
}

#[test]
fn no_sandbox_with_the_sandbox_off_changes_nothing() {
    let b = setup("off", "model = \"m\"\n");
    let out = text(&tole(&b, false, &["--no-sandbox", "doctor"]));
    assert!(!out.contains("refused"), "{out}");
    assert!(!out.contains("--no-sandbox overrides"), "{out}");
}

#[test]
fn sandbox_flag_is_refused_for_a_script_without_a_terminal() {
    // The harness has no terminal: exactly a script's situation (#846). A
    // valid user-defined profile is refused (before the fix the run
    // succeeded and doctor printed "overrides the config").
    let b = setup("sel", REQUIRED);
    for profile in ["net", "strict"] {
        let o = tole(&b, false, &["--sandbox", profile, "doctor"]);
        let out = text(&o);
        assert!(!o.status.success(), "{out}");
        assert!(
            out.contains("--sandbox is refused in an unattended run"),
            "{out}"
        );
        assert!(!out.contains("overrides the config"), "{out}");
    }
    // `serve` is unattended even at a terminal: refused before it binds.
    #[cfg(feature = "shell-tools")]
    {
        let out = text(&tole(&b, false, &["--sandbox", "net", "serve"]));
        assert!(
            out.contains("--sandbox is refused in an unattended run"),
            "{out}"
        );
    }
    let o = tole(&b, false, &["--sandbox", "nope", "doctor"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("unknown sandbox profile"), "{}", text(&o));
}

#[test]
fn sandbox_flag_cannot_turn_the_sandbox_on_or_run_under_a_session() {
    let b = setup("on", "model = \"m\"\n");
    let o = tole(&b, false, &["--sandbox", "strict", "doctor"]);
    assert!(!o.status.success());
    assert!(
        text(&o).contains("does not turn the sandbox on"),
        "{}",
        text(&o)
    );
    let b = setup("sess", REQUIRED);
    let o = tole(&b, true, &["--sandbox", "strict", "doctor"]);
    assert!(!o.status.success() && text(&o).contains("agent session"));
}

#[test]
fn the_two_flags_conflict_and_cron_and_sandbox_commands_refuse_them() {
    let b = setup("conf", REQUIRED);
    let o = tole(
        &b,
        false,
        &["--no-sandbox", "--sandbox", "strict", "doctor"],
    );
    assert!(!o.status.success());
    // `tole plugin` probes and approves servers: never under a run flag.
    #[cfg(feature = "mcp")]
    for flag in [&["--no-sandbox"][..], &["--sandbox", "strict"][..]] {
        let mut args = flag.to_vec();
        args.extend(["plugin", "list"]);
        let o = tole(&b, false, &args);
        assert!(!o.status.success(), "{}", text(&o));
        assert!(text(&o).contains("not accepted here"), "{}", text(&o));
    }
    let o = tole(&b, false, &["--sandbox", "strict", "sandbox", "list"]);
    assert!(text(&o).contains("starts no MCP server"), "{}", text(&o));
}
