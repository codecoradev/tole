//! Durable task-list tools (issue #198): `todo_write` / `todo_read`.
//!
//! The mission-mode keystone (0.7.0 train): long autonomous runs need a
//! plan memory the model maintains itself, or it re-derives intent every
//! turn. The state lives in ordinary session entries — `todo_write`
//! returns the FULL new list as its tool result, and the result entry is
//! the durable record (write-once preserved; new state = new entry;
//! readers fold to latest). No new storage engine, no second writer: the
//! shared in-memory state is seeded by folding the transcript at session
//! open ([`TodoState::hydrate`]) and updated only by executions on the
//! turn thread (single-threaded turn = no races).
//!
//! Risk: `todo_write` is Write (through the normal approval gate; the
//! `--trust internal` preset covers it), `todo_read` is ReadOnly.
//! Conventions match leading harnesses: at most ONE task in_progress at
//! a time; list order is priority.

use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The validated, canonical task list: a JSON array of
/// `{id, content, status, parent?}` objects.
#[derive(Default)]
pub struct TodoState {
    list: Mutex<Vec<Value>>,
}

impl TodoState {
    /// Fresh, fully independent state for ONE session or mission. Every
    /// session host owns its own instance — todo state is per-session,
    /// never process-global (issue #226).
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Deprecated alias for [`TodoState::new`]. The state was never a
    /// process-global singleton — every call already returned a fresh
    /// instance — but the name kept inviting that misreading (the
    /// 0.7.0 pre-tag scan filed it as a cross-session leak, issue
    /// #226). Use [`TodoState::new`].
    #[deprecated(
        since = "0.7.0",
        note = "`shared` was never a global singleton; use TodoState::new() (issue #226)"
    )]
    pub fn shared() -> Arc<Self> {
        Self::new()
    }

    /// Seed from a session transcript: pair INTENT entries
    /// (`payload.tool == "todo_write"`) with their TOOL_RESULT
    /// settlements (`payload.output` = the returned list) in commit
    /// order; the last settled list wins. Called at session open (after
    /// `JsonlStorage` replay) — crash-resume therefore restores the last
    /// SETTLED list, and an un-settled write lost to a crash reverts
    /// exactly like any other un-settled effect (the sandwich contract).
    pub fn hydrate(&self, entries: &[crate::entry::Entry]) {
        let mut list: Vec<Value> = Vec::new();
        // intent id → is this a todo_write intent?
        let mut todo_intents: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for e in entries {
            let kind = e.kind.as_str();
            if kind == crate::entry::EntryType::INTENT {
                if e.payload.get("tool").and_then(Value::as_str) == Some("todo_write") {
                    todo_intents.insert(e.id.as_str());
                }
            } else if kind == crate::entry::EntryType::TOOL_RESULT {
                let parent_is_todo = e
                    .parent_id
                    .as_deref()
                    .map(|p| todo_intents.contains(p))
                    .unwrap_or(false);
                if parent_is_todo {
                    if let Some(parsed) = e.payload.get("output").and_then(Value::as_array) {
                        list = parsed.clone();
                    }
                }
            }
        }
        *self.list.lock().unwrap_or_else(|p| p.into_inner()) = list;
    }

    fn snapshot(&self) -> Vec<Value> {
        self.list.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn replace(&self, list: Vec<Value>) {
        *self.list.lock().unwrap_or_else(|p| p.into_inner()) = list;
    }
}

