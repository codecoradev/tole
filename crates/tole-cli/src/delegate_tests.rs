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
    }
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
    let bin = fake_tole(root, secs);
    let allows = allow.iter().map(|s| s.to_string()).collect();
    std::sync::Arc::new(
        Delegator::for_session(bin, root.to_path_buf(), false, allows, setup(src), &|_| {
            None
        })
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
    let h = host(&root, 0, &[], "[delegation]\nmax_live_processes = 1\n");
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
