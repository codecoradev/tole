//! JSONL storage tests — cover every E1 acceptance criterion.
//!
//! - Format documentation lives in the `storage` module docs (rustdoc).
//! - Older format version → replay cleanly (roundtrip).
//! - Newer format version → hard error.
//! - All ops through the `Storage` trait (callers use `dyn Storage` here to
//!   prove no backend-specific code leaks).

use serde_json::json;

use cora_agent_core::entry::{EntryType, NewEntry};
use cora_agent_core::register::{is_valid_namespace, RegisterWrite};
use cora_agent_core::state::{Pc, StateTransition};
use cora_agent_core::storage::{Commit, JsonlStorage, Storage, StorageError, UsageRecord};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cora-agent-e1-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn message(payload: serde_json::Value) -> NewEntry {
    NewEntry::root(EntryType::new(EntryType::MESSAGE), payload)
}

#[test]
fn roundtrip_full_session_through_trait() {
    let dir = tmpdir("roundtrip");
    let path = dir.join("s1.jsonl");

    // Write a full session: entries, registers, transition, usage.
    let mut s = JsonlStorage::create(&dir, "s1", Some("/tmp".into())).unwrap();
    let root = s
        .commit(Commit::new().entry(message(json!({"role":"user","text":"hi"}))))
        .unwrap();
    let root_id = root[0].id.clone();

    let observed = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::with_parent(
                &root_id,
                EntryType::new(EntryType::INTENT),
                json!({"tool":"read_file"}),
            ))
            .register(RegisterWrite::set(
                "pending",
                "op_1",
                json!({"tool":"read_file"}),
            ))
            .transition(StateTransition::from(observed, Pc::ToolCall)),
    )
    .unwrap();
    s.commit(
        Commit::new()
            .register(RegisterWrite::delete("pending", "op_1"))
            .register(RegisterWrite::set("lane.leaf", "main", json!("e_2")))
            .usage(UsageRecord {
                id: String::new(),
                entry_id: root_id.clone(),
                usage: json!({"input_tokens": 10, "output_tokens": 5}),
                cost_usd: Some(0.001),
            }),
    )
    .unwrap();

    // Close (drop) and reopen through the trait object.
    drop(s);
    let reopened: Box<dyn Storage> = Box::new(JsonlStorage::open(&path).unwrap());

    assert_eq!(reopened.session_id(), "s1");
    assert_eq!(reopened.entries().len(), 2);
    assert_eq!(reopened.state().pc, Pc::ToolCall);
    assert_eq!(reopened.entry(&root_id).unwrap().payload["text"], "hi");
    assert_eq!(reopened.children(&root_id).len(), 1);
    assert_eq!(
        reopened.get_register("lane.leaf", "main"),
        Some(&json!("e_2"))
    );
    assert!(reopened.get_register("pending", "op_1").is_none()); // deleted
    let lane = reopened.list_register("lane.leaf");
    assert_eq!(lane, vec![("main", &json!("e_2"))]);
}

#[test]
fn reopen_after_compact_preserves_logical_state() {
    let dir = tmpdir("compact");
    let mut s = JsonlStorage::create(&dir, "s2", None).unwrap();

    let e1 = s
        .commit(Commit::new().entry(message(json!({"n":1}))))
        .unwrap();
    let id1 = e1[0].id.clone();
    // Register churn: set, overwrite, delete (dead bytes).
    for n in 0..5 {
        s.commit(Commit::new().register(RegisterWrite::set("fact", "counter", json!(n))))
            .unwrap();
    }
    s.commit(Commit::new().register(RegisterWrite::delete("fact", "counter")))
        .unwrap();
    s.commit(Commit::new().register(RegisterWrite::set("fact", "alive", json!("yes"))))
        .unwrap();

    let seq_before = s.last_seq();
    s.compact().unwrap();

    drop(s);
    let reopened = JsonlStorage::open(dir.join("s2.jsonl")).unwrap();
    assert_eq!(reopened.entries().len(), 1);
    assert_eq!(reopened.entry(&id1).unwrap().payload["n"], 1);
    assert!(reopened.get_register("fact", "counter").is_none());
    assert_eq!(reopened.get_register("fact", "alive"), Some(&json!("yes")));
    assert!(reopened.last_seq() >= seq_before);
    // Post-compact file must still accept commits.
    let mut reopened = reopened;
    reopened
        .commit(Commit::new().entry(message(json!({"after":"compact"}))))
        .unwrap();
}

