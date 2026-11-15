//! #848: the interactive preflight decides by the lock BEFORE it starts a
//! server, and what it does start (an unapproved or changed server, to show
//! its tools) gets no secret and ends before the session starts. Evidence
//! comes from the host: the fixture wrapper records its start, its
//! environment and its pid in files.

#![cfg(all(unix, feature = "mcp"))]

mod common;
use common::{wait_until, write_executable, TestDir, WAIT};
use std::path::PathBuf;
use tole_cli::plugin_gate::{preflight, source_of, Approval, Gate};
use tole_core::approval::{AllowlistApprover, Decision};
use tole_core::mcp::{register_server_tools, McpServerConfig};
use tole_core::plugin_lock::{Lockfile, ManifestTool};
use tole_core::sandbox::{self, Availability, Mode, Network, Policy};
use tole_core::secrets::SecretSet;
use tole_core::tool::ToolRegistry;

/// A value the default `[secrets]` release binds to `mcp`.
const SECRET: &str = "gh-preflight-test-0123456789abcdef";

fn release_secret() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY (edition 2021: safe call): every test sets the same value
    // first, inside this `Once`, before it starts a child process.
    ONCE.call_once(|| std::env::set_var("GITHUB_TOKEN", SECRET));
}

fn tools() -> Vec<ManifestTool> {
    vec![ManifestTool {
        name: "echo".into(),
        description: "Echoes".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
    }]
}

struct Fixture {
    dir: TestDir,
    ws: TestDir,
    cfg: McpServerConfig,
    user: PathBuf,
}

impl Fixture {
    /// A server `name` whose wrapper writes `start.<pid>` (its environment)
    /// and `pid` on every start, then runs the logging fixture server.
    fn new(name: &str) -> Self {
        release_secret();
        let dir = TestDir::new(&format!("tole-preflight-{name}"));
        let mock = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tole-core/tests/fixtures/lazy_mcp.py"
        );
        let wrapper = dir.join("server.sh");
        write_executable(
            &wrapper,
            format!(
                "#!/bin/sh\nprintenv > {d}/env.$$\necho $$ > {d}/pid\nexec python3 {mock} {d}/log Echoes\n",
                d = dir.display()
            ),
        );
        let cfg = McpServerConfig::parse(&format!("{name}={}", wrapper.display())).unwrap();
        let user = dir.join("plugins.lock");
        let ws = TestDir::new(&format!("tole-preflight-{name}-ws"));
        Fixture { dir, ws, cfg, user }
    }

    fn gate(&self) -> Gate {
        Gate::load(Some(self.user.clone()), &self.ws)
    }

    fn approve(&self, with: Approval) {
        let source = source_of(&self.cfg);
        self.gate()
            .approve_with(&self.cfg.name, &source, "1", &tools(), with)
            .unwrap();
    }

    fn started(&self) -> bool {
        self.dir.join("log").exists() || self.dir.join("pid").exists()
    }

    /// The environments of every process the wrapper started.
    fn envs(&self) -> Vec<String> {
        std::fs::read_dir(&*self.dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("env."))
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect()
    }

    /// Runs the preflight; the prompts the owner saw.
    fn prompts(&self, gate: &mut Gate, answer: bool) -> Vec<String> {
        let mut seen = Vec::new();
        preflight(std::slice::from_ref(&self.cfg), gate, &mut |t| {
            seen.push(t.to_string());
            answer
        });
        seen
    }

    /// Runs the preflight; returns how often the owner was asked.
    fn preflight(&self, gate: &mut Gate, answer: bool) -> usize {
        self.prompts(gate, answer).len()
    }

    fn lock_entry(&self) -> tole_core::plugin_lock::LockEntry {
        Lockfile::load(&self.user)
            .unwrap()
            .servers
            .get(&self.cfg.name)
            .cloned()
            .expect("an approved entry")
    }
}

/// Another tool list than the fixture server serves.
fn old_tools() -> Vec<ManifestTool> {
    let mut t = tools();
    t[0].description = "Old description".into();
    t
}

