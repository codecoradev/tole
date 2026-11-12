#![cfg(unix)]

use super::*;
use tole_core::delegation::NodeStatus;
use tole_core::entry::{EntryType, NewEntry};
use tole_core::storage::{Commit, JsonlStorage, UsageRecord};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-delegate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// A stand-in tole binary: records its argv and env next to its sessions
/// dir, then sleeps `secs`.
fn fake_tole(dir: &Path, secs: u64) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(format!("fake-tole-{secs}"));
    let body = format!(
        "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do [ \"$prev\" = \"--sessions-dir\" ] && D=\"$a\"; \
         prev=\"$a\"; done\nmkdir -p \"$D\"\nprintf '%s\\n' \"$@\" > \"$D/../argv\"\n\
         cut -d' ' -f19 /proc/$$/stat > \"$D/../nice\" 2>/dev/null\n\
         env > \"$D/../env\"\nsleep {secs}\n"
    );
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn setup(src: &str) -> Setup {
    Setup {
        cfg: crate::config::parse(src, Path::new("user.toml")).unwrap(),
        base: None,
        home: None,
        data_dir: Some(data_dir()),
        // The machine guard never holds these tests back: they share one
        // registry and run in parallel.
        probe: Some(SharedProbe(std::sync::Arc::new(Dial::roomy()))),
        thresholds: Some(open_thresholds()),
    }
}

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use tole_core::resource_guard::{Disk, Load, Mem, Pids, Probe};

/// A probe whose free memory a test turns; everything else is roomy.
struct Dial(AtomicU64);

impl Dial {
    fn roomy() -> Self {
        Self(AtomicU64::new(1 << 20))
    }
}

impl Probe for Dial {
    fn memory(&self) -> Option<Mem> {
        Some(Mem {
            available_mb: self.0.load(Relaxed),
        })
    }
    fn load(&self) -> Option<Load> {
        Some(Load {
            one_min: 0.0,
            cpus: 4,
        })
    }
    fn disk(&self, _: &Path) -> Option<Disk> {
        Some(Disk { free_mb: 1 << 20 })
    }
    fn pids(&self) -> Option<Pids> {
        Some(Pids {
            current: 1,
            max: 1 << 20,
        })
    }
}

fn open_thresholds() -> Thresholds {
    Thresholds {
        spawn_interval_secs: 0,
        machine_max_live: 1024,
        ..Thresholds::default()
    }
}

/// A host on its own data dir (its own machine registry), guarded by
/// `probe` and either `thresholds` or the config's `[delegation]` keys.
fn guarded(
    root: &Path,
    data: &Path,
    probe: std::sync::Arc<Dial>,
    thresholds: Option<Thresholds>,
    src: &str,
) -> std::sync::Arc<Delegator> {
    let setup = Setup {
        data_dir: Some(data.to_path_buf()),
        probe: Some(SharedProbe(probe)),
        thresholds,
        ..setup(src)
    };
    std::sync::Arc::new(
        Delegator::for_session(
            fake_tole(root, 30),
            root.to_path_buf(),
            false,
            Vec::new(),
            setup,
            &|_| None,
        )
        .unwrap(),
    )
}

fn pid_of(out: &Value) -> u32 {
    u32::try_from(out["pid"].as_u64().unwrap()).unwrap()
}

