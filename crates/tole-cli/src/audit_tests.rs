use super::*;
use tole_core::delegation::{
    Budget, BudgetRequest, Check, DelegateRequest, Goal, Outcome, Report, Spend,
};
use tole_core::ledger::Lease;

const T: i64 = 1_000_000;
const SECRET: &str = "sk-audit-0123456789abcdef";

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-audit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn secrets() -> SecretSet {
    SecretSet::collect([("AUDIT_API_KEY".to_string(), SECRET.to_string())], [])
}

/// A goal with a settled execute child (with a report that quotes the
/// secret), a running consult grandchild and a dropped pattern.
fn tree(log: &Path) {
    let mut d = Delegation::open(FileStore::new(log.to_path_buf())).unwrap();
    let allow = vec!["write_*".to_string()];
    d.begin(
        &Goal {
            id: "g",
            task: "orchestrator",
            budget: Budget {
                tokens: 1000,
                steps: 100,
                deadline: T + 3600,
            },
            allow: &allow,
            max_depth: 2,
            max_live: 8,
        },
        T,
    )
    .unwrap();
    let long_task = format!("fix it\u{1b}[2J {} {}", "word ".repeat(100), SECRET);
    let profile_allow = vec!["write_file".to_string(), "run_command".to_string()];
    let a = d
        .delegate(
            "g",
            &DelegateRequest {
                profile: "coder",
                task: &long_task,
                mode: Mode::Execute,
                budget: BudgetRequest {
                    tokens: Some(400),
                    steps: Some(20),
                    minutes: None,
                },
                profile_allow: &profile_allow,
                workspace: Some("/w"),
            },
            Spend::default(),
            T,
        )
        .unwrap();
    d.start(
        &a.node_id,
        Lease {
            pid: 1,
            start_time: 1,
        },
        Some("/w/tole-agents/a-1"),
        T,
    )
    .unwrap();
    d.lease_write(&a.node_id, "/w", T).unwrap();
    let c = d
        .delegate(
            &a.node_id,
            &DelegateRequest {
                profile: "-",
                task: "review",
                mode: Mode::Consult,
                budget: BudgetRequest::default(),
                profile_allow: &[],
                workspace: Some("/w"),
            },
            Spend::default(),
            T,
        )
        .unwrap();
    let _ = c;
    let report = Report {
        summary: format!("changed the parser; key was {SECRET}"),
        files: vec!["src/a.rs".into()],
        diff_ref: Some("branch x".into()),
        checks: vec![Check {
            command: "cargo test".into(),
            result: "passed".into(),
        }],
        risks: vec![],
    };
    d.settle(
        &a.node_id,
        Outcome::Ok,
        Spend {
            tokens: 120,
            steps: 4,
        },
        Some(report),
        None,
        T,
    )
    .unwrap();
}

#[test]
fn the_tree_is_printed_capped_and_redacted() {
    let d = tmp("tree");
    let log = d.join("delegation.jsonl");
    tree(&log);
    let out = audit(&log, Some("g"), false, &secrets()).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("g orchestrator running [execute] carve tokens 1000"),
        "{out}"
    );
    assert!(
        out.contains("  g.1 coder incomplete [execute] charged tokens"),
        "{out}"
    );
    assert!(
        out.contains("allow: write_file") && out.contains("dropped: run_command"),
        "{out}"
    );
    assert!(
        out.contains("    g.1.1 (no profile) queued [consult] carve"),
        "{out}"
    );
    assert!(out.contains("report: changed the parser"), "{out}");
    // Never the secret, never a control character, never the full prompt.
    assert!(!out.contains(SECRET), "{out}");
    assert!(out.contains("«AUDIT_API_KEY»"), "{out}");
    assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
    let task = lines
        .iter()
        .find(|l| l.trim_start().starts_with("task: fix it"))
        .unwrap();
    assert!(task.chars().count() < TASK_CHARS + 20, "{task}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn json_lists_goals_and_nodes_with_every_field() {
    let d = tmp("json");
    let log = d.join("delegation.jsonl");
    tree(&log);
    let goals: Value = serde_json::from_str(&audit(&log, None, true, &secrets()).unwrap()).unwrap();
    assert_eq!(goals["goals"][0]["node"], json!("g"));
    assert_eq!(goals["goals"][0]["nodes"], json!(3));
    let v: Value =
        serde_json::from_str(&audit(&log, Some("g"), true, &secrets()).unwrap()).unwrap();
    let a = &v["nodes"][1];
    assert_eq!(a["node"], json!("g.1"));
    assert_eq!(a["profile"], json!("coder"));
    assert_eq!(a["used"], json!({"tokens": 120, "steps": 4}));
    assert_eq!(a["budget"]["tokens"], json!(400));
    assert_eq!(a["allow"], json!(["write_file"]));
    assert_eq!(a["dropped"], json!(["run_command"]));
    assert_eq!(a["leases"], json!([]), "the lease ended when it settled");
    assert_eq!(a["report"]["files"], json!(1));
    assert_eq!(v["nodes"][2]["profile"], Value::Null, "no profile");
    assert!(!v.to_string().contains(SECRET));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn audit_writes_nothing_and_refuses_an_unknown_goal() {
    let d = tmp("ro");
    let log = d.join("delegation.jsonl");
    tree(&log);
    // A torn last line stays as it is: audit never repairs.
    let mut bytes = std::fs::read(&log).unwrap();
    bytes.extend_from_slice(b"{\"kind\":\"settle\"");
    std::fs::write(&log, &bytes).unwrap();
    let before: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    audit(&log, Some("g"), false, &secrets()).unwrap();
    audit(&log, None, true, &secrets()).unwrap();
    assert_eq!(std::fs::read(&log).unwrap(), bytes, "the log is unchanged");
    let after: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(before, after, "no lock or other file created");
    assert!(
        audit(&log, Some("g.1"), false, &secrets()).is_err(),
        "not a goal"
    );
    assert!(audit(&log, Some("zz"), false, &secrets()).is_err());
    // No log yet: an empty listing, not an error.
    let none = audit(&d.join("missing.jsonl"), None, false, &secrets()).unwrap();
    assert!(none.starts_with("no delegation goals"), "{none}");
    let _ = std::fs::remove_dir_all(&d);
}
