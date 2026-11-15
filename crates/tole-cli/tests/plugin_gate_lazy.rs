//! #750: a lazily started server runs the same lock checks as an eager one on
//! its first call, the sandbox profile admission included: a server whose
//! approved profile no longer matches is refused and its process is never
//! started. The guard is built exactly as the roster builds it.

#![cfg(all(unix, feature = "mcp"))]

mod common;
use common::TestDir;
use serde_json::json;
use std::sync::Arc;
use tole_cli::plugin_gate::{source_of, Approval, Gate};
use tole_core::approval::{AllowlistApprover, Decision};
use tole_core::mcp::{lazy_tools, register_server_tools_lazy, LazyGuard, McpServerConfig};
use tole_core::plugin_lock::ManifestTool;
use tole_core::sandbox::{self, Availability, Mode, Network, Policy};
use tole_core::secrets::SecretSet;
use tole_core::tool::ToolRegistry;

fn usable() -> Availability {
    Availability::Usable {
        bwrap: "/usr/bin/bwrap".into(),
        version: "0.9.0".into(),
    }
}

fn policy(network: Network) -> Policy {
    let mut profile = sandbox::builtin_profile("server").unwrap();
    profile.network = network;
    Policy {
        mode: Mode::Required,
        profile_name: "server".into(),
        profile,
        servers: Default::default(),
        skip: Vec::new(),
        max_servers: 4,
        idle_secs: 60,
    }
}

#[test]
fn a_lazy_server_whose_approved_profile_moved_is_refused_and_never_started() {
    let dir = TestDir::new("tole-gate-lazy");
    let script = dir.join("lazy_mcp.py");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tole-core/tests/fixtures/lazy_mcp.py"
        ),
        &script,
    )
    .unwrap();
    let log = dir.join("log");
    let cfg = McpServerConfig::parse(&format!(
        "srv=python3 {} {} Echoes",
        script.display(),
        log.display()
    ))
    .unwrap();
    let source = source_of(&cfg);
    let tools = vec![ManifestTool {
        name: "echo".into(),
        description: "Echoes".into(),
        input_schema: json!({"type": "object", "properties": {}}),
    }];
    // Approved under the profile without a network.
    let user = dir.join("plugins.lock");
    let ws = TestDir::new("tole-gate-lazy-ws");
    let approved = policy(Network::None);
    let id = approved.applied_profile("srv", &usable()).unwrap();
    Gate::load(Some(user.clone()), &ws)
        .with_policy(Some(approved))
        .with_backend(usable())
        .approve_with(
            "srv",
            &source,
            "1",
            &tools,
            Approval {
                sandbox: Some(id.name),
                sandbox_hash: Some(id.hash),
                ..Default::default()
            },
        )
        .unwrap();
    // The config was widened since: this session's gate sees network = host.
    let gate = Arc::new(
        Gate::load(Some(user), &ws)
            .with_policy(Some(policy(Network::Host)))
            .with_backend(usable()),
    );
    // The roster's own first check already refuses it ...
    assert!(gate.refuse_unseen("srv", &source).is_some());
    // ... and so does the lazy guard on the first call, if it got that far.
    let e = gate.entries().get("srv").unwrap();
    let locked = lazy_tools(&e.manifest, &e.manifest_hash).expect("a complete snapshot");
    let (pre, post, name, src) = (
        Arc::clone(&gate),
        Arc::clone(&gate),
        cfg.name.clone(),
        source.clone(),
    );
    let guard = LazyGuard {
        pre_start: Box::new(move || pre.refuse_unseen(&name, &src).map_or(Ok(()), Err)),
        admit: Box::new(move |s, v, t| post.check(s, &source, v, t)),
    };
    let mut reg =
        ToolRegistry::with_approver(AllowlistApprover::new(vec!["mcp_*".into()], Decision::Deny));
    let names = register_server_tools_lazy(&mut reg, &cfg, &SecretSet::default(), locked, guard);
    assert_eq!(names.len(), 1, "{names:?}");
    for _ in 0..2 {
        let err = reg
            .get(&names[0])
            .unwrap()
            .execute(json!({"arguments": {}}))
            .unwrap_err();
        assert!(err.contains("sandbox profile"), "{err}");
    }
    assert!(!log.exists(), "the server process was never started");
}
