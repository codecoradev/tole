//! #750: `--sandbox <profile>` replaces the profile only in the session view
//! (`sandbox::session_policy`). The configured policy, the probe's launch
//! and the command consumers keep what the owner configured.

#![cfg(unix)]

#[path = "support/test_dir.rs"]
mod test_dir;
use test_dir::TestDir;
use tole_core::sandbox::command::{self, Consumer, Consumers};
use tole_core::sandbox::flags::{self, Override};
use tole_core::sandbox::{self, Mode, Network, Policy, Profile, ServerProfile};

fn host_profile() -> Profile {
    Profile {
        network: Network::Host,
        ..sandbox::builtin_profile("server").unwrap()
    }
}

#[test]
fn the_profile_flag_changes_only_the_session_view() {
    let base = TestDir::new("tole-profflag");
    let ws = std::fs::canonicalize(&*base).unwrap();
    let mut servers = std::collections::BTreeMap::new();
    servers.insert(
        "gh".to_string(),
        ServerProfile {
            name: "wide".into(),
            profile: host_profile(),
        },
    );
    let configured = Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile: sandbox::builtin_profile("server").unwrap(),
        servers,
        skip: vec!["mine".into()],
        max_servers: 4,
        idle_secs: 60,
    };
    sandbox::set_policy(Some(configured.clone())).unwrap();
    command::set_consumers(Consumers {
        run_command: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
    flags::set(Override::Profile {
        name: "strict".into(),
        profile: sandbox::builtin_profile("strict").unwrap(),
        configured: "server".into(),
    })
    .unwrap();

    // The session's servers: the flag's profile, bindings dropped.
    let s = sandbox::session_policy().unwrap();
    assert_eq!(s.profile_name, "strict");
    assert!(s.servers.is_empty());
    assert_eq!((s.mode, &s.skip), (Mode::Required, &configured.skip));

    // The configured policy is untouched (bindings, profile, skip).
    assert_eq!(sandbox::policy(), Some(configured));

    // The command consumers do not read it: with `required`, no sandbox is
    // refused or wrapped, whatever profile the flag picked.
    let c = command::command_for(Consumer::RunCommand, &ws, "echo", &["x".to_string()]);
    assert!(c.as_ref().map_or(true, |c| c.get_program() != "echo"));
}
