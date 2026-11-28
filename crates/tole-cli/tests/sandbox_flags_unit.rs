//! `tole_cli::sandbox_flags` (#601): what the sandbox flags and the config make of a run. Moved
//! from the unit tests of `src/sandbox_flags.rs` (#1320) with the test names unchanged.

use std::path::Path;
use tole_cli::config::SandboxConfig;
use tole_cli::sandbox_flags::*;
use tole_core::sandbox::flags::Override;
use tole_core::sandbox::{Mode, Network};

fn cfg(src: &str) -> SandboxConfig {
    tole_cli::config::parse(src, Path::new("config.toml"))
        .unwrap()
        .sandbox
        .unwrap_or_default()
}

const USER: &str = "[sandbox]\nmode = \"required\"\n[sandbox.profiles.net]\nnetwork = \"host\"\n\
    [sandbox.servers.gh]\nprofile = \"net\"\n";

fn ctx() -> Context {
    Context {
        under_session: false,
        unattended_command: false,
        meaningless: false,
        tty: true,
    }
}

fn no() -> Flags {
    Flags {
        sandbox: None,
        no_sandbox: true,
    }
}

fn prof(n: &str) -> Flags {
    Flags {
        sandbox: Some(n.into()),
        no_sandbox: false,
    }
}

fn run(f: &Flags, src: &str, c: Context) -> Result<Option<Outcome>, String> {
    let s = cfg(src);
    let p = s.policy().ok();
    apply(f, p.as_ref(), Some(&s), &c)
}

#[test]
fn no_flag_changes_nothing() {
    assert_eq!(run(&Flags::default(), USER, ctx()), Ok(None));
}

#[test]
fn no_sandbox_at_a_terminal_is_honoured_and_records_the_configured_mode() {
    let o = run(&no(), USER, ctx()).unwrap().unwrap();
    assert_eq!(
        o.over,
        Override::NoSandbox {
            configured: Mode::Required
        }
    );
}

#[test]
fn no_sandbox_is_a_no_op_when_the_sandbox_is_off() {
    assert_eq!(run(&no(), "model = \"m\"\n", ctx()), Ok(None));
    assert_eq!(run(&no(), "[sandbox]\nmode = \"off\"\n", ctx()), Ok(None));
}

#[test]
fn no_sandbox_is_refused_for_unattended_runs_in_required_and_optional() {
    for mode in ["required", "optional"] {
        let src = format!("[sandbox]\nmode = \"{mode}\"\n");
        // serve / acp
        let c = Context {
            unattended_command: true,
            ..ctx()
        };
        let e = run(&no(), &src, c).unwrap_err();
        assert!(e.contains("unattended") && e.contains(mode), "{e}");
        // a script: no terminal
        let c = Context {
            tty: false,
            ..ctx()
        };
        assert!(run(&no(), &src, c).unwrap_err().contains("unattended"));
    }
}

/// #846: a profile can be wider than the configured one, so `--sandbox` has
/// the same terminal test as `--no-sandbox`, in every mode where it is
/// otherwise valid.
#[test]
fn sandbox_is_refused_without_a_terminal_and_for_unattended_commands() {
    let unattended = Context {
        unattended_command: true,
        ..ctx()
    };
    let no_tty = Context {
        tty: false,
        ..ctx()
    };
    let optional = "[sandbox]\nmode = \"optional\"\n[sandbox.profiles.net]\nnetwork = \"host\"\n";
    for src in [USER, optional] {
        for c in [unattended, no_tty] {
            for name in ["net", "strict"] {
                let e = run(&prof(name), src, c).unwrap_err();
                assert!(
                    e.contains("--sandbox is refused in an unattended run"),
                    "{e}"
                );
            }
        }
    }
    // The same flag at a terminal is accepted.
    assert!(run(&prof("net"), USER, ctx()).unwrap().is_some());
}

#[test]
fn both_flags_are_refused_under_an_agent_session_even_when_attended() {
    let c = Context {
        under_session: true,
        ..ctx()
    };
    for f in [no(), prof("strict")] {
        let e = run(&f, USER, c).unwrap_err();
        assert!(e.contains("agent session") && e.contains("#684"), "{e}");
    }
    // Also when the sandbox is off: an agent never passes the flag.
    assert!(run(&no(), "model = \"m\"\n", c).is_err());
}

#[test]
fn both_flags_are_refused_where_no_server_starts() {
    let c = Context {
        meaningless: true,
        ..ctx()
    };
    assert!(run(&no(), USER, c).unwrap_err().contains("not accepted"));
    assert!(run(&prof("strict"), USER, c).is_err());
}

#[test]
fn sandbox_selects_a_builtin_or_user_profile_for_every_server() {
    let o = run(&prof("net"), USER, ctx()).unwrap().unwrap();
    let Override::Profile {
        name,
        profile,
        configured,
    } = o.over
    else {
        panic!("a profile override");
    };
    assert_eq!((name.as_str(), configured.as_str()), ("net", "server"));
    assert_eq!(profile.network, Network::Host);
    let Override::Profile { name, .. } = run(&prof("strict"), USER, ctx()).unwrap().unwrap().over
    else {
        panic!("a profile override");
    };
    assert_eq!(name, "strict");
}

#[test]
fn sandbox_cannot_name_an_unknown_profile_or_turn_the_sandbox_on() {
    let e = run(&prof("nope"), USER, ctx()).unwrap_err();
    assert!(e.contains("unknown sandbox profile"), "{e}");
    for src in ["model = \"m\"\n", "[sandbox]\nmode = \"off\"\n"] {
        let e = run(&prof("strict"), src, ctx()).unwrap_err();
        assert!(e.contains("does not turn the sandbox on"), "{e}");
    }
    // No config at all.
    assert!(apply(&prof("strict"), None, None, &ctx()).is_err());
}

#[test]
fn a_project_config_cannot_define_the_profile_a_flag_selects() {
    let project = tole_cli::config::parse(
        "[sandbox.profiles.loose]\nworkspace = \"write\"\nnetwork = \"host\"\n",
        Path::new("p.toml"),
    )
    .unwrap();
    let user = tole_cli::config::parse(USER, Path::new("u.toml")).unwrap();
    let layered = tole_cli::config::layered(&project, &user);
    let s = layered.sandbox.unwrap_or_default();
    let p = s.policy().ok();
    let e = apply(&prof("loose"), p.as_ref(), Some(&s), &ctx()).unwrap_err();
    assert!(e.contains("unknown sandbox profile"), "{e}");
}
