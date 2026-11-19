//! #946 at the session host: a server session (the host behind `tole serve`
//! and `tole acp`) attaches the OWNER's MCP servers, through the same plugin
//! lock as the CLI. An approved server's tools are there, as `Write`; an
//! unapproved, changed or revoked one is never started, the session still
//! opens, and ONE notice per refused server is in the session log. One test
//! in its own process: the owner's list is set once, and the user plugin lock
//! lives in a temp tole data dir.

#![cfg(all(feature = "mcp", feature = "shell-tools", unix))]

mod common;
use common::TestDir;
use tole_core::approval::AllowlistApprover;
use tole_core::mcp::McpServerConfig;
use tole_core::plugin_lock::{manifest_hash, LockEntry, Lockfile, ManifestTool, LOCK_FILE};
use tole_core::storage::Storage;
use tole_core::tool::Risk;

const SECRET: &str = "sk-live-server-secret-946";

/// A stdio server with one tool (`echo`) and resources off; `log` records
/// that it was started.
fn mock_spec(name: &str, log: &std::path::Path) -> String {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tole-core/tests/fixtures/lazy_mcp.py"
    );
    format!("{name}=python3 {script} {} desc", log.display())
}

fn lock_entry(cfg: &McpServerConfig, hash: String, version: &str) -> LockEntry {
    let source = tole_cli::plugin_gate::source_of(cfg);
    LockEntry {
        integrity: tole_cli::plugin_gate::integrity(&source),
        source,
        version: version.into(),
        manifest_hash: hash,
        ..Default::default()
    }
}

/// A "server" that only records that it was started.
fn spy(dir: &std::path::Path, name: &str) -> String {
    let script = dir.join(format!("{name}.sh"));
    common::write_executable(
        &script,
        format!("#!/bin/sh\n: > {}/{name}.started\nexit 1\n", dir.display()),
    );
    format!("{name}={}", script.display())
}

