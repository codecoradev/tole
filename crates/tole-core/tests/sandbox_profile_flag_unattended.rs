//! #846: a `--sandbox <profile>` override that slipped past the host check
//! (a script, no terminal) is ignored once the process is marked
//! unattended, like `--no-sandbox`: the session view is the configured
//! policy again. Its own test binary, so the process-wide statics (the
//! override, the unattended mark) start clean.

#![cfg(unix)]

use tole_core::sandbox::flags::{self, Override};
use tole_core::sandbox::{self, Mode, Network, Policy, Profile};

#[test]
fn an_unattended_process_ignores_the_profile_flag() {
    let configured = Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile: sandbox::builtin_profile("server").unwrap(),
        servers: Default::default(),
        skip: vec![],
        max_servers: 4,
        idle_secs: 60,
    };
    sandbox::set_policy(Some(configured.clone())).unwrap();
    flags::set(Override::Profile {
        name: "wide".into(),
        profile: Profile {
            network: Network::Host,
            ..sandbox::builtin_profile("server").unwrap()
        },
        configured: "server".into(),
    })
    .unwrap();
    // Attended: the flag applies.
    let s = sandbox::session_policy().unwrap();
    assert_eq!(
        (s.profile_name.as_str(), s.profile.network),
        ("wide", Network::Host)
    );
    assert!(flags::note().unwrap().contains("overrides the config"));

    sandbox::mark_unattended();
    assert_eq!(sandbox::session_policy(), Some(configured));
    assert!(flags::note().unwrap().contains("ignored in an unattended"));
}