#[test]
fn older_format_version_replays_cleanly() {
    let dir = tmpdir("older");
    // Hand-write a v1 file (this build writes v1; simulate an even older
    // consumer by bumping our const check is covered by newer-version test).
    let body = concat!(
        r#"{"v":1,"kind":"header","id":"old","storageVersion":1,"created_at":1,"cwd":""}"#,
        "\n",
        r#"{"kind":"entry","seq":1,"id":"e_1","type":"message","timestamp":1,"payload":{"a":1}}"#,
        "\n",
    );
    std::fs::write(dir.join("old.jsonl"), body).unwrap();
    let s = JsonlStorage::open(dir.join("old.jsonl")).unwrap();
    assert_eq!(s.entries().len(), 1);
    assert_eq!(s.last_seq(), 1);
}

#[test]
fn newer_format_version_is_hard_error() {
    let dir = tmpdir("newer");
    let body = concat!(
        r#"{"v":1,"kind":"header","id":"fut","storageVersion":999,"created_at":1,"cwd":""}"#,
        "\n",
    );
    std::fs::write(dir.join("fut.jsonl"), body).unwrap();
    let err = match JsonlStorage::open(dir.join("fut.jsonl")) {
        Err(e) => e,
        Ok(_) => panic!("expected UnsupportedVersion error"),
    };
    match err {
        StorageError::UnsupportedVersion { file, supported } => {
            assert_eq!(file, 999);
            assert_eq!(supported, cora_agent_core::STORAGE_VERSION);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn torn_final_line_is_discarded_whole() {
    let dir = tmpdir("torn");
    let mut s = JsonlStorage::create(&dir, "t", None).unwrap();
    s.commit(Commit::new().entry(message(json!({"ok":1}))))
        .unwrap();
    drop(s);

    // Simulate crash mid-append: truncate the last line halfway.
    let path = dir.join("t.jsonl");
    let raw = std::fs::read(&path).unwrap();
    let mut truncated = raw.clone();
    // Find last newline before end; cut inside the final line.
    let cut = raw.len() - 6;
    truncated.truncate(cut);
    std::fs::write(&path, truncated).unwrap();

    let mut reopened = JsonlStorage::open(&path).unwrap();
    // Header replayed; the torn entry line was discarded whole.
    assert_eq!(reopened.session_id(), "t");
    assert_eq!(reopened.entries().len(), 0);
    assert_eq!(reopened.last_seq(), 0);
    // Torn bytes were physically truncated: a fresh commit must NOT
    // concatenate onto the fragment (regression for the CodeCora finding).
    reopened
        .commit(Commit::new().entry(message(json!({"after":1}))))
        .unwrap();
    drop(reopened);
    let again = JsonlStorage::open(&path).unwrap();
    assert_eq!(again.entries().len(), 1);
    assert_eq!(again.entries()[0].payload["after"], 1);
}

#[test]
fn stale_transition_is_rejected_not_overwritten() {
    let dir = tmpdir("stale");
    let mut s = JsonlStorage::create(&dir, "st", None).unwrap();
    let observed = s.state().seq; // 0
                                  // A competing writer commits first.
    s.commit(Commit::new().transition(StateTransition::from(observed, Pc::Planning)))
        .unwrap();
    // Our stale write (still expecting seq 0) must be rejected.
    let err = s
        .commit(Commit::new().transition(StateTransition::from(observed, Pc::Planning)))
        .unwrap_err();
    assert!(matches!(err, StorageError::StaleTransition { .. }));
    // Machine is untouched by the rejected commit.
    assert_eq!(s.state().pc, Pc::Planning);
}

#[test]
fn invalid_commits_are_rejected_before_any_write() {
    let dir = tmpdir("invalid");
    let mut s = JsonlStorage::create(&dir, "inv", None).unwrap();
    let before = std::fs::read(dir.join("inv.jsonl")).unwrap();

    // Unknown parent.
    let e = s
        .commit(Commit::new().entry(NewEntry::with_parent(
            "nope",
            EntryType::new(EntryType::MESSAGE),
            json!({}),
        )))
        .unwrap_err();
    assert!(matches!(e, StorageError::Invalid(_)));

    // Unknown namespace family.
    let e = s
        .commit(Commit::new().register(RegisterWrite::set("bogus.ns", "k", json!(1))))
        .unwrap_err();
    assert!(matches!(e, StorageError::Invalid(_)));

    // Set without value.
    let e = s
        .commit(Commit::new().register(RegisterWrite {
            op: cora_agent_core::register::RegisterOp::Set,
            namespace: "fact".into(),
            key: "k".into(),
            value: None,
        }))
        .unwrap_err();
    assert!(matches!(e, StorageError::Invalid(_)));

    // Nothing was written by any failed commit.
    let after = std::fs::read(dir.join("inv.jsonl")).unwrap();
    assert_eq!(before, after);
}

#[test]
fn intra_commit_parent_chaining_works() {
    let dir = tmpdir("chain");
    let mut s = JsonlStorage::create(&dir, "ch", None).unwrap();
    let committed = s
        .commit(Commit::new().entry(message(json!({"i":1}))))
        .unwrap();
    let parent = committed[0].id.clone();

    // Two entries in one commit: the second references the first via an
    // explicit id assigned within this same commit line.
    let a = NewEntry {
        id: Some("a".into()),
        parent_id: Some(parent.clone()),
        kind: EntryType::new(EntryType::INTENT),
        payload: json!({"i":2}),
        timestamp: 0,
    };
    let b = NewEntry::with_parent("a", EntryType::new(EntryType::TOOL_RESULT), json!({"i":3}));
    let committed = s.commit(Commit::new().entry(a).entry(b)).unwrap();
    assert_eq!(committed.len(), 2);
    assert_eq!(committed[0].id, "a");
    assert_eq!(committed[1].parent_id.as_deref(), Some("a"));
    assert_eq!(s.children("a").len(), 1);

    // Empty-string parent is not a root — it is an unknown id.
    let err = s
        .commit(Commit::new().entry(NewEntry::with_parent(
            "",
            EntryType::new(EntryType::MESSAGE),
            json!({}),
        )))
        .unwrap_err();
    assert!(matches!(err, StorageError::Invalid(_)));
}

#[test]
fn namespace_validation() {
    assert!(is_valid_namespace("lane"));
    assert!(is_valid_namespace("lane.leaf"));
    assert!(is_valid_namespace("op.state"));
    assert!(!is_valid_namespace("bogus"));
    assert!(!is_valid_namespace("lane."));
    assert!(!is_valid_namespace("lane.a.b"));
}

#[test]
fn duplicate_entry_id_rejected() {
    let dir = tmpdir("dup");
    let mut s = JsonlStorage::create(&dir, "dup", None).unwrap();
    let e1 = s.commit(Commit::new().entry(message(json!({})))).unwrap();
    let dup = NewEntry {
        id: Some(e1[0].id.clone()),
        parent_id: None,
        kind: EntryType::new(EntryType::MESSAGE),
        payload: json!({}),
        timestamp: 0,
    };
    let err = s.commit(Commit::new().entry(dup)).unwrap_err();
    assert!(matches!(err, StorageError::Invalid(_)));
}

#[test]
fn usage_survives_compact_in_same_session() {
    // Regression: commit() wrote usage rows to the file but never pushed
    // them to the in-memory list, so compact() (which rewrites from that
    // list) silently dropped usage rows committed since open.
    let dir = tmpdir("usage-compact");
    let mut s = JsonlStorage::create(&dir, "uc", None).unwrap();
    let e = s.commit(Commit::new().entry(message(json!({})))).unwrap();
    s.commit(Commit::new().usage(UsageRecord {
        id: String::new(),
        entry_id: e[0].id.clone(),
        usage: json!({"input_tokens": 7}),
        cost_usd: Some(0.002),
    }))
    .unwrap();
    // Visible in-memory before compact...
    assert_eq!(s.usages().len(), 1);
    s.compact().unwrap();
    // ...and still present in the rewritten file.
    assert_eq!(s.usages().len(), 1);
    drop(s);
    let reopened = JsonlStorage::open(dir.join("uc.jsonl")).unwrap();
    assert_eq!(reopened.usages().len(), 1);
    assert_eq!(reopened.usages()[0].usage["input_tokens"], 7);
    assert_eq!(reopened.usages()[0].cost_usd, Some(0.002));
}

#[test]
fn seq_is_monotonic_across_mixed_records() {
    let dir = tmpdir("seq");
    let mut s = JsonlStorage::create(&dir, "sq", None).unwrap();
    let e = s.commit(Commit::new().entry(message(json!({})))).unwrap();
    let seq0 = e[0].seq;
    let committed = s
        .commit(
            Commit::new()
                .register(RegisterWrite::set("fact", "k", json!(1)))
                .usage(UsageRecord {
                    id: String::new(),
                    entry_id: e[0].id.clone(),
                    usage: json!({}),
                    cost_usd: None,
                }),
        )
        .unwrap();
    assert!(committed.is_empty());
    assert_eq!(s.last_seq(), seq0 + 2); // register + usage consumed seqs
    drop(s);
    let reopened = JsonlStorage::open(dir.join("sq.jsonl")).unwrap();
    assert_eq!(reopened.last_seq(), seq0 + 2);
}