/// Validate + normalize one entries payload (shared by todo_write and
/// its tests). Returns the canonical list.
fn normalize_entries(input: &Value) -> Result<Vec<Value>, String> {
    let arr = input
        .get("todos")
        .or_else(|| input.get("entries"))
        .and_then(Value::as_array)
        .ok_or("todo_write: 'todos' (array) is required")?;
    if arr.is_empty() {
        return Err("todo_write: an empty list clears the plan — pass the full \
                    list including completed tasks, or use status 'completed' per task"
            .into());
    }
    let mut seen_ids = std::collections::HashSet::new();
    let mut in_progress = 0usize;
    let mut out = Vec::with_capacity(arr.len());
    for (i, e) in arr.iter().enumerate() {
        let content = e["content"]
            .as_str()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or(format!(
                "todo_write: entry {i} needs a non-empty 'content' string"
            ))?;
        let status = e["status"].as_str().unwrap_or("pending");
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return Err(format!(
                "todo_write: invalid status '{status}' on entry {i} (pending | in_progress | completed)"
            ));
        }
        if status == "in_progress" {
            in_progress += 1;
        }
        let id = match e["id"].as_str() {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => format!("t{}", i + 1),
        };
        if !seen_ids.insert(id.clone()) {
            return Err(format!("todo_write: duplicate id '{id}'"));
        }
        let mut task = json!({"id": id, "content": content, "status": status});
        if let Some(parent) = e["parent"].as_str().filter(|p| !p.trim().is_empty()) {
            task["parent"] = json!(parent.trim());
        }
        out.push(task);
    }
    if in_progress > 1 {
        return Err(format!(
            "todo_write: at most ONE task may be in_progress (got {in_progress}) — \
             finish the current task before starting the next"
        ));
    }
    Ok(out)
}

/// Create/replace the task list. The FULL list is passed every time
/// (matching harness conventions) and echoed back verbatim in the result
/// — which is also the durable record the transcript (and hydration)
/// folds on.
pub struct TodoWriteTool {
    state: Arc<TodoState>,
}

impl TodoWriteTool {
    pub fn new(state: Arc<TodoState>) -> Self {
        Self { state }
    }
}

impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn summary(&self) -> String {
        "Replace the mission task list with the full updated list of todos.".into()
    }

    fn describe(&self, _input: &Value) -> String {
        "update the mission task list".into()
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The FULL task list, replacing any previous one",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "content": {"type": "string"},
                            "status": {"enum": ["pending", "in_progress", "completed"]},
                            "parent": {"type": "string",
                                       "description": "id of the parent task, for subtasks"}
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["todos"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let list = normalize_entries(&input)?;
        self.state.replace(list.clone());
        Ok(Value::Array(list))
    }
}

/// Fetch the current task list (ReadOnly; never prompts).
pub struct TodoReadTool {
    state: Arc<TodoState>,
}

impl TodoReadTool {
    pub fn new(state: Arc<TodoState>) -> Self {
        Self { state }
    }
}

impl Tool for TodoReadTool {
    fn name(&self) -> &str {
        "todo_read"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Read the current mission task list.".into()
    }

    fn describe(&self, _input: &Value) -> String {
        "read the current mission task list".into()
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        let list = self.state.snapshot();
        Ok(json!({"todos": list}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            crate::entry::Entry {
                id: id.to_string(),
                parent_id: parent.map(str::to_string),
                kind: crate::entry::EntryType::new(kind),
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
                crate::entry::EntryType::INTENT,
                json!({"tool": "todo_write"}),
            ),
            entry(
                "e_1",
                Some("intent_1"),
                crate::entry::EntryType::TOOL_RESULT,
                json!({"ok": true, "output": list1}),
            ),
            // Another tool's settlement between writes: must be skipped.
            entry(
                "intent_2",
                None,
                crate::entry::EntryType::INTENT,
                json!({"tool": "read_file"}),
            ),
            entry(
                "e_2",
                Some("intent_2"),
                crate::entry::EntryType::TOOL_RESULT,
                json!({"ok": true, "output": "file text"}),
            ),
            entry(
                "intent_3",
                None,
                crate::entry::EntryType::INTENT,
                json!({"tool": "todo_write"}),
            ),
            entry(
                "e_3",
                Some("intent_3"),
                crate::entry::EntryType::TOOL_RESULT,
                json!({"ok": true, "output": list2}),
            ),
        ];
        state.hydrate(&entries);
        let list = state.snapshot();
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
}
