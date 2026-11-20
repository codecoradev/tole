//! #846: the REAL lazy-start wiring of the tool roster. `plugin_gate_lazy.rs`
//! builds the guard by hand; this one runs `tool_roster::build` itself with
//! `[sandbox] lazy_start` on, against the fixture MCP server of
//! `tole-core/tests/mcp_lazy.rs` (it logs `start` and `call` to a file), so
//! `lazy_snapshot`, the `pre_start` and `admit` closures and
//! `set_lazy_start` are what is under test:
//!
//! - nothing starts at roster build;
//! - the first call runs the lock's pre-start check, starts the server in
//!   its sandbox, and compares the live manifest, once;
//! - a binary swapped after the roster was built is refused on the first
//!   call and its process is never started (`pre_start`);
//! - a live manifest that differs from the approval is refused after the
//!   start, the call never reaches the server (`admit`);
//! - a revoked server, and one whose approved profile moved, never register.
//!
//! Needs a working `bwrap` (the lazy path applies to sandboxed servers
//! only): without one it prints a `SKIPPED:` line and returns, and
//! `TOLE_REQUIRE_BWRAP=1` makes that a failure, like `mcp_sandbox.rs`.
//! What it cannot cover: a profile that moves between roster build and the
//! first call (the process policy is set once; the profile check on the
//! first call is covered by `plugin_gate_lazy.rs`). One test, because the
//! policy, the lazy flag and `CODECORA_HOME` are process-wide.

#![cfg(all(unix, feature = "mcp"))]

mod common;
use common::{wait_until, write_executable, TestDir, WAIT};
use serde_json::json;
use tole_cli::plugin_gate::{source_of, Approval, Gate};
use tole_cli::tool_roster::{build, Roster};
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::mcp::McpServerConfig;
use tole_core::plugin_lock::{Lockfile, ManifestTool};
use tole_core::sandbox::{self, Availability, Mode, Network, Policy};
use tole_core::tool::ToolRegistry;

struct Deny;

impl Approver for Deny {
    fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
        Verdict::Deny
    }
}

fn tools() -> Vec<ManifestTool> {
    vec![ManifestTool {
        name: "echo".into(),
        description: "Echoes".into(),
        input_schema: json!({"type": "object", "properties": {}}),
    }]
}

