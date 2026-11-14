//! #601: a stdio MCP server started under `[sandbox]` really runs inside
//! bubblewrap, and every session binds its own workspace.
//!
//! Needs a working `bwrap`. Without one these tests print a `SKIPPED:` line
//! with the reason and return (set `TOLE_REQUIRE_BWRAP=1` to make that a
//! failure, as a CI job that installs bubblewrap should). The sandbox
//! policy is process-wide and set once here (`mode = required`); the
//! unsandboxed path is checked in `mcp_sandbox_off.rs`, a process of its
//! own.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tole_core::mcp::{
    conn_key_for, get_server_prompt_in, register_server_tools, McpReadTool, McpServerConfig,
};
use tole_core::plugin_lock::ManifestTool;
use tole_core::sandbox::{self, Availability, Launch, Mode, Policy};
use tole_core::secrets::SecretSet;

const CANARY: &str = "sandbox-canary-not-a-credential-0123456789";

/// The scratch root every test of this process writes evidence under; the
/// policy grants it as a writable path. Sets the policy exactly once.
fn out_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("tole-sbx-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let d = std::fs::canonicalize(&d).unwrap();
        let mut profile = sandbox::builtin_profile("server").unwrap();
        profile.write = vec![d.clone()];
        sandbox::set_policy(Some(Policy {
            mode: Mode::Required,
            profile_name: "server".into(),
            profile,
            skip: vec!["plainsrv".into()],
            max_servers: sandbox::DEFAULT_MAX_SERVERS,
            idle_secs: sandbox::DEFAULT_IDLE_SECS,
        }))
        .unwrap();
        d
    })
}

/// None when a sandbox works here; else the printed reason it is skipped.
fn skipped() -> Option<String> {
    out_root();
    match sandbox::detect() {
        Availability::Usable { .. } => None,
        Availability::Unavailable(why) => {
            let reason = format!("SKIPPED sandboxed half: {why}");
            assert!(
                std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
                "TOLE_REQUIRE_BWRAP is set but {reason}"
            );
            eprintln!("{reason}");
            Some(reason)
        }
    }
}

fn dir(tag: &str) -> PathBuf {
    let d = std::fs::canonicalize(out_root()).unwrap().join(tag);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Starts a server that records what it sees into `out`, then serves.
/// The registry must stay alive: it holds the session's claim on the server.
fn start(
    name: &str,
    ws: &Path,
    out: &Path,
    data: &Path,
    port: u16,
) -> (McpServerConfig, tole_core::tool::ToolRegistry) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py"),
        ws.join("mock_mcp.py"),
    )
    .unwrap();
    let wrapper = ws.join("srv.sh");
    let o = out.display();
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             cd \"$(dirname \"$0\")\"\n\
             printenv > {o}/env\n\
             {{ tr '\\0' '\\n' < /proc/1/environ > {o}/pid1env; }} 2>/dev/null\n\
             (test -e {data}/secret && echo yes || echo no) > {o}/data\n\
             echo \"$HOME\" > {o}/home\n\
             (touch \"$PWD/x\" 2>/dev/null && echo writable || echo readonly) > {o}/ws\n\
             grep CapEff /proc/self/status > {o}/caps\n\
             python3 -c \"import socket; s=socket.socket(); s.settimeout(2); s.connect(('127.0.0.1',{port}))\" 2>/dev/null && echo reached > {o}/net || echo blocked > {o}/net\n\
             exec python3 {m} full\n",
            data = data.display(),
            m = ws.join("mock_mcp.py").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cfg = McpServerConfig::parse(&format!("{name}={}", wrapper.display())).unwrap();
    cfg.secret_grant = Some(Vec::new());
    cfg.workspace = Some(ws.to_path_buf());
    let accept = |_: &str, _: &str, _: &[ManifestTool]| Ok(());
    let mut reg = tole_core::tool::ToolRegistry::new();
    register_server_tools(&mut reg, &cfg, &SecretSet::default(), &accept);
    (cfg, reg)
}

