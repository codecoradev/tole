//! #750: lazy start. A server registered from its locked snapshot starts on
//! its first call, after the same two checks an eager start runs; a refusal
//! never reaches the server. Evidence comes from the host: the fixture
//! server logs `start` and `call` to a file.

#![cfg(unix)]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use test_dir::{wait_until, TestDir, WAIT};
use tole_core::approval::{AllowlistApprover, Decision};
use tole_core::mcp::{lazy_tools, register_server_tools_lazy, LazyGuard, McpServerConfig};
use tole_core::plugin_lock::{self, ManifestTool};
use tole_core::secrets::SecretSet;
use tole_core::tool::ToolRegistry;

const DESC: &str = "Echoes";

fn manifest(desc: &str) -> Vec<ManifestTool> {
    vec![ManifestTool {
        name: "echo".into(),
        description: desc.into(),
        input_schema: json!({"type": "object", "properties": {}}),
    }]
}

struct Fixture {
    dir: TestDir,
    cfg: McpServerConfig,
    /// What the lock stored: `(name, description)` pairs and the hash.
    locked: Vec<(String, String)>,
    hash: String,
}

impl Fixture {
    /// A server whose live tool description is `live` while the lock holds
    /// `DESC`.
    fn new(name: &str, live: &str) -> Self {
        let dir = TestDir::new(&format!("tole-lazy-{name}"));
        let script = dir.join("lazy_mcp.py");
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lazy_mcp.py"),
            &script,
        )
        .unwrap();
        let log = dir.join("log");
        let cfg = McpServerConfig::parse(&format!(
            "{name}=python3 {} {} {}",
            script.display(),
            log.display(),
            live
        ))
        .unwrap();
        let snap = plugin_lock::snapshot(&manifest(DESC), &SecretSet::default());
        let hash = plugin_lock::manifest_hash(&manifest(DESC));
        let locked = lazy_tools(&snap, &hash).expect("a complete snapshot");
        Fixture {
            dir,
            cfg,
            locked,
            hash,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }

    fn started(&self) -> bool {
        Path::new(&self.dir.join("log")).exists()
    }
}

#[derive(Default, Clone)]
struct Counts {
    pre: Arc<AtomicUsize>,
    admit: Arc<AtomicUsize>,
}

/// The eager checks, as the host builds them: `pre_start` refuses when told
/// to, `admit` compares the live manifest hash with the locked one.
fn guard(c: &Counts, hash: &str, pre_refuses: bool) -> LazyGuard {
    let (pre, admit, hash) = (c.pre.clone(), c.admit.clone(), hash.to_string());
    LazyGuard {
        pre_start: Box::new(move || {
            pre.fetch_add(1, Ordering::SeqCst);
            if pre_refuses {
                Err("binary changed since approval".into())
            } else {
                Ok(())
            }
        }),
        admit: Box::new(move |_, _, live| {
            admit.fetch_add(1, Ordering::SeqCst);
            if plugin_lock::manifest_hash(live) == hash {
                Ok(())
            } else {
                Err("manifest changed since approval".into())
            }
        }),
    }
}

fn call(reg: &ToolRegistry, name: &str) -> Result<serde_json::Value, String> {
    reg.get(name)
        .expect("registered")
        .execute(json!({"arguments": {}}))
}

fn register(f: &Fixture, c: &Counts, pre_refuses: bool) -> (ToolRegistry, String) {
    let mut reg =
        ToolRegistry::with_approver(AllowlistApprover::new(vec!["mcp_*".into()], Decision::Deny));
    let names = register_server_tools_lazy(
        &mut reg,
        &f.cfg,
        &SecretSet::default(),
        f.locked.clone(),
        guard(c, &f.hash, pre_refuses),
    );
    assert_eq!(names.len(), 1, "{names:?}");
    (reg, names[0].clone())
}

#[test]
fn registering_from_the_snapshot_starts_nothing_and_the_first_call_starts_it_once() {
    let f = Fixture::new("lz-ok", DESC);
    let c = Counts::default();
    let (reg, tool) = register(&f, &c, false);
    assert_eq!(tool, "mcp_lz-ok_echo");
    assert!(!reg.mcp_admitted("lz-ok"), "not admitted before first use");
    assert!(!f.started(), "no process at registration");
    assert_eq!(
        (c.pre.load(Ordering::SeqCst), c.admit.load(Ordering::SeqCst)),
        (0, 0)
    );

    let out = call(&reg, &tool).unwrap();
    assert!(out.to_string().contains("called"), "{out}");
    assert_eq!(f.log(), "start\ncall\n");
    assert_eq!(
        (c.pre.load(Ordering::SeqCst), c.admit.load(Ordering::SeqCst)),
        (1, 1)
    );

    // The checks ran once; a second call is just a call.
    call(&reg, &tool).unwrap();
    assert_eq!(f.log(), "start\ncall\ncall\n");
    assert_eq!(
        (c.pre.load(Ordering::SeqCst), c.admit.load(Ordering::SeqCst)),
        (1, 1)
    );
}

