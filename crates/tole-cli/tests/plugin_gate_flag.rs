//! #750: the plugin gate compares the profile a server will ACTUALLY run
//! under (the session view, with the run's `--sandbox` / `--no-sandbox`
//! override applied) with the approved one. Each case needs its own process
//! (the policy and the override are set once), so each is its own test
//! binary function guarded by a process-wide `Once`-style set: this file
//! holds the `--sandbox <profile>` case, `plugin_gate_no_sandbox_flag.rs`
//! the other.

mod common;
use common::TestDir;
use tole_cli::plugin_gate::{Approval, Gate};
use tole_core::plugin_lock::ManifestTool;
use tole_core::sandbox::flags::{self, Override};
use tole_core::sandbox::{self, Mode, Policy};

fn usable() -> tole_core::sandbox::Availability {
    tole_core::sandbox::Availability::Usable {
        bwrap: "/usr/bin/bwrap".into(),
        version: "0.9.0".into(),
    }
}

fn tools() -> Vec<ManifestTool> {
    vec![ManifestTool {
        name: "search".into(),
        description: "Search.".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }]
}

fn configured() -> Policy {
    Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile: sandbox::builtin_profile("server").unwrap(),
        servers: Default::default(),
        skip: Vec::new(),
        max_servers: 4,
        idle_secs: 60,
    }
}

#[test]
fn a_sandbox_flag_profile_cannot_run_an_approved_server_without_a_block() {
    let dir = TestDir::new("tole-gate-flag-prof");
    let user = dir.join("plugins.lock");
    let ws = TestDir::new("tole-gate-flag-prof-ws");
    sandbox::set_policy(Some(configured())).unwrap();
    // Approved under the configured profile (what `tole plugin approve` sees).
    let id = configured().applied_profile("srv", &usable()).unwrap();
    let mut gate = Gate::load(Some(user.clone()), &ws).with_backend(usable());
    assert_eq!(gate.live_profile("srv", "stdio:srv").unwrap(), id);
    gate.approve_with(
        "srv",
        "stdio:srv",
        "1",
        &tools(),
        Approval {
            sandbox: Some(id.name),
            sandbox_hash: Some(id.hash),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(Gate::load(Some(user.clone()), &ws)
        .with_backend(usable())
        .refuse_unseen("srv", "stdio:srv")
        .is_none());
    // `--sandbox strict`: the server would now start under another profile.
    flags::set(Override::Profile {
        name: "strict".into(),
        profile: sandbox::builtin_profile("strict").unwrap(),
        configured: "server".into(),
    })
    .unwrap();
    let gate = Gate::load(Some(user), &ws).with_backend(usable());
    assert_eq!(
        gate.live_profile("srv", "stdio:srv").unwrap().name,
        "strict"
    );
    let note = gate.refuse_unseen("srv", "stdio:srv").unwrap();
    assert!(note.contains("sandbox profile"), "{note}");
    assert!(gate.check("srv", "stdio:srv", "1", &tools()).is_err());
    // The configured policy is what approvals and the first-party list read.
    assert_eq!(sandbox::policy().unwrap().profile_name, "server");
}
