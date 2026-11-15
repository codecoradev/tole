//! #750: `--no-sandbox` changes only what this run's stdio MCP servers start
//! under (`sandbox::session_policy`), and only while the run is attended.
//! The configured policy, the `plugin add` probe's launch and the command
//! consumers never see it, and an unattended process ignores it. One
//! process, one order: statics are process-wide.

#![cfg(unix)]

#[path = "support/test_dir.rs"]
mod test_dir;
use std::path::PathBuf;
use test_dir::TestDir;
use tole_core::sandbox::command::{self, Consumer, Consumers};
use tole_core::sandbox::flags::{self, Override};
use tole_core::sandbox::{self, Launch, Mode, Policy};

fn required() -> Policy {
    Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile: sandbox::builtin_profile("server").unwrap(),
        servers: Default::default(),
        skip: vec!["mine".into()],
        max_servers: 4,
        idle_secs: 60,
    }
}

/// True when `command_for` hands back the bare program: the consumer is not
/// sandboxed. A refusal or a wrapper (`bwrap ...`) is false.
fn runs_bare(ws: &std::path::Path) -> bool {
    match command::command_for(Consumer::RunCommand, ws, "echo", &["hi".to_string()]) {
        Ok(c) => c.get_program() == "echo",
        Err(_) => false,
    }
}

#[test]
fn the_flag_reaches_only_the_session_servers_and_never_an_unattended_run() {
    let base = TestDir::new("tole-noflag");
    let ws: PathBuf = std::fs::canonicalize(&*base).unwrap();
    sandbox::set_policy(Some(required())).unwrap();
    command::set_consumers(Consumers {
        run_command: Mode::Required,
        job_start: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
    assert!(flags::note().is_none());
    assert_eq!(sandbox::session_policy(), sandbox::policy());
    assert!(
        !runs_bare(&ws),
        "baseline: the consumer is sandboxed or refused"
    );

    flags::set(Override::NoSandbox {
        configured: Mode::Required,
    })
    .unwrap();
    assert!(
        flags::set(Override::Profile {
            name: "strict".into(),
            profile: sandbox::builtin_profile("strict").unwrap(),
            configured: "server".into()
        })
        .is_err(),
        "a different override is refused"
    );

    // Attended: the session's servers see the sandbox as off...
    assert!(sandbox::session_policy().is_none(), "the flag is honoured");
    assert!(matches!(
        sandbox::launch_session("srv", None, "true", &[]),
        Launch::Direct { .. }
    ));
    // ...and nothing else does: the configured policy (first-party names),
    // the probe's launch and the command consumers are unchanged. This
    // fails if one of them is routed through the flag-affected view.
    let configured = sandbox::policy().expect("the configured policy is intact");
    assert_eq!(
        (configured.mode, configured.skip),
        (Mode::Required, vec!["mine".into()])
    );
    assert!(
        !matches!(
            sandbox::launch("srv", None, "true", &[]),
            Launch::Direct { .. }
        ),
        "the probe launch ignores the flag"
    );
    assert!(!runs_bare(&ws), "command consumers ignore the flag");
    #[cfg(feature = "mcp")]
    {
        // Connection keys follow the session view: a plain name.
        assert_eq!(tole_core::mcp::conn_key_for("srv", Some(&ws)), "srv");
    }
    assert!(flags::note().unwrap().contains("--no-sandbox overrides"));

    // Unattended: the session view is the configured policy again.
    sandbox::mark_unattended();
    assert_eq!(sandbox::session_policy(), sandbox::policy());
    assert!(flags::note().unwrap().contains("ignored in an unattended"));
    let launch = sandbox::launch_session("srv", None, "true", &[]);
    assert!(
        !matches!(launch, Launch::Direct { .. }),
        "never unsandboxed for an unattended run: {launch:?}"
    );
    assert!(!runs_bare(&ws));
}