fn read(out: &Path, f: &str) -> String {
    std::fs::read_to_string(out.join(f))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The server wrote its first evidence file.
fn ran(out: &Path) -> bool {
    !read(out, "env").is_empty()
}

/// A full MCP handshake works (the mock advertises resources).
fn handshakes(cfg: &McpServerConfig) -> bool {
    tole_core::mcp::probe_server_extras(cfg).resources
}

#[test]
fn a_sandboxed_server_sees_a_private_view() {
    if skipped().is_some() {
        return;
    }
    let base = dir("view");
    let (ws, out, data) = (
        std::env::temp_dir().join(format!("tole-sbx-ws-{}", std::process::id())),
        base.join("out"),
        std::env::temp_dir().join(format!("tole-sbx-data-{}/.codecora", std::process::id())),
    );
    for d in [&ws, &out, &data] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(data.join("secret"), "x").unwrap();
    std::env::set_var("GITHUB_TOKEN", CANARY);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let (cfg, _reg) = start("sbx-view", &ws, &out, &data, port);
    assert!(ran(&out) && handshakes(&cfg), "the sandboxed server ran");
    assert_eq!(read(&out, "net"), "blocked", "no network");
    assert_eq!(read(&out, "data"), "no", "the data dir is absent");
    assert_eq!(read(&out, "home"), "/scratch", "a private home");
    assert_eq!(read(&out, "ws"), "readonly", "workspace is read-only");
    assert!(read(&out, "caps").ends_with('0'), "no capabilities");
    assert!(!read(&out, "env").contains(CANARY), "no secret in the env");
    let pid1 = read(&out, "pid1env");
    assert!(
        !pid1.is_empty() && !pid1.contains(CANARY),
        "bwrap's own env is scrubbed"
    );
}

/// The workspace bind of a launch is the session's own: a launch of another
/// session never changes it (a process-global workspace raced, per review).
#[test]
fn concurrent_sessions_each_bind_their_own_workspace() {
    if skipped().is_some() {
        return;
    }
    let handles: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let ws = dir(&format!("conc{i}"));
                for _ in 0..40 {
                    let l = sandbox::launch(&format!("conc{i}"), Some(&ws), "sh", &[]);
                    let Launch::Sandboxed { args, .. } = l else {
                        panic!("session {i} was not sandboxed: {l:?}");
                    };
                    let a: Vec<String> = args
                        .iter()
                        .map(|x| x.to_string_lossy().into_owned())
                        .collect();
                    assert!(
                        a.contains(&ws.display().to_string()),
                        "session {i} binds its own workspace"
                    );
                    for j in (0..8).filter(|j| *j != i) {
                        let other = format!("/conc{j}");
                        assert!(
                            !a.iter().any(|x| x.ends_with(&other)),
                            "session {i} saw the workspace of {j}"
                        );
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn a_second_policy_that_differs_is_refused_and_the_same_one_is_fine() {
    out_root();
    let current = sandbox::policy();
    assert!(sandbox::set_policy(current.clone()).is_ok(), "same value");
    assert!(sandbox::set_policy(None).is_err(), "a different value");
    let mut other = current.clone().unwrap();
    other.skip = vec!["x".into()];
    assert!(sandbox::set_policy(Some(other)).is_err());
    assert_eq!(sandbox::policy(), current, "unchanged after refusals");
}

#[test]
fn the_connection_key_carries_the_workspace_only_for_sandboxed_servers() {
    out_root();
    let (a, b) = (dir("ka"), dir("kb"));
    let ka = conn_key_for("srv", Some(&a));
    assert!(ka.starts_with("srv@"), "{ka}");
    assert_ne!(ka, conn_key_for("srv", Some(&b)), "workspaces differ");
    assert_eq!(
        ka,
        conn_key_for("srv", Some(&a)),
        "stable for one workspace"
    );
    // The key uses the resolved workspace, like the bind does: a link and
    // its target are one workspace.
    let link = a.with_file_name("kl-link");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&a, &link).unwrap();
    assert_eq!(
        ka,
        conn_key_for("srv", Some(&link)),
        "link and target agree"
    );
    assert_eq!(conn_key_for("plainsrv", Some(&a)), "plainsrv", "skipped");
    let http = McpServerConfig::parse("web=https://example.com/mcp").unwrap();
    assert_eq!(http.conn_key(), "web", "http servers are not sandboxed");
}

/// Two sessions of one process register the same server name with different
/// workspaces: two server processes, each bound to its own workspace, and a
/// session (or any other workspace) only reaches the server started for it.
#[test]
fn one_server_name_in_two_workspaces_is_two_servers_that_never_mix() {
    use std::os::unix::fs::PermissionsExt;
    if skipped().is_some() {
        return;
    }
    let out = out_root();
    let (wa, wb, wc) = (dir("sa"), dir("sb"), dir("sc"));
    let register = |ws: &Path| {
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py"),
            ws.join("mock_mcp.py"),
        )
        .unwrap();
        let w = ws.join("srv.sh");
        std::fs::write(
            &w,
            format!(
                "#!/bin/sh\ncd \"$(dirname \"$0\")\"\npwd > {o}/marker-$(basename \"$PWD\")\nexec python3 {m} full\n",
                o = out.display(),
                m = ws.join("mock_mcp.py").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&w, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = McpServerConfig::parse(&format!("shared={}", w.display())).unwrap();
        cfg.secret_grant = Some(Vec::new());
        cfg.workspace = Some(ws.to_path_buf());
        let accept = |_: &str, _: &str, _: &[ManifestTool]| Ok(());
        let mut reg = tole_core::tool::ToolRegistry::new();
        register_server_tools(&mut reg, &cfg, &SecretSet::default(), &accept);
        (cfg, reg)
    };
    let ((ca, _ra), (cb, _rb)) = (register(&wa), register(&wb));
    assert_ne!(ca.conn_key(), cb.conn_key());
    for (ws, tag) in [(&wa, "sa"), (&wb, "sb")] {
        let m = std::fs::read_to_string(out.join(format!("marker-{tag}"))).unwrap_or_default();
        assert_eq!(
            m.trim(),
            ws.display().to_string(),
            "a server started for {tag}"
        );
    }
    let args = || [("topic".to_string(), "x".to_string())].into();
    for ws in [&wa, &wb] {
        assert_eq!(
            get_server_prompt_in("shared", Some(ws), "review", args()).unwrap(),
            "Review: x"
        );
        let r = McpReadTool::new(vec!["shared".into()]).in_workspace(ws.clone());
        use tole_core::tool::Tool;
        assert!(r
            .execute(serde_json::json!({"server": "shared", "uri": "file:///demo"}))
            .is_ok());
    }
    // A tool of session A reaches A's server (the mock has no tools, so the
    // call fails inside the server, not with "never registered").
    {
        use tole_core::tool::Tool;
        let t = tole_core::mcp::McpTool::new(
            "shared".into(),
            "mcp_shared_x".into(),
            "x".into(),
            "d".into(),
        )
        .with_key(ca.conn_key());
        let e = t.execute(serde_json::json!({"arguments": {}})).unwrap_err();
        assert!(!e.contains("never registered"), "{e}");
    }
    // A third workspace never registered the server: it reaches neither.
    let e = get_server_prompt_in("shared", Some(&wc), "review", args()).unwrap_err();
    assert!(e.contains("never registered"), "{e}");
    use tole_core::tool::Tool;
    let r = McpReadTool::new(vec!["shared".into()]).in_workspace(wc.clone());
    let e = r
        .execute(serde_json::json!({"server": "shared", "uri": "file:///demo"}))
        .unwrap_err();
    assert!(e.contains("never registered"), "{e}");
}