#[test]
fn a_launch_identity_the_lock_refuses_never_starts_the_process() {
    let f = Fixture::new("lz-pre", DESC);
    let c = Counts::default();
    let (reg, tool) = register(&f, &c, true);
    for _ in 0..2 {
        let e = call(&reg, &tool).unwrap_err();
        assert!(e.contains("binary changed since approval"), "{e}");
    }
    assert!(!f.started(), "the process was never started");
    assert_eq!(
        (c.pre.load(Ordering::SeqCst), c.admit.load(Ordering::SeqCst)),
        (1, 0),
        "the refusal is sticky: the check does not run again, admit never runs"
    );
}

#[test]
fn a_live_manifest_that_differs_from_the_approval_refuses_the_call_and_stops_the_server() {
    // The live tool description is not the locked one (a rug pull).
    let f = Fixture::new("lz-rug", "Changed");
    let c = Counts::default();
    let (reg, tool) = register(&f, &c, false);
    let e = call(&reg, &tool).unwrap_err();
    assert!(e.contains("manifest changed since approval"), "{e}");
    assert_eq!(f.log(), "start\n", "started to be compared, never called");
    assert_eq!(
        (c.pre.load(Ordering::SeqCst), c.admit.load(Ordering::SeqCst)),
        (1, 1)
    );
    // Every later call of the session is refused without a second start.
    let e = call(&reg, &tool).unwrap_err();
    assert!(e.contains("manifest changed since approval"), "{e}");
    assert_eq!(f.log(), "start\n", "no call ever reached the server");
    assert_eq!(c.admit.load(Ordering::SeqCst), 1);
    // The process the refusal left running is stopped.
    let script = f.dir.join("lazy_mcp.py").display().to_string();
    wait_until(WAIT, "the refused server's process to exit", || {
        !process_running(&script)
    });
}

#[test]
fn a_snapshot_that_is_missing_stale_cut_or_unsafe_is_not_lazy() {
    let snap = plugin_lock::snapshot(&manifest(DESC), &SecretSet::default());
    let hash = plugin_lock::manifest_hash(&manifest(DESC));
    assert!(lazy_tools(&snap, &hash).is_some());
    // Missing (a version 1 entry has no snapshot).
    assert!(lazy_tools(&[], &hash).is_none());
    // Stale: the lock's hash is no longer the snapshot's.
    let other = plugin_lock::manifest_hash(&manifest("Other"));
    assert!(lazy_tools(&snap, &other).is_none());
    // A description cut or edited after the hash was taken.
    let mut cut = snap.clone();
    cut[0].description.push_str("…[cut]");
    assert!(lazy_tools(&cut, &hash).is_none());
    // A schema not stored in full.
    let mut nostore = snap.clone();
    nostore[0].schema = plugin_lock::NOT_STORED.into();
    assert!(lazy_tools(&nostore, &hash).is_none());
    // A name that is not a plain tool name (it would be sent on the wire).
    let mut odd = snap.clone();
    odd[0].name = "ec ho\u{1b}".into();
    assert!(lazy_tools(&odd, &hash).is_none());
    // A description the redaction changed no longer matches its hash.
    let secret = SecretSet::default().with("LAZY_TEST_TOKEN", "hunter2hunter2");
    let leaky = vec![ManifestTool {
        description: "uses hunter2hunter2".into(),
        ..manifest(DESC).remove(0)
    }];
    let snap = plugin_lock::snapshot(&leaky, &secret);
    assert!(lazy_tools(&snap, &plugin_lock::manifest_hash(&leaky)).is_none());
}

/// Whether a live (non-zombie: those have an empty command line) process
/// has `needle` in its command line, read from /proc.
fn process_running(needle: &str) -> bool {
    std::fs::read_dir("/proc").is_ok_and(|d| {
        d.flatten().any(|e| {
            std::fs::read(e.path().join("cmdline"))
                .is_ok_and(|b| String::from_utf8_lossy(&b).contains(needle))
        })
    })
}