#[test]
fn a_revoked_server_is_not_started_nor_offered() {
    let f = Fixture::new("pf-revoked");
    f.approve(Approval::default());
    let mut lock = Lockfile::load(&f.user).unwrap();
    lock.servers.get_mut("pf-revoked").unwrap().revoked = true;
    lock.save(&f.user).unwrap();
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 0, "never offered");
    assert!(!f.started(), "the process of a revoked server never ran");
}

#[test]
fn a_moved_server_is_not_started_nor_offered() {
    let f = Fixture::new("pf-moved");
    // Approved for ANOTHER command line than the configured one.
    f.gate()
        .approve("pf-moved", "stdio:/bin/other --flag", "1", &tools())
        .unwrap();
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 0);
    assert!(!f.started(), "the process of a moved server never ran");
}

#[test]
fn a_server_with_a_swapped_binary_is_not_started_nor_offered() {
    let f = Fixture::new("pf-rebuilt");
    // The lock holds the hash of another program file under this command.
    f.approve(Approval {
        integrity: Some("sha256:0000".into()),
        ..Default::default()
    });
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 0);
    assert!(!f.started(), "a swapped binary never ran");
}

#[test]
fn a_server_whose_sandbox_profile_changed_is_not_started_nor_offered() {
    let f = Fixture::new("pf-profile");
    f.approve(Approval {
        sandbox: Some("server".into()),
        sandbox_hash: Some("sha256:approved".into()),
        ..Default::default()
    });
    // No sandbox applies now (policy off): the approved profile is gone.
    let mut gate = f.gate().with_policy(None);
    assert_eq!(f.preflight(&mut gate, true), 0);
    assert!(!f.started(), "a server in another profile never ran");
}

#[test]
fn a_new_server_is_reviewed_without_any_secret_and_its_process_ends() {
    let f = Fixture::new("cora");
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 1, "offered for review");
    let envs = f.envs();
    assert_eq!(envs.len(), 1, "started once, for the listing");
    assert!(
        !envs[0].contains(SECRET),
        "a review start gets no mcp-released secret"
    );
    // Approved, and the review process does not outlive the preflight.
    assert!(f.gate().entries().contains_key("cora"));
    let pid: u32 = std::fs::read_to_string(f.dir.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_until(WAIT, "the review process to end", || {
        !PathBuf::from(format!("/proc/{pid}")).exists()
            || std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map_or(true, |s| s.contains(") Z "))
    });
}

#[test]
fn the_session_connection_after_a_preflight_has_only_the_lock_grant() {
    let f = Fixture::new("uteke");
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 1);
    // The approval grants this server no secret.
    let mut lock = Lockfile::load(&f.user).unwrap();
    lock.servers.get_mut("uteke").unwrap().secrets.clear();
    lock.save(&f.user).unwrap();

    // The session, as the roster builds it: the lock's grant, own start.
    let gate = f.gate();
    let source = source_of(&f.cfg);
    let mut cfg = f.cfg.clone();
    cfg.secret_grant = gate.secret_grant("uteke");
    assert_eq!(cfg.secret_grant, Some(Vec::new()));
    cfg.workspace = Some(f.ws.path().to_path_buf());
    let admit = |s: &str, v: &str, t: &[ManifestTool]| gate.check(s, &source, v, t);
    let mut reg =
        ToolRegistry::with_approver(AllowlistApprover::new(vec!["mcp_*".into()], Decision::Deny));
    let names = register_server_tools(&mut reg, &cfg, &SecretSet::default(), &admit);
    assert_eq!(names.len(), 1, "{names:?}");
    let envs = f.envs();
    assert_eq!(envs.len(), 2, "the session starts its own process");
    assert!(
        envs.iter().all(|e| !e.contains(SECRET)),
        "neither the review nor the session got the secret the lock does not grant"
    );
}

#[test]
fn an_approved_unchanged_server_is_listed_once_and_not_offered() {
    // Lazy start is off: its tools are compared eagerly, so the preflight is
    // the only place that can offer a changed list; an unchanged one costs
    // one start and no prompt.
    let f = Fixture::new("laya");
    f.approve(Approval::default());
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 0);
    assert_eq!(f.envs().len(), 1, "listed once");
}

