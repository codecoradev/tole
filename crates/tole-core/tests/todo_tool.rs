//! The `todo_write` / `todo_read` tools (issue #198), moved from `src/tools/todo.rs`
//! so the crate's source stays under its size guard: they use the public API only.

use serde_json::{json, Value};
use std::sync::Arc;
use tole_core::todo::{TodoReadTool, TodoState, TodoWriteTool};
use tole_core::tool::Tool;

fn tool_pair() -> (TodoWriteTool, TodoReadTool, Arc<TodoState>) {
    let state = TodoState::new();
    (
        TodoWriteTool::new(Arc::clone(&state)),
        TodoReadTool::new(Arc::clone(&state)),
        state,
    )
}

#[test]
fn write_replaces_and_read_returns_the_full_list() {
    let (w, r, _) = tool_pair();
    w.execute(json!({"todos": [
        {"id": "t1", "content": "spec it", "status": "completed"},
        {"id": "t2", "content": "build it", "status": "in_progress"},
    ]}))
    .unwrap();
    let read = r.execute(json!({})).unwrap();
    assert_eq!(read["todos"].as_array().unwrap().len(), 2);
    assert_eq!(read["todos"][1]["status"], "in_progress");
}

#[test]
fn defaults_are_applied_and_subtasks_keep_parent() {
    let (w, _r, _) = tool_pair();
    let out = w
        .execute(json!({"todos": [
            {"content": "one"},
            {"content": "sub", "parent": "t1"},
        ]}))
        .unwrap();
    assert_eq!(out[0]["id"], "t1");
    assert_eq!(out[0]["status"], "pending");
    assert_eq!(out[1]["parent"], "t1");
}

#[test]
fn at_most_one_task_in_progress() {
    let (w, _, _) = tool_pair();
    let err = w
        .execute(json!({"todos": [
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "in_progress"},
        ]}))
        .unwrap_err();
    assert!(err.contains("at most ONE"), "{err}");
}

#[test]
fn malformed_input_is_a_loud_tool_error() {
    let (w, _, _) = tool_pair();
    assert!(w.execute(json!({})).is_err());
    assert!(w.execute(json!({"todos": []})).is_err());
    assert!(w
        .execute(json!({"todos": [{"content": "x", "status": "done"}]}))
        .is_err());
    assert!(w
        .execute(json!({"todos": [
            {"id": "dup", "content": "a"}, {"id": "dup", "content": "b"}
        ]}))
        .is_err());
}

/// The durable-record contract: the settled output IS the state —
/// hydrating a transcript of intent+settlement pairs restores the
/// last one, and a settlement from another tool never pollutes it.
#[test]
fn hydrate_folds_settled_write_results_in_order() {
    let state = TodoState::new();
    let entry = |id: &str, parent: Option<&str>, kind: &'static str, payload: Value| {
        tole_core::entry::Entry {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            kind: tole_core::entry::EntryType::new(kind),
            timestamp: 0,
            seq: 0,
            payload,
        }
    };
    let list1 = json!([{"id": "t1", "content": "old", "status": "pending"}]);
    let list2 = json!([
        {"id": "t1", "content": "old", "status": "completed"},
        {"id": "t2", "content": "new", "status": "in_progress"},
    ]);
    let entries = vec![
        entry(
            "intent_1",
            None,
            tole_core::entry::EntryType::INTENT,
            json!({"tool": "todo_write"}),
        ),
        entry(
            "e_1",
            Some("intent_1"),
            tole_core::entry::EntryType::TOOL_RESULT,
            json!({"ok": true, "output": list1}),
        ),
        // Another tool's settlement between writes: must be skipped.
        entry(
            "intent_2",
            None,
            tole_core::entry::EntryType::INTENT,
            json!({"tool": "read_file"}),
        ),
        entry(
            "e_2",
            Some("intent_2"),
            tole_core::entry::EntryType::TOOL_RESULT,
            json!({"ok": true, "output": "file text"}),
        ),
        entry(
            "intent_3",
            None,
            tole_core::entry::EntryType::INTENT,
            json!({"tool": "todo_write"}),
        ),
        entry(
            "e_3",
            Some("intent_3"),
            tole_core::entry::EntryType::TOOL_RESULT,
            json!({"ok": true, "output": list2}),
        ),
    ];
    state.hydrate(&entries);
    let read = TodoReadTool::new(Arc::clone(&state));
    let list = read.execute(json!({})).unwrap()["todos"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["status"], "completed");
    assert_eq!(list[1]["id"], "t2");
}

/// Issue #226 regression: todo state is PER-SESSION. Each host
/// (`tole serve`/ACP open_session, mission, run/chat) builds its own
/// instance via `TodoState::new()`; a write in one session's state
/// must be invisible to another session's, and re-hydrating a
/// session from its own (empty) transcript must not resurrect
/// another instance's list.
#[test]
fn state_instances_are_independent_per_session() {
    let session_a = TodoState::new();
    let session_b = TodoState::new();

    let write_a = TodoWriteTool::new(Arc::clone(&session_a));
    let read_b = TodoReadTool::new(Arc::clone(&session_b));

    write_a
        .execute(json!({"todos": [
            {"id": "a1", "content": "a's private task", "status": "in_progress"}
        ]}))
        .unwrap();

    // B folds only B's own transcript (empty here): A's list must
    // never bleed in.
    session_b.hydrate(&[]);
    let seen = read_b.execute(json!({})).unwrap();
    assert_eq!(
        seen["todos"].as_array().unwrap().len(),
        0,
        "session B must not observe session A's todo list"
    );
}
