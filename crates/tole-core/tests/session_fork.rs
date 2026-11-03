//! #210: forking a session. A fork is a NEW session file holding the
//! lineage of the source up to a settled entry; the source is never
//! touched (write-once), and every reader (replay, todo hydration) works on
//! the fork unchanged because the copied prefix is a plain linear lineage.

use serde_json::json;

use tole_core::entry::{EntryType, NewEntry};
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::{Commit, JsonlStorage, Storage, StorageError};
use tole_core::todo::TodoState;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-fork-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn msg(role: &str, text: &str) -> NewEntry {
    NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        json!({"role": role, "text": text}),
    )
}

/// Settles a turn: user message, optional todo_write sandwich, final answer.
fn turn(s: &mut JsonlStorage, user: &str, answer: &str, todo: Option<serde_json::Value>) {
    s.commit(Commit::new().entry(msg("user", user))).unwrap();
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
        .unwrap();
    if let Some(list) = todo {
        let seq = s.state().seq;
        let intent = s
            .commit(
                Commit::new()
                    .entry(NewEntry::root(
                        EntryType::new(EntryType::INTENT),
                        json!({"tool": "todo_write"}),
                    ))
                    .transition(StateTransition::from(seq, Pc::ToolCall)),
            )
            .unwrap();
        let seq = s.state().seq;
        s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Executing)))
            .unwrap();
        let seq = s.state().seq;
        s.commit(
            Commit::new()
                .entry(NewEntry::with_parent(
                    &intent[0].id,
                    EntryType::new(EntryType::TOOL_RESULT),
                    json!({"ok": true, "output": list}),
                ))
                .transition(StateTransition::from(seq, Pc::Settling)),
        )
        .unwrap();
        let seq = s.state().seq;
        s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
            .unwrap();
    }
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(msg("assistant", answer))
            .transition(StateTransition::from(seq, Pc::Final)),
    )
    .unwrap();
}

fn todo_list(entries: &[tole_core::entry::Entry]) -> String {
    let st = TodoState::new();
    st.hydrate(entries);
    let out =
        tole_core::tool::Tool::execute(&tole_core::todo::TodoReadTool::new(st), json!({})).unwrap();
    out.to_string()
}

#[test]
fn fork_copies_the_lineage_up_to_a_settled_entry_and_leaves_the_source_alone() {
    let dir = tmpdir("basic");
    let mut src = JsonlStorage::create_with(&dir, "src", None, Some("be terse")).unwrap();
    turn(&mut src, "approach A", "did A", None);
    let a_answer = src
        .entries()
        .iter()
        .find(|e| e.payload["text"] == json!("did A"))
        .unwrap()
        .id
        .clone();
    turn(&mut src, "keep going", "more A", None);
    let src_len = src.entries().len();
    let src_bytes = std::fs::read(dir.join("src.jsonl")).unwrap();
    drop(src);

    let f = JsonlStorage::fork(dir.join("src.jsonl"), &dir, "fork1", Some(&a_answer)).unwrap();
    assert_eq!(f.session_id(), "fork1");
    assert_eq!(f.system_prompt(), Some("be terse"));
    let texts: Vec<_> = f
        .entries()
        .iter()
        .map(|e| e.payload["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts, ["approach A", "did A"], "cut at the chosen entry");
    assert_eq!(
        f.state().pc,
        Pc::Final,
        "a settled fork resumes like a chat"
    );
    let origin = f.forked_from().unwrap();
    assert_eq!(
        (origin.session.as_str(), origin.entry.as_str()),
        ("src", a_answer.as_str())
    );
    drop(f);

    // Source file is byte-identical; the fork survives a reopen.
    assert_eq!(std::fs::read(dir.join("src.jsonl")).unwrap(), src_bytes);
    assert!(src_len > 2);
    let mut re = JsonlStorage::open(dir.join("fork1.jsonl")).unwrap();
    assert_eq!(re.forked_from().unwrap().session, "src");
    // The branch diverges: new work on the fork never reaches the source.
    turn(&mut re, "approach B", "did B", None);
    assert_eq!(re.entries().len(), 4);
    let orig = JsonlStorage::open_read_only(dir.join("src.jsonl")).unwrap();
    assert!(!orig
        .entries()
        .iter()
        .any(|e| e.payload["text"] == json!("did B")));
}

#[test]
fn fork_defaults_to_the_last_settled_entry() {
    let dir = tmpdir("default");
    let mut src = JsonlStorage::create(&dir, "src", None).unwrap();
    turn(&mut src, "one", "r1", None);
    turn(&mut src, "two", "r2", None);
    // A dangling, unanswered prompt after the last settled answer.
    src.commit(Commit::new().entry(msg("user", "three")))
        .unwrap();
    drop(src);
    let f = JsonlStorage::fork(dir.join("src.jsonl"), &dir, "f", None).unwrap();
    let last = f.entries().last().unwrap();
    assert_eq!(last.payload["text"], json!("r2"));
}

#[test]
fn fork_rejects_unsettled_unknown_and_taken_targets() {
    let dir = tmpdir("reject");
    let mut src = JsonlStorage::create(&dir, "src", None).unwrap();
    turn(&mut src, "one", "r1", None);
    let user = src.entries()[0].id.clone();
    drop(src);
    let p = dir.join("src.jsonl");
    // A user prompt is not a settled point.
    assert!(matches!(
        JsonlStorage::fork(&p, &dir, "f1", Some(&user)),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        JsonlStorage::fork(&p, &dir, "f2", Some("nope")),
        Err(StorageError::Invalid(_))
    ));
    // Bad ids and an existing target are refused; nothing is left behind.
    assert!(JsonlStorage::fork(&p, &dir, "../x", None).is_err());
    assert!(JsonlStorage::fork(&p, &dir, "src", None).is_err());
    assert!(!dir.join("f1.jsonl").exists() && !dir.join("f2.jsonl").exists());
    // No settled entry at all.
    let mut empty = JsonlStorage::create(&dir, "empty", None).unwrap();
    empty
        .commit(Commit::new().entry(msg("user", "hi")))
        .unwrap();
    drop(empty);
    assert!(matches!(
        JsonlStorage::fork(dir.join("empty.jsonl"), &dir, "f3", None),
        Err(StorageError::Invalid(_))
    ));
}

#[test]
fn tool_state_hydrates_per_branch() {
    let dir = tmpdir("todo");
    let mut src = JsonlStorage::create(&dir, "src", None).unwrap();
    let l1 = json!([{"id": "t1", "content": "A", "status": "pending"}]);
    let l2 = json!([{"id": "t2", "content": "later", "status": "pending"}]);
    turn(&mut src, "plan", "planned", Some(l1.clone()));
    let cut = src.entries().last().unwrap().id.clone();
    turn(&mut src, "replan", "replanned", Some(l2.clone()));
    drop(src);
    let f = JsonlStorage::fork(dir.join("src.jsonl"), &dir, "f", Some(&cut)).unwrap();
    let on_fork = todo_list(f.entries());
    let on_src = todo_list(
        JsonlStorage::open_read_only(dir.join("src.jsonl"))
            .unwrap()
            .entries(),
    );
    assert!(on_fork.contains("\"A\""), "{on_fork}");
    assert!(!on_fork.contains("later"), "{on_fork}");
    assert!(on_src.contains("later"), "{on_src}");
}