fn wait_dead(pid: u32) {
    for _ in 0..200 {
        if !crate::ledger_file::lease_alive(&crate::ledger_file::lease_of(pid)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("{pid} never died");
}

/// A data dir outside every test workspace.
fn data_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-delegate-data-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn log_file(root: &Path) -> PathBuf {
    log_path(&data_dir(), root).unwrap()
}

fn host(root: &Path, secs: u64, allow: &[&str], src: &str) -> std::sync::Arc<Delegator> {
    host_wt(root, secs, allow, src, false)
}

fn host_wt(
    root: &Path,
    secs: u64,
    allow: &[&str],
    src: &str,
    worktrees: bool,
) -> std::sync::Arc<Delegator> {
    let bin = fake_tole(root, secs);
    let allows = allow.iter().map(|s| s.to_string()).collect();
    std::sync::Arc::new(
        Delegator::for_session(
            bin,
            root.to_path_buf(),
            worktrees,
            allows,
            setup(src),
            &|_| None,
        )
        .unwrap(),
    )
}

fn tools(h: &std::sync::Arc<Delegator>, root: &Path) -> (DelegateTool, DelegatePollTool) {
    (
        DelegateTool {
            name: "delegate",
            host: h.clone(),
        },
        DelegatePollTool {
            name: "delegate_poll",
            host: h.clone(),
            inner: AgentPollTool::new(root.to_path_buf()),
        },
    )
}

/// Waits for a file the fake child writes.
fn wait_for(p: &Path) -> String {
    for _ in 0..200 {
        if let Ok(s) = std::fs::read_to_string(p) {
            if !s.is_empty() {
                return s;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("{} never appeared", p.display());
}

fn agent_dir_of(root: &Path, out: &Value) -> PathBuf {
    tole_core::agents::agent_dir(root, out["agent"].as_str().unwrap()).unwrap()
}

/// Polls until the child has exited; returns the settled poll output.
fn poll_done(poll: &DelegatePollTool, out: &Value) -> Value {
    for _ in 0..200 {
        let p = poll.execute(json!({"agent": out["agent"]})).unwrap();
        if p["running"] == json!(false) {
            return p;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("child never exited");
}

fn log_of(root: &Path) -> Delegation<FileStore> {
    Delegation::open(FileStore::new(log_file(root))).unwrap()
}

const PROFILE: &str = "[delegation]\ngoal_budget = 1000\ngoal_steps = 100\n\
    [profiles.coder]\nallow = [\"write_file\", \"run_command\"]\n\
    grants = [\"TOLE_DLG_A_TOKEN\", \"TOLE_DLG_B_TOKEN\"]\n";

#[test]
fn a_profile_child_is_a_budgeted_mission_with_parent_intersect_profile_and_its_grants() {
    // Held by this process: A (granted) and C (not granted); B is granted
    // but not held, so it cannot be passed on.
    std::env::set_var("TOLE_DLG_A_TOKEN", "value-a");
    std::env::set_var("TOLE_DLG_C_TOKEN", "value-c");
    let root = tmp("profile");
    let h = host(&root, 0, &["write_*"], PROFILE);
    let (start, _) = tools(&h, &root);
    let out = start
        .execute(json!({"task": "add the parser", "profile": "coder", "budget": {"tokens": 400}}))
        .unwrap();
    assert_eq!(out["allow"], json!(["write_file"]));
    assert_eq!(out["dropped"], json!(["run_command"]));
    assert_eq!(out["budget"]["tokens"], json!(400));
    assert_eq!(out["budget"]["steps"], json!(50), "half of what is left");
    let dir = agent_dir_of(&root, &out);
    let argv: Vec<String> = wait_for(&dir.join("argv"))
        .lines()
        .map(str::to_string)
        .collect();
    let pair = |k: &str| {
        argv.iter()
            .position(|a| a == k)
            .map(|i| argv[i + 1].as_str())
    };
    assert_eq!(argv[0], "mission");
    assert_eq!(pair("--max-steps"), Some("50"));
    assert_eq!(pair("--max-tokens"), Some("400"));
    assert_eq!(pair("--profile"), Some("coder"));
    assert_eq!(pair("--allow"), Some("write_file"));
    assert!(!argv.iter().any(|a| a == "run_command"), "{argv:?}");
    assert_eq!(argv.last().map(String::as_str), Some("add the parser"));
    let env = wait_for(&dir.join("env"));
    let node = out["node"].as_str().unwrap();
    assert!(env.contains("TOLE_AGENT_DEPTH=1\n"), "{env}");
    assert!(
        env.contains(&format!("TOLE_DELEGATION_NODE={node}\n")),
        "{env}"
    );
    assert!(
        env.contains("TOLE_DLG_A_TOKEN=value-a\n"),
        "granted and held"
    );
    assert!(
        !env.contains("TOLE_DLG_C_TOKEN"),
        "held but not granted: scrubbed"
    );
    // The log: started with its agent dir, and a lease on the canonical root.
    let d = log_of(&root);
    let n = d.poll(node).unwrap();
    assert_eq!(n.status, NodeStatus::Running);
    assert_eq!(n.session.as_deref(), Some(dir.to_str().unwrap()));
    let held = d.lease_conflict(root.to_str().unwrap(), "");
    assert_eq!(
        held,
        Some((root.to_str().unwrap().to_string(), node.to_string()))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_second_writer_in_one_workspace_is_refused_before_it_spawns() {
    let root = tmp("lease");
    let h = host(&root, 5, &["write_*"], "");
    let (start, _) = tools(&h, &root);
    start.execute(json!({"task": "one"})).unwrap();
    let e = start.execute(json!({"task": "two"})).unwrap_err();
    assert!(e.contains("write-leased"), "{e}");
    // Consult work needs no lease.
    let c = start
        .execute(json!({"task": "review", "mode": "consult"}))
        .unwrap();
    assert_eq!(c["mode"], json!("consult"));
    let dirs = std::fs::read_dir(tole_core::agents::agents_dir(&root))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("a-")
        })
        .count();
    assert_eq!(dirs, 2, "the refused writer never spawned");
    let _ = std::fs::remove_dir_all(&root);
}

fn write_usage(dir: &Path, prompt: u64, completion: u64) {
    let sessions = dir.join("sessions");
    let mut s = JsonlStorage::create(&sessions, "s1", None).unwrap();
    let out = s
        .commit(Commit::new().entry(NewEntry::root(
            EntryType::new(EntryType::MESSAGE),
            json!({"role": "assistant", "text": "x"}),
        )))
        .unwrap();
    s.commit(Commit::new().usage(UsageRecord {
        id: String::new(),
        entry_id: out[0].id.clone(),
        usage: json!({"prompt_tokens": prompt, "completion_tokens": completion}),
        cost_usd: None,
    }))
    .unwrap();
}

#[test]
fn a_settled_child_is_charged_its_measured_usage_or_its_whole_carve() {
    let root = tmp("usage");
    let h = host(
        &root,
        0,
        &[],
        "[delegation]\ngoal_budget = 1000\ngoal_steps = 100\n",
    );
    let (start, poll) = tools(&h, &root);
    let r = |t: u64| json!({"task": "t", "mode": "consult", "budget": {"tokens": t, "steps": 10}});
    let a = start.execute(r(300)).unwrap();
    wait_for(&agent_dir_of(&root, &a).join("env"));
    write_usage(&agent_dir_of(&root, &a), 30, 20);
    let p = poll_done(&poll, &a);
    let v = &p["delegation"];
    assert_eq!(v["used"], json!({"tokens": 50, "steps": 1}), "{p}");
    assert_eq!(v["status"], json!("incomplete"), "no report until part b");
    // No readable session usage: charged the whole carve.
    let b = start.execute(r(200)).unwrap();
    wait_for(&agent_dir_of(&root, &b).join("env"));
    let p = poll_done(&poll, &b);
    assert_eq!(
        p["delegation"]["used"],
        json!({"tokens": 200, "steps": 10}),
        "{p}"
    );
    // What is left: 1000 - 50 - 200, half of it by default.
    let c = start
        .execute(json!({"task": "t", "mode": "consult"}))
        .unwrap();
    assert_eq!(c["budget"]["tokens"], json!((1000 - 50 - 200) / 2));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_machine_guard_queues_and_a_poll_starts_the_queued_child() {
    let root = tmp("queue");
    let h = host(&root, 1, &[], "[delegation]\nmax_live_processes = 1\n");
    let (start, poll) = tools(&h, &root);
    let a = start
        .execute(json!({"task": "a", "mode": "consult"}))
        .unwrap();
    let b = start
        .execute(json!({"task": "b", "mode": "consult"}))
        .unwrap();
    assert_eq!(b["queued"], json!(true), "{b}");
    assert!(b.get("agent").is_none(), "nothing spawned yet");
    // A child that can write the queue file tries to widen its sibling:
    // the launch takes allow, budget and secrets from the log instead.
    let qfile = log_file(&root)
        .with_extension("queued")
        .join(format!("{}.json", b["node"].as_str().unwrap()));
    let mut forged: Value = serde_json::from_slice(&std::fs::read(&qfile).unwrap()).unwrap();
    for (k, v) in [
        ("allow", json!(["*"])),
        ("keep_secrets", json!(["HOME_TOKEN"])),
        (
            "budget",
            json!({"tokens": 999_999_999u64, "steps": 9999, "deadline": i64::MAX}),
        ),
        ("task", json!("forged")),
    ] {
        forged[k] = v;
    }
    std::fs::write(&qfile, forged.to_string()).unwrap();
    wait_for(&agent_dir_of(&root, &a).join("env"));
    let p = poll_done(&poll, &a);
    assert_eq!(
        p["delegation"]["started_from_queue"],
        json!([b["node"]]),
        "{p}"
    );
    let d = log_of(&root);
    assert_eq!(
        d.poll(b["node"].as_str().unwrap()).unwrap().status,
        NodeStatus::Running
    );
    let n = d.poll(b["node"].as_str().unwrap()).unwrap();
    let dir = PathBuf::from(n.session.clone().unwrap());
    let argv: Vec<String> = wait_for(&dir.join("argv"))
        .lines()
        .map(str::to_string)
        .collect();
    let pair = |k: &str| {
        argv.iter()
            .position(|a| a == k)
            .map(|i| argv[i + 1].clone())
    };
    assert_eq!(
        pair("--max-tokens"),
        Some(n.budget.tokens.to_string()),
        "{argv:?}"
    );
    assert!(!argv.iter().any(|a| a == "*"), "no forged allow: {argv:?}");
    assert_eq!(
        argv.last().map(String::as_str),
        Some("b"),
        "the recorded task"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn only_a_session_the_tree_lets_delegate_gets_the_tool() {
    let root = tmp("depth");
    let h = host(&root, 0, &[], "[delegation]\nmax_depth = 1\n");
    let (start, _) = tools(&h, &root);
    let out = start
        .execute(json!({"task": "t", "mode": "consult"}))
        .unwrap();
    let log = log_file(&root);
    let node = out["node"].as_str().unwrap().to_string();
    let child_env = |k: &str| match k {
        "TOLE_AGENT_DEPTH" => Some("1".to_string()),
        "TOLE_DELEGATION_LOG" => Some(log.to_string_lossy().into_owned()),
        "TOLE_DELEGATION_NODE" => Some(node.clone()),
        _ => None,
    };
    let make = |env: &dyn Fn(&str) -> Option<String>| {
        Delegator::for_session(
            PathBuf::from("tole"),
            root.clone(),
            false,
            vec![],
            setup(""),
            env,
        )
    };
    assert!(
        make(&child_env).is_none(),
        "depth 1 of max 1: no delegate tool"
    );
    // A classic agent_start child (depth 1, no delegation) gets none either.
    assert!(make(&|k: &str| (k == "TOLE_AGENT_DEPTH").then(|| "1".into())).is_none());
    assert!(make(&|_| None).is_some(), "a root session does");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_log_is_outside_the_workspace_and_forged_spawns_launch_nothing() {
    let root = tmp("forged");
    let h = host(
        &root,
        0,
        &["write_*"],
        "[delegation]\nmax_live_processes = 1\n",
    );
    let (start, poll) = tools(&h, &root);
    let a = start
        .execute(json!({"task": "a", "mode": "consult"}))
        .unwrap();
    let log = log_file(&root);
    assert!(
        !log.starts_with(&root),
        "{} is inside the workspace",
        log.display()
    );
    assert!(log.is_file());
    // Someone with write access to the log appends spawns that break caps:
    // a wider allow list, a budget past the carve, an unknown parent.
    let goal = a["node"]
        .as_str()
        .unwrap()
        .rsplit_once('.')
        .unwrap()
        .0
        .to_string();
    let forged = |node: &str, parent: &str, allow: Value, tokens: u64| {
        json!({"v": 1, "kind": "spawn", "node_id": node, "parent_id": parent,
            "profile": "x", "task": "forged", "mode": "execute",
            "budget": {"tokens": tokens, "steps": 1, "deadline": now() + 60},
            "allow": allow, "dropped": [], "workspace": root.to_str().unwrap(), "at": 0})
        .to_string()
    };
    let mut lines = String::new();
    for l in [
        forged(&format!("{goal}.2"), &goal, json!(["*"]), 10),
        forged(
            &format!("{goal}.2"),
            &goal,
            json!(["write_file"]),
            1_000_000_000,
        ),
        forged("zz.1", "zz", json!(["write_file"]), 10),
    ] {
        lines.push_str(&l);
        lines.push('\n');
    }
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(lines.as_bytes())
        .unwrap();
    wait_for(&agent_dir_of(&root, &a).join("env"));
    let p = poll_done(&poll, &a);
    assert_eq!(
        p["delegation"]["started_from_queue"],
        json!([]),
        "nothing forged is launched: {p}"
    );
    let d = log_of(&root);
    assert!(d.poll(&format!("{goal}.2")).is_none() && d.poll("zz.1").is_none());
    // The legitimate flow still works after the forged lines.
    let b = start
        .execute(json!({"task": "b", "mode": "consult"}))
        .unwrap();
    assert_eq!(b["node"], json!(format!("{goal}.2")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn with_worktrees_two_writers_run_in_parallel_each_on_its_own_lease() {
    let root = tmp("worktrees");
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let h = host_wt(&root, 5, &["write_*"], "", true);
    let (start, _) = tools(&h, &root);
    let a = start.execute(json!({"task": "one"})).unwrap();
    let b = start.execute(json!({"task": "two"})).unwrap();
    let d = log_of(&root);
    let leased = |out: &Value| {
        let wt = PathBuf::from(out["worktree"].as_str().unwrap());
        let wt = wt.canonicalize().unwrap();
        let node = out["node"].as_str().unwrap().to_string();
        assert_eq!(
            d.lease_conflict(wt.to_str().unwrap(), ""),
            Some((wt.to_string_lossy().into_owned(), node))
        );
    };
    leased(&a);
    leased(&b);
    assert_ne!(a["worktree"], b["worktree"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// A host whose root is spelled `root_as` (a symlink, a `..` path): every
/// key must still be the canonical root.
fn host_at(real: &Path, root_as: &Path, src: &str) -> std::sync::Arc<Delegator> {
    let bin = fake_tole(real, 0);
    std::sync::Arc::new(
        Delegator::for_session(
            bin,
            root_as.to_path_buf(),
            false,
            vec![],
            setup(src),
            &|_| None,
        )
        .unwrap(),
    )
}

#[test]
fn a_symlinked_or_dotted_root_still_settles_its_children() {
    let base = tmp("spelled");
    let real = base.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let dotted = base.join("real").join("..").join("real");
    for (i, spelled) in [link, dotted].into_iter().enumerate() {
        let h = host_at(
            &real,
            &spelled,
            "[delegation]\ngoal_budget = 1000\ngoal_steps = 100\n",
        );
        assert_eq!(h.root(), real.as_path(), "one canonical root");
        let start = DelegateTool {
            name: "delegate",
            host: h.clone(),
        };
        let poll = DelegatePollTool {
            name: "delegate_poll",
            host: h.clone(),
            inner: AgentPollTool::new(spelled.clone()),
        };
        let task = format!("t{i}");
        let out = start
            .execute(
                json!({"task": task, "mode": "consult", "budget": {"tokens": 300, "steps": 10}}),
            )
            .unwrap();
        let dir = agent_dir_of(&real, &out);
        wait_for(&dir.join("env"));
        write_usage(&dir, 30, 20);
        let p = poll_done(&poll, &out);
        assert_eq!(
            p["delegation"]["used"],
            json!({"tokens": 50, "steps": 1}),
            "{spelled:?}: {p}"
        );
        let d = log_of(&real);
        let node = d.poll(out["node"].as_str().unwrap()).unwrap();
        assert!(node.status.is_terminal(), "{:?}", node.status);
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_new_delegate_never_starts_ahead_of_an_older_queued_one() {
    let root = tmp("fifo");
    let h = host(&root, 1, &[], "[delegation]\nmax_live_processes = 1\n");
    let (start, _) = tools(&h, &root);
    let c = |t: &str| json!({"task": t, "mode": "consult", "budget": {"tokens": 10, "steps": 2}});
    let a = start.execute(c("a")).unwrap();
    let b = start.execute(c("b")).unwrap();
    assert_eq!(b["queued"], json!(true));
    // A exits; nobody polls. The next delegate recovers A, starts B first
    // and queues C behind it.
    wait_for(&agent_dir_of(&root, &a).join("env"));
    let pid = u32::try_from(a["pid"].as_u64().unwrap()).unwrap();
    for _ in 0..200 {
        if !crate::ledger_file::lease_alive(&crate::ledger_file::lease_of(pid)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let c_out = start.execute(c("c")).unwrap();
    assert_eq!(c_out["queued"], json!(true), "C waits behind B: {c_out}");
    assert_eq!(
        c_out["started_from_queue"][0]["node"], b["node"],
        "B started first: {c_out}"
    );
    let d = log_of(&root);
    assert_eq!(
        d.poll(b["node"].as_str().unwrap()).unwrap().status,
        NodeStatus::Running
    );
    assert_eq!(
        d.poll(c_out["node"].as_str().unwrap()).unwrap().status,
        NodeStatus::Queued
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn low_memory_queues_in_order_and_a_later_delegate_starts_them_when_it_recovers() {
    let root = tmp("guard-mem");
    let data = tmp("guard-mem-data");
    let dial = std::sync::Arc::new(Dial(AtomicU64::new(100)));
    let h = guarded(&root, &data, dial.clone(), Some(open_thresholds()), "");
    let (start, _) = tools(&h, &root);
    let c = |t: &str| json!({"task": t, "mode": "consult"});
    let a = start.execute(c("a")).unwrap();
    assert_eq!(a["queued"], json!(true), "{a}");
    assert!(a.get("agent").is_none(), "nothing spawned: {a}");
    let why = a["queued_reason"].as_str().unwrap();
    assert!(why.contains("memory 100 MB free"), "{a}");
    assert!(a["waited_secs"].as_i64().unwrap() <= 1, "{a}");
    std::thread::sleep(std::time::Duration::from_millis(2100));
    // The head waits, so the queue holds: B waits behind A.
    let b = start.execute(c("b")).unwrap();
    assert_eq!(b["queued"], json!(true), "{b}");
    assert!(
        b["queue"][0]["waited_secs"].as_i64().unwrap() >= 2,
        "A's wait shows: {b}"
    );
    assert_eq!(b["queue"][0]["node"], a["node"], "{b}");
    assert!(b["queue"][0]["queued_reason"]
        .as_str()
        .unwrap()
        .contains("memory"));
    assert!(
        b["queued_reason"].as_str().unwrap().contains("behind"),
        "{b}"
    );
    // Memory recovers: the next delegate starts A, then B, then itself.
    dial.0.store(1 << 20, Relaxed);
    let c3 = start.execute(c("c")).unwrap();
    let order: Vec<&Value> = c3["started_from_queue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| &v["node"])
        .collect();
    assert_eq!(order, vec![&a["node"], &b["node"]], "{c3}");
    assert!(c3.get("agent").is_some(), "C started too: {c3}");
    // Every delegated child runs at least at child_nice (10).
    #[cfg(target_os = "linux")]
    {
        let nice: i64 = wait_for(&agent_dir_of(&root, &c3).join("nice"))
            .trim()
            .parse()
            .unwrap();
        assert!(nice >= 10, "nice {nice}");
    }
    for v in c3["started_from_queue"].as_array().unwrap() {
        kill(pid_of(&v["result"]));
    }
    kill(pid_of(&c3));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn two_goals_share_the_machine_count_and_a_dead_child_frees_its_slot() {
    let data = tmp("guard-machine-data");
    let (ra, rb) = (tmp("guard-machine-a"), tmp("guard-machine-b"));
    let one = Thresholds {
        machine_max_live: 1,
        ..open_thresholds()
    };
    let ha = guarded(
        &ra,
        &data,
        std::sync::Arc::new(Dial::roomy()),
        Some(one),
        "",
    );
    // B takes its thresholds from the user config, through Setup.
    let hb = guarded(
        &rb,
        &data,
        std::sync::Arc::new(Dial::roomy()),
        None,
        "[delegation]\nmachine_max_live = 1\nspawn_interval_secs = 1\n",
    );
    let (sa, _) = tools(&ha, &ra);
    let (sb, _) = tools(&hb, &rb);
    let a = sa.execute(json!({"task": "a", "mode": "consult"})).unwrap();
    assert!(a.get("agent").is_some(), "{a}");
    // Another goal, another log: it still sees A on this machine.
    let b1 = sb
        .execute(json!({"task": "b1", "mode": "consult"}))
        .unwrap();
    assert_eq!(b1["queued"], json!(true), "{b1}");
    let why = b1["queued_reason"].as_str().unwrap();
    assert!(
        why.contains("1 delegated children live on this machine (max 1)"),
        "{b1}"
    );
    // A dies without anyone settling it: its registered lease is dead, so
    // its slot frees for B's head; B's second spawn waits behind it.
    kill(pid_of(&a));
    wait_dead(pid_of(&a));
    std::thread::sleep(std::time::Duration::from_millis(2100)); // spawn interval
    let b2 = sb
        .execute(json!({"task": "b2", "mode": "consult"}))
        .unwrap();
    assert_eq!(b2["started_from_queue"][0]["node"], b1["node"], "{b2}");
    assert_eq!(b2["queued"], json!(true), "{b2}");
    assert!(
        b2["queued_reason"].as_str().unwrap().contains("max 1"),
        "{b2}"
    );
    kill(pid_of(&b2["started_from_queue"][0]["result"]));
    for d in [&ra, &rb, &data] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn doctor_shows_the_headroom_and_a_spawn_stuck_in_the_queue() {
    let data = tmp("guard-doctor-data");
    let root = tmp("guard-doctor");
    let log = log_path(&data, &root).unwrap();
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut d = Delegation::open(FileStore::new(log)).unwrap();
    let t0 = now() - 1000;
    let allow = vec!["read_file".to_string()];
    d.begin(
        &Goal {
            id: "g",
            task: "orchestrator",
            budget: Budget {
                tokens: 100,
                steps: 10,
                deadline: t0 + 5000,
            },
            allow: &allow,
            max_depth: 2,
            max_live: 1,
        },
        t0,
    )
    .unwrap();
    let ws = root.to_string_lossy().into_owned();
    let req = DelegateRequest {
        profile: "-",
        task: "stuck",
        mode: Mode::Consult,
        budget: BudgetRequest::default(),
        profile_allow: &allow,
        workspace: Some(&ws),
    };
    let h = d.delegate("g", &req, Spend::default(), t0).unwrap();
    let setup = Setup {
        data_dir: Some(data.clone()),
        probe: Some(SharedProbe(std::sync::Arc::new(Dial(AtomicU64::new(100))))),
        thresholds: Some(open_thresholds()),
        ..setup("")
    };
    let lines = crate::machine_guard::doctor_lines(&setup, Some(&root), now()).join("\n");
    assert!(
        lines.contains("! memory 100 MB free (a spawn needs 1536 MB)"),
        "{lines}"
    );
    assert!(lines.contains("✓ load 0.00 per CPU (max 1.50)"), "{lines}");
    assert!(
        lines.contains(&format!("✓ disk 1048576 MB free on {}", root.display())),
        "{lines}"
    );
    assert!(
        lines.contains("✓ 0 of 1024 delegated children live"),
        "{lines}"
    );
    assert!(
        lines.contains(&format!("spawn `{}` has waited 16 min", h.node_id)),
        "{lines}"
    );
    // A data dir not made yet is checked on the filesystem it will live on,
    // with the real probe: no "cannot read disk" note.
    let fresh = Setup {
        data_dir: Some(data.join("not-yet")),
        probe: None,
        ..setup
    };
    let lines = crate::machine_guard::doctor_lines(&fresh, Some(&root), now()).join("\n");
    assert!(lines.contains("disk"), "{lines}");
    #[cfg(target_os = "linux")]
    assert!(!lines.contains("cannot read"), "{lines}");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}