fn policy(logs: &std::path::Path, network: Network) -> Policy {
    let mut profile = sandbox::builtin_profile("server").unwrap();
    profile.network = network;
    // The fixture writes its evidence log from inside the sandbox.
    profile.write = vec![logs.to_path_buf()];
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

fn call(reg: &ToolRegistry, name: &str) -> Result<serde_json::Value, String> {
    reg.get(name)
        .unwrap_or_else(|| panic!("{name} is registered"))
        .execute(json!({"arguments": {}}))
}

fn process_running(needle: &str) -> bool {
    std::fs::read_dir("/proc").is_ok_and(|d| {
        d.flatten().any(|e| {
            std::fs::read(e.path().join("cmdline"))
                .is_ok_and(|b| String::from_utf8_lossy(&b).contains(needle))
        })
    })
}

#[test]
fn the_roster_registers_locked_servers_lazily_and_the_first_call_runs_the_lock_checks() {
    let backend = sandbox::detect();
    if let Availability::Unavailable(why) = &backend {
        assert!(
            std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
            "TOLE_REQUIRE_BWRAP is set but no sandbox works: {why}"
        );
        eprintln!("SKIPPED: no sandbox here: {why}");
        return;
    }
    let base = TestDir::new("tole-roster-lazy");
    let home = base.join("home");
    let ws = base.join("ws");
    let logs = base.join("logs");
    for d in [&home, &ws, &logs] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (ws, logs) = (
        std::fs::canonicalize(&ws).unwrap(),
        std::fs::canonicalize(&logs).unwrap(),
    );
    std::env::set_var("CODECORA_HOME", &home);
    sandbox::set_policy(Some(policy(&logs, Network::None))).unwrap();
    sandbox::set_lazy_start(true).unwrap();

    let script = ws.join("lazy_mcp.py");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tole-core/tests/fixtures/lazy_mcp.py"
        ),
        &script,
    )
    .unwrap();
    let log = |n: &str| logs.join(n);
    let read = |n: &str| std::fs::read_to_string(log(n)).unwrap_or_default();
    let spec = |name: &str, desc: &str| {
        McpServerConfig::parse(&format!(
            "{name}=python3 {} {} {desc}",
            script.display(),
            log(name).display()
        ))
        .unwrap()
    };
    // The stale-binary server runs through a wrapper whose bytes change.
    let wrapper = ws.join("stale.sh");
    let wrapper_src = |extra: &str| {
        format!(
            "#!/bin/sh\n{extra}exec python3 {} {} Echoes\n",
            script.display(),
            log("lzstale").display()
        )
    };
    write_executable(&wrapper, wrapper_src(""));
    let stale = McpServerConfig::parse(&format!("lzstale={}", wrapper.display())).unwrap();
    let servers = vec![
        spec("lzok", "Echoes"),
        // Approved with "Echoes"; the live server says something else.
        spec("lzrug", "Changed"),
        stale,
        spec("lzrev", "Echoes"),
        spec("lzmoved", "Echoes"),
    ];

    // Approvals, as `tole plugin approve` writes them: the same sandbox the
    // session runs in, except `lzmoved`, approved under a wider network.
    let user = Lockfile::user_path().expect("a data dir");
    let approve = |cfg: &McpServerConfig, p: Policy| {
        let id = p.applied_profile(&cfg.name, &backend).unwrap();
        Gate::load(Some(user.clone()), &ws)
            .with_policy(Some(p))
            .with_backend(backend.clone())
            .approve_with(
                &cfg.name,
                &source_of(cfg),
                "1",
                &tools(),
                Approval {
                    sandbox: Some(id.name),
                    sandbox_hash: Some(id.hash),
                    ..Default::default()
                },
            )
            .unwrap();
    };
    for cfg in &servers {
        let net = if cfg.name == "lzmoved" {
            Network::Host
        } else {
            Network::None
        };
        approve(cfg, policy(&logs, net));
    }
    let mut lock = Lockfile::load(&user).unwrap();
    lock.servers.get_mut("lzrev").unwrap().revoked = true;
    lock.save(&user).unwrap();

    // The real roster build.
    let mut roster = Roster::new(ws.clone());
    roster.mcp_servers = servers;
    let reg = build(Deny, roster).unwrap();

    // Nothing started at build; locked servers registered from the lock,
    // a revoked one and one whose profile moved did not register.
    for n in ["lzok", "lzrug", "lzstale", "lzrev", "lzmoved"] {
        assert!(!log(n).exists(), "{n} was started at roster build");
    }
    assert!(reg.get("mcp_lzrev_echo").is_none(), "revoked registered");
    assert!(reg.get("mcp_lzmoved_echo").is_none(), "moved registered");
    for n in ["lzok", "lzrug", "lzstale"] {
        assert!(reg.get(&format!("mcp_{n}_echo")).is_some(), "{n}");
        assert!(!reg.mcp_admitted(n), "{n} admitted before its first call");
    }

    // The first call: pre-start check, start, live manifest check, call.
    let out = call(&reg, "mcp_lzok_echo").unwrap();
    assert!(out.to_string().contains("called"), "{out}");
    assert_eq!(read("lzok"), "start\ncall\n");
    call(&reg, "mcp_lzok_echo").unwrap();
    assert_eq!(read("lzok"), "start\ncall\ncall\n", "started only once");

    // A binary swapped after the roster was built is never started.
    write_executable(&wrapper, wrapper_src("# swapped\n"));
    for _ in 0..2 {
        let e = call(&reg, "mcp_lzstale_echo").unwrap_err();
        assert!(e.contains("binary"), "{e}");
    }
    assert!(!log("lzstale").exists(), "the swapped binary was started");

    // A live manifest that is not the approved one: started to be
    // compared, refused, the call never reached it, and it is stopped.
    for _ in 0..2 {
        let e = call(&reg, "mcp_lzrug_echo").unwrap_err();
        assert!(e.contains("manifest") || e.contains("changed"), "{e}");
    }
    assert_eq!(read("lzrug"), "start\n", "started to be compared only");
    let needle = log("lzrug").display().to_string();
    wait_until(WAIT, "the refused server's process to exit", || {
        !process_running(&needle)
    });
}
