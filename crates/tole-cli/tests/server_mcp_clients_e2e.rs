//! #946 end to end over the REAL `tole serve` and `tole acp`: the servers a
//! server session attaches are the OWNER's (the command line or the user
//! config, read at startup) and nothing else. A request cannot add one, the
//! `cora` binary on the PATH is not attached, the plugin lock and the
//! sandbox rules apply as on the CLI, and a refused server leaves one notice
//! in the session log that a client reads with the entries.

#![cfg(all(feature = "mcp", feature = "shell-tools", unix))]

mod common;
mod serve_harness;
use common::TestDir;
use serde_json::{json, Value};
use serve_harness::{new_session, spawn_mock, Serve};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Stdio;
use tole_core::plugin_lock::{manifest_hash, LockEntry, Lockfile, LOCK_FILE};

const SECRET: &str = "sk-test";

/// An owner home with `config` as its user config.
fn home(config: &str) -> TestDir {
    let h = TestDir::new("tole-srv-mcp-home");
    std::fs::create_dir_all(h.join("tole")).unwrap();
    std::fs::write(h.join("tole/config.toml"), config).unwrap();
    h
}

/// A "server" that only records that it was started (a shell builtin, so it
/// needs nothing on the PATH).
fn spy(dir: &Path, name: &str) -> String {
    let script = dir.join(format!("{name}.sh"));
    common::write_executable(
        &script,
        format!("#!/bin/sh\n: > {}/{name}.started\nexit 1\n", dir.display()),
    );
    format!("{name}={}", script.display())
}

fn started(dir: &Path, name: &str) -> bool {
    dir.join(format!("{name}.started")).exists()
}

fn fresh_dir() -> std::path::PathBuf {
    let dir = serve_harness::workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The `mcp_server_not_started` notices of a session, as a client reads them
/// (GET entries).
fn notices(serve: &Serve, id: &str) -> Vec<Value> {
    let (code, page) = serve.http("GET", &format!("/sessions/{id}/entries"), None);
    assert_eq!(code, 200, "{page}");
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["payload"]["notice"] == "mcp_server_not_started")
        .cloned()
        .collect()
}

fn env_of<'a>(home: &'a Path, path: &'a Path) -> Vec<(&'a str, &'a str)> {
    vec![
        ("CODECORA_HOME", home.to_str().unwrap()),
        ("HOME", home.to_str().unwrap()),
        ("PATH", path.to_str().unwrap()),
        ("TOLE_NO_UPDATE_CHECK", "1"),
    ]
}

/// The owner's user config names an unapproved server: it is not started,
/// the session opens, one notice is in the log. A request that names other
/// servers (in every shape a client could try) changes nothing.
#[test]
fn an_unapproved_owner_server_is_not_started_and_a_request_cannot_add_one() {
    let d = TestDir::new("tole-srv-mcp-spies");
    let (uns, evil) = (spy(&d, "uns"), spy(&d, "evil"));
    let h = home(&format!("mcp_server = [{uns:?}]\n"));
    let empty = TestDir::new("tole-srv-mcp-path");
    let serve = Serve::spawn_with_env(&spawn_mock(0), &env_of(&h, &empty));

    let body = json!({
        "mcp_server": [evil],
        "mcpServers": [{"name": "evil", "command": d.join("evil.sh")}],
        "mcp": {"evil": d.join("evil.sh")},
    });
    let (code, v) = serve.http("POST", "/sessions", Some(&body.to_string()));
    // The unknown fields are not an input: the session opens as for `{}`.
    assert_eq!(code, 200, "{v}");
    let id = v["sessionId"].as_str().unwrap().to_string();
    let n = notices(&serve, &id);
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0]["payload"]["server"], "uns");
    let text = n[0]["payload"]["error"].as_str().unwrap();
    assert!(
        text.contains("`uns`") && text.contains("not approved"),
        "{text}"
    );
    assert!(!text.contains(SECRET));
    assert!(!started(&d, "uns"), "an unapproved server was started");
    assert!(!started(&d, "evil"), "a request started a server");
    // Nothing in any session of this process names the request's server.
    let (_, list) = serve.http("GET", "/sessions", None);
    assert!(!list.to_string().contains("evil"), "{list}");
}

/// The owner's `--mcp-server` flag on `tole serve` is allowed, like the user
/// config: it is the owner typing it. The same lock applies.
#[test]
fn the_owner_flag_on_serve_is_allowed_and_goes_through_the_lock() {
    let d = TestDir::new("tole-srv-mcp-flag");
    let uns = spy(&d, "flagged");
    let h = home("");
    let empty = TestDir::new("tole-srv-mcp-path");
    let serve = Serve::spawn_in_env_args(
        &spawn_mock(0),
        fresh_dir(),
        true,
        &env_of(&h, &empty),
        &["--mcp-server", &uns],
    );
    let id = new_session(&serve);
    let n = notices(&serve, &id);
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0]["payload"]["server"], "flagged");
    assert!(!started(&d, "flagged"));
}