#[test]
fn a_changed_server_is_offered_and_a_re_approval_never_widens_its_grant() {
    let f = Fixture::new("nginjen");
    // Approved earlier with another tool list and NO secret.
    f.gate()
        .approve_with(
            "nginjen",
            &source_of(&f.cfg),
            "1",
            &old_tools(),
            Approval {
                secrets: Some(Vec::new()),
                tier: Some(tole_core::plugin_lock::Tier::FirstParty),
                ..Default::default()
            },
        )
        .unwrap();
    let mut gate = f.gate();
    let seen = f.prompts(&mut gate, true);
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains("CHANGED since you approved it"),
        "{}",
        seen[0]
    );
    assert!(
        seen[0].contains("It will receive no secret."),
        "{}",
        seen[0]
    );
    let e = f.lock_entry();
    assert_eq!(
        e.manifest_hash,
        tole_core::plugin_lock::manifest_hash(&tools()),
        "the new list is what is locked"
    );
    assert!(
        e.secrets.is_empty(),
        "the released GITHUB_TOKEN was not added to a narrowed grant: {:?}",
        e.secrets
    );
    assert_eq!(e.tier, Some(tole_core::plugin_lock::Tier::FirstParty));
}

#[test]
fn a_changed_bundle_server_keeps_its_bundle_and_grant_when_re_approved() {
    let f = Fixture::new("cora");
    let manifest = f.dir.join("tole-plugin.toml");
    std::fs::write(&manifest, "name = \"p\"\n").unwrap();
    let bundle = tole_core::plugin_lock::BundleRef {
        plugin: "p".into(),
        version: "1".into(),
        hash: tole_core::plugin_lock::sha(&std::fs::read(&manifest).unwrap()),
        path: manifest.display().to_string(),
    };
    f.gate()
        .approve_with(
            "cora",
            &source_of(&f.cfg),
            "1",
            &old_tools(),
            Approval {
                secrets: Some(Vec::new()),
                bundle: Some(bundle.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 1);
    let e = f.lock_entry();
    assert_eq!(e.bundle, Some(bundle), "bundle membership survives");
    assert!(e.secrets.is_empty(), "{:?}", e.secrets);
}

#[test]
fn a_project_pin_never_shrinks_the_user_grant_on_a_re_approval() {
    let f = Fixture::new("cora");
    let held = vec!["GITHUB_TOKEN".to_string(), "OTHER_SECRET".to_string()];
    f.gate()
        .approve_with(
            "cora",
            &source_of(&f.cfg),
            "1",
            &old_tools(),
            Approval {
                secrets: Some(held.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    // A repository pins the same approval with ONE of the two grants.
    let mut pin = f.lock_entry();
    pin.secrets = vec!["GITHUB_TOKEN".into()];
    let mut project = Lockfile::default();
    project.servers.insert("cora".into(), pin);
    project.save(&Lockfile::project_path(&f.ws)).unwrap();
    let mut gate = f.gate();
    let seen = f.prompts(&mut gate, true);
    assert_eq!(seen.len(), 1, "offered once");
    let e = f.lock_entry();
    assert_eq!(e.secrets, held, "the user lock keeps both grants");
    assert_eq!(
        e.manifest_hash,
        tole_core::plugin_lock::manifest_hash(&tools())
    );
}

#[test]
fn a_server_whose_lock_entry_was_deleted_is_offered_with_the_ledgers_moved_profile_warning() {
    let f = Fixture::new("cora");
    // Approved WITH a sandbox profile, then the lock entry is deleted by hand:
    // the ledger still says what was approved (the rule of `plugin approve`).
    f.approve(Approval {
        sandbox: Some("server".into()),
        sandbox_hash: Some("sha256:approved".into()),
        ..Default::default()
    });
    let mut lock = Lockfile::load(&f.user).unwrap();
    lock.servers.clear();
    lock.save(&f.user).unwrap();
    let mut gate = f.gate();
    let seen = f.prompts(&mut gate, false);
    assert_eq!(seen.len(), 1, "a terminal is offered it");
    assert!(
        seen[0].contains("differs from the one your ledger recorded"),
        "{}",
        seen[0]
    );
}

#[test]
fn an_owner_who_declines_a_changed_server_changes_nothing() {
    let f = Fixture::new("cora");
    f.gate()
        .approve_with(
            "cora",
            &source_of(&f.cfg),
            "1",
            &old_tools(),
            Approval::default(),
        )
        .unwrap();
    let before = f.lock_entry();
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, false), 1, "asked once");
    let after = f.lock_entry();
    assert_eq!(after.manifest_hash, before.manifest_hash);
    assert_eq!(after.approved_at, before.approved_at);
}

#[test]
fn a_third_party_server_nobody_verified_is_not_started_without_a_sandbox() {
    // Same rule as `tole plugin add`: no sandbox applies here (no policy), so
    // an unverified third-party stdio server is neither started nor offered.
    let f = Fixture::new("pf-unverified");
    let mut gate = f.gate();
    assert_eq!(f.preflight(&mut gate, true), 0);
    assert!(!f.started(), "refused before anything started");
    assert!(!f.gate().entries().contains_key("pf-unverified"));
}

fn usable() -> Availability {
    Availability::Usable {
        bwrap: "/usr/bin/bwrap".into(),
        version: "0.9.0".into(),
    }
}

/// #848 (finding 2): an approval recorded WITH a sandbox is refused, never
/// run unsandboxed, when the wrapper can no longer be built in mode
/// `optional`.
#[test]
fn a_recorded_sandbox_that_can_no_longer_be_built_is_refused_in_optional_mode() {
    let dir = TestDir::new("tole-preflight-optional");
    let ws = TestDir::new("tole-preflight-optional-ws");
    let granted = std::fs::canonicalize(&*dir).unwrap().join("granted");
    std::fs::create_dir(&granted).unwrap();
    let mut profile = sandbox::builtin_profile("server").unwrap();
    profile.network = Network::None;
    profile.read = vec![granted.clone()];
    let policy = Policy {
        mode: Mode::Optional,
        profile_name: "server".into(),
        profile,
        servers: Default::default(),
        skip: Vec::new(),
        max_servers: 4,
        idle_secs: 60,
    };
    let source = "stdio:true";
    let user = dir.join("plugins.lock");
    let id = policy.applied_profile("srv", &usable()).unwrap();
    Gate::load(Some(user.clone()), &ws)
        .approve_with(
            "srv",
            source,
            "1",
            &tools(),
            Approval {
                sandbox: Some(id.name),
                sandbox_hash: Some(id.hash),
                ..Default::default()
            },
        )
        .unwrap();
    let gate = || {
        Gate::load(Some(user.clone()), &ws)
            .with_policy(Some(policy.clone()))
            .with_backend(usable())
    };
    assert!(
        gate().refuse_unseen("srv", source).is_none(),
        "buildable: admitted"
    );
    // The granted path is renamed away (same user, no privilege needed).
    std::fs::rename(&granted, dir.join("moved")).unwrap();
    let why = gate().refuse_unseen("srv", source).unwrap();
    assert!(why.contains("sandbox profile"), "{why}");
    assert!(gate().check("srv", source, "1", &tools()).is_err());
}

/// A server approved WITHOUT a sandbox keeps `optional`'s meaning.
#[test]
fn optional_mode_keeps_running_a_server_that_was_approved_without_a_sandbox() {
    let dir = TestDir::new("tole-preflight-optional-none");
    let ws = TestDir::new("tole-preflight-optional-none-ws");
    let mut profile = sandbox::builtin_profile("server").unwrap();
    profile.read = vec![dir.join("never-existed")];
    let policy = Policy {
        mode: Mode::Optional,
        profile_name: "server".into(),
        profile,
        servers: Default::default(),
        skip: Vec::new(),
        max_servers: 4,
        idle_secs: 60,
    };
    let user = dir.join("plugins.lock");
    // Approved before the sandbox was on: nothing recorded.
    Gate::load(Some(user.clone()), &ws)
        .approve("srv", "stdio:true", "1", &tools())
        .unwrap();
    let gate = Gate::load(Some(user), &ws)
        .with_policy(Some(policy))
        .with_backend(usable());
    assert!(gate.refuse_unseen("srv", "stdio:true").is_none());
}
