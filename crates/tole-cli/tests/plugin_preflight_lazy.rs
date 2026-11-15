//! #848: an approved server that the session starts lazily
//! (`[sandbox] lazy_start`, #750) is not started by the preflight at all: the
//! checks of its first call cover it. One test binary, one process: the lazy
//! setting is once per process.

#![cfg(all(unix, feature = "mcp"))]

mod common;
use common::{write_executable, TestDir};
use tole_cli::plugin_gate::{preflight, source_of, Approval, Gate};
use tole_core::mcp::McpServerConfig;
use tole_core::plugin_lock::ManifestTool;
use tole_core::sandbox::{self, Availability, Mode, Policy};

fn usable() -> Availability {
    Availability::Usable {
        bwrap: "/usr/bin/bwrap".into(),
        version: "0.9.0".into(),
    }
}

fn policy() -> Policy {
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

fn tools() -> Vec<ManifestTool> {
    vec![ManifestTool {
        name: "echo".into(),
        description: "Echoes".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
    }]
}

#[test]
fn a_lazily_started_approved_server_is_not_started_by_the_preflight() {
    sandbox::set_lazy_start(true).unwrap();
    let dir = TestDir::new("tole-preflight-lazy");
    let ws = TestDir::new("tole-preflight-lazy-ws");
    let mock = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tole-core/tests/fixtures/lazy_mcp.py"
    );
    let wrapper = dir.join("server.sh");
    write_executable(
        &wrapper,
        format!(
            "#!/bin/sh\necho started > {d}/started\nexec python3 {mock} {d}/log Echoes\n",
            d = dir.display()
        ),
    );
    let cfg = McpServerConfig::parse(&format!("cora={}", wrapper.display())).unwrap();
    let user = dir.join("plugins.lock");
    let source = source_of(&cfg);
    let id = policy().applied_profile("cora", &usable()).unwrap();
    Gate::load(Some(user.clone()), &ws)
        .approve_with(
            "cora",
            &source,
            "1",
            &tools(),
            Approval {
                sandbox: Some(id.name),
                sandbox_hash: Some(id.hash),
                ..Default::default()
            },
        )
        .unwrap();
    let mut gate = Gate::load(Some(user), &ws)
        .with_policy(Some(policy()))
        .with_backend(usable());
    assert!(gate.lazy_start_applies("cora", &source), "lazy applies");
    let mut asked = 0;
    preflight(std::slice::from_ref(&cfg), &mut gate, &mut |_| {
        asked += 1;
        true
    });
    assert_eq!(asked, 0);
    assert!(
        !dir.join("started").exists(),
        "the preflight did not start a lazily started server"
    );
}