/// A `cora` binary on the PATH is NOT attached on a server face (the CLI
/// attaches it as a preset): no start, no notice.
#[test]
fn no_cora_auto_preset_on_a_server_face() {
    let bin = TestDir::new("tole-srv-mcp-cora");
    common::write_executable(
        bin.join("cora"),
        format!("#!/bin/sh\n: > {}/cora.started\nexit 1\n", bin.display()),
    );
    let h = home("");
    let serve = Serve::spawn_with_env(&spawn_mock(0), &env_of(&h, &bin));
    let id = new_session(&serve);
    assert!(notices(&serve, &id).is_empty());
    let (_, page) = serve.http("GET", &format!("/sessions/{id}/entries"), None);
    assert!(!page.to_string().contains("cora"), "{page}");
    assert!(!started(&bin, "cora"), "the cora preset was started");
}

/// The sandbox rule is the CLI's: with mode `optional` and no usable
/// bubblewrap, an attended `tole run` starts an approved server unsandboxed
/// (with a warning), but `tole serve` (nobody to ask) treats `optional` as
/// `required` and does not start it. Same lock entry, same code path.
#[test]
fn an_optional_sandbox_counts_as_required_on_serve() {
    let d = TestDir::new("tole-srv-mcp-sbx");
    let (cli_spec, srv_spec) = (spy(&d, "cli"), spy(&d, "srv"));
    let h = home("[sandbox]\nmode = \"optional\"\n");
    // No bwrap on this PATH: the backend is unavailable.
    let empty = TestDir::new("tole-srv-mcp-path");
    {
        let mut lock = Lockfile::default();
        for spec in [&cli_spec, &srv_spec] {
            let cfg = tole_core::mcp::McpServerConfig::parse(spec).unwrap();
            let source = tole_cli::plugin_gate::source_of(&cfg);
            lock.servers.insert(
                cfg.name.clone(),
                LockEntry {
                    integrity: tole_cli::plugin_gate::integrity(&source),
                    source,
                    manifest_hash: manifest_hash(&[]),
                    ..Default::default()
                },
            );
        }
        lock.save(&h.join("tole").join(LOCK_FILE)).unwrap();
    }
    // Attended CLI run: the approved server is started (unsandboxed).
    let ws = TestDir::new("tole-srv-mcp-ws");
    let out = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &h, &env_of(&h, &empty))
        .args(["run", "--mcp-server", &cli_spec, "hi"])
        .current_dir(&ws)
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(started(&d, "cli"), "the CLI did not start it: {err}");

    // Serve: not started, one notice, the session still opens.
    let serve = Serve::spawn_in_env_args(
        &spawn_mock(0),
        fresh_dir(),
        true,
        &env_of(&h, &empty),
        &["--mcp-server", &srv_spec],
    );
    let id = new_session(&serve);
    assert!(!started(&d, "srv"), "an unattended optional sandbox ran it");
    let n = notices(&serve, &id);
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0]["payload"]["server"], "srv");
    let text = n[0]["payload"]["error"].as_str().unwrap();
    assert!(
        text.contains("did not start") && text.contains("optional"),
        "{text}"
    );
}

/// `tole acp`: the owner's user config servers apply; an ACP `session/new`
/// that carries `mcpServers` (the protocol's own field) does not.
#[test]
fn acp_uses_the_owner_list_and_ignores_the_clients_mcp_servers() {
    let d = TestDir::new("tole-srv-mcp-acp");
    let uns = spy(&d, "uns");
    let _ = spy(&d, "evil");
    let h = home(&format!("mcp_server = [{uns:?}]\n"));
    let empty = TestDir::new("tole-srv-mcp-path");
    let cwd = TestDir::new("tole-srv-mcp-acp-ws");
    let mut child = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &h, &env_of(&h, &empty))
        .arg("acp")
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut send = |v: Value| {
        writeln!(child.stdin.as_mut().unwrap(), "{v}").unwrap();
    };
    let mut read_id = |id: u64| -> Value {
        loop {
            let mut line = String::new();
            assert!(out.read_line(&mut line).unwrap() > 0, "acp closed stdout");
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v["id"] == id {
                    return v;
                }
            }
        }
    };
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}}));
    read_id(1);
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy(),
                   "mcpServers": [{"name": "evil", "command": d.join("evil.sh"), "args": [], "env": []}]}}));
    let created = read_id(2);
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
    let log = std::fs::read_to_string(cwd.join(format!(".tole/sessions/{sid}.jsonl"))).unwrap();
    let entries: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["payload"]["notice"] == "mcp_server_not_started")
        .collect();
    assert_eq!(entries.len(), 1, "{log}");
    assert_eq!(entries[0]["payload"]["server"], "uns");
    assert!(!log.contains("evil"), "the client's server reached the log");
    assert!(!started(&d, "uns") && !started(&d, "evil"));
    let _ = child.kill();
    let _ = child.wait();
}