#[test]
fn owner_servers_go_through_the_lock_and_refused_ones_leave_one_notice_each() {
    let d = TestDir::new("tole-server-mcp");
    std::fs::create_dir_all(d.join("home/tole")).unwrap();
    std::fs::create_dir_all(d.join("w")).unwrap();
    std::fs::create_dir_all(d.join("spies")).unwrap();
    // SAFETY (edition 2021: safe call): the only test in this binary.
    std::env::set_var("CODECORA_HOME", d.join("home"));
    std::env::set_var("TOLE_API_KEY", SECRET);

    let (okay, chg) = (
        mock_spec("okay", &d.join("spies/okay.log")),
        mock_spec("chg", &d.join("spies/chg.log")),
    );
    let uns = spy(&d.join("spies"), "uns");
    let rev = spy(&d.join("spies"), "rev");
    let cfg_okay = McpServerConfig::parse(&okay).unwrap();
    let cfg_chg = McpServerConfig::parse(&chg).unwrap();
    let cfg_rev = McpServerConfig::parse(&rev).unwrap();
    let (version, tools) = tole_core::mcp::list_manifest(&cfg_okay).unwrap();

    let mut lock = Lockfile::default();
    lock.servers.insert(
        "okay".into(),
        lock_entry(&cfg_okay, manifest_hash(&tools), &version),
    );
    // Approved for another tool list than the server lists now: changed.
    let other = vec![ManifestTool {
        name: "gone".into(),
        description: "a tool the server no longer lists".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    lock.servers.insert(
        "chg".into(),
        lock_entry(&cfg_chg, manifest_hash(&other), &version),
    );
    let mut revoked = lock_entry(&cfg_rev, manifest_hash(&[]), "");
    revoked.revoked = true;
    lock.servers.insert("rev".into(), revoked);
    lock.save(&d.join("home/tole").join(LOCK_FILE)).unwrap();

    let owner = vec![okay, chg, uns, rev];
    tole_cli::session_host::set_server_skills(
        tole_cli::session::assemble::ServerSkills::new(vec![], true).unwrap(),
    );
    tole_cli::session::owner_mcp::set(owner.clone()).unwrap();
    assert_eq!(tole_cli::session::owner_mcp::specs(), owner.as_slice());
    // The list is set once: a second call cannot replace it.
    assert!(
        tole_cli::session::owner_mcp::set(vec![mock_spec("late", &d.join("late.log"))]).is_err()
    );
    assert_eq!(tole_cli::session::owner_mcp::specs(), owner.as_slice());

    let sessions = d.join("sessions");
    let session = tole_cli::session_host::open_session(
        "srv-mcp",
        d.join("w").to_str().unwrap(),
        false,
        false,
        AllowlistApprover::allow_only(vec![]),
        None,
        Some(&sessions),
        vec![],
        vec![],
        tole_core::cancel::CancelToken::default(),
        None,
    )
    .expect("the session opens although three servers are refused");
    let reg = &session.registry;

    // The approved server's tools are there, every one of them Write.
    let mcp_tools: Vec<String> = reg
        .specs()
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap().to_string())
        .filter(|n| n.starts_with("mcp_"))
        .collect();
    assert!(!mcp_tools.is_empty(), "the approved server has tools");
    for t in &mcp_tools {
        assert!(t.starts_with("mcp_okay_"), "only the approved server: {t}");
        assert_eq!(reg.get(t).unwrap().risk(), Risk::Write, "{t}");
    }
    // The ReadOnly resource reader stays off on a server face.
    assert_eq!(mcp_tools, vec!["mcp_okay_echo".to_string()]);
    assert!(reg.get("mcp_read").is_none());
    // A refused server is not started and has no tools.
    for n in ["uns", "rev"] {
        assert!(!d.join(format!("spies/{n}.started")).exists(), "{n} ran");
    }
    assert!(reg.mcp_admitted("okay"));
    for n in ["chg", "uns", "rev"] {
        assert!(!reg.mcp_admitted(n), "{n} was admitted");
    }

    // One notice per refused server in the session log, naming the reason.
    let log = tole_core::storage::JsonlStorage::open_read_only(&session.path).unwrap();
    let notices: Vec<&tole_core::entry::Entry> = log
        .entries()
        .iter()
        .filter(|e| e.payload["notice"] == "mcp_server_not_started")
        .collect();
    assert_eq!(notices.len(), 3, "{notices:?}");
    let text_of = |server: &str| -> String {
        let hits: Vec<_> = notices
            .iter()
            .filter(|e| e.payload["server"] == server)
            .collect();
        assert_eq!(hits.len(), 1, "one notice for {server}");
        assert_eq!(hits[0].kind.as_str(), "error");
        hits[0].payload["error"].as_str().unwrap().to_string()
    };
    assert!(
        text_of("uns").contains("not approved"),
        "{}",
        text_of("uns")
    );
    assert!(text_of("rev").contains("revoked"), "{}", text_of("rev"));
    assert!(text_of("chg").contains("changed"), "{}", text_of("chg"));
    for n in ["uns", "rev", "chg"] {
        assert!(text_of(n).contains(n), "the notice names the server");
    }
    assert!(log.entries().iter().all(|e| e.payload["server"] != "okay"));
    // No secret in the log, in whole.
    let raw = std::fs::read_to_string(&session.path).unwrap();
    assert!(!raw.contains(SECRET), "a secret reached the session log");

    let path = session.path.clone();
    drop(log);
    drop(session);
    // A resumed session still opens, and its notices are not repeated.
    let again = tole_cli::session_host::open_session(
        "srv-mcp",
        d.join("w").to_str().unwrap(),
        true,
        false,
        AllowlistApprover::allow_only(vec![]),
        None,
        Some(&sessions),
        vec![],
        vec![],
        tole_core::cancel::CancelToken::default(),
        None,
    )
    .unwrap();
    assert!(again.registry.get("mcp_okay_echo").is_some());
    let log = tole_core::storage::JsonlStorage::open_read_only(&path).unwrap();
    let n = log
        .entries()
        .iter()
        .filter(|e| e.payload["notice"] == "mcp_server_not_started")
        .count();
    assert_eq!(n, 3, "a resume does not repeat a notice already in the log");
    let _ = std::fs::remove_dir_all(&d);
}

/// No request, override or header can carry an MCP server: the per-session
/// overrides hold exactly a persona, a model and a profile name. Adding a
/// field here must be a deliberate edit of this pin (and of the threat model).
#[test]
fn session_overrides_carry_no_mcp_input() {
    let tole_cli::session_host::SessionOverrides {
        system_prompt: _,
        model: _,
        profile: _,
    } = tole_cli::session_host::SessionOverrides::default();
}
