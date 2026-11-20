//! #750: `--no-sandbox` (attended run) means no sandbox applies to the
//! session's servers, which fails closed for a server approved with one.
//! Its own process: the policy and the override are set once.

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

#[test]
fn no_sandbox_blocks_a_server_that_was_approved_with_one() {
    let tools = vec![ManifestTool {
        name: "search".into(),
        description: "Search.".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let dir = TestDir::new("tole-gate-nosb");
    let user = dir.join("plugins.lock");
    let ws = TestDir::new("tole-gate-nosb-ws");
    let configured = Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile: sandbox::builtin_profile("server").unwrap(),
        servers: Default::default(),
        skip: Vec::new(),
        max_servers: 4,
        idle_secs: 60,
    };
    sandbox::set_policy(Some(configured.clone())).unwrap();
    let id = configured.applied_profile("srv", &usable()).unwrap();
    Gate::load(Some(user.clone()), &ws)
        .with_backend(usable())
        .approve_with(
            "srv",
            "stdio:true",
            "1",
            &tools,
            Approval {
                sandbox: Some(id.name),
                sandbox_hash: Some(id.hash),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(Gate::load(Some(user.clone()), &ws)
        .with_backend(usable())
        .refuse_unseen("srv", "stdio:true")
        .is_none());
    flags::set(Override::NoSandbox {
        configured: Mode::Required,
    })
    .unwrap();
    let gate = Gate::load(Some(user), &ws).with_backend(usable());
    assert!(gate.live_profile("srv", "stdio:true").is_none());
    let note = gate.refuse_unseen("srv", "stdio:true").unwrap();
    assert!(note.contains("sandbox profile"), "{note}");
    assert!(gate.check("srv", "stdio:true", "1", &tools).is_err());
}
