//! Tests of the task adapter: client values (built from the wire JSON, through
//! the client's own decoders) are mapped and fed to the real `TaskState`.

mod effects_tests;
mod hostile_tests;
mod live_tests;
mod replay_tests;
mod review_tests;
mod round2_tests;

use super::{stream_item, EntryMapper};
use serde_json::{json, Value};
use tole_client::{Approval, Entry, Event, Frame, StreamItem};
use tole_desktop_model::task::{Item, TaskEvent, TaskState};

/// An entry as the log writes it.
pub fn entry(seq: u64, id: &str, parent: Option<&str>, kind: &str, payload: Value) -> Entry {
    let row = json!({"seq": seq, "id": id, "parentId": parent, "type": kind,
        "timestamp": 0, "payload": payload});
    serde_json::from_value(row).expect("an entry")
}

pub fn user(seq: u64, text: &str) -> Entry {
    let payload = json!({"role": "user", "text": text});
    entry(seq, &format!("e_{seq}"), None, "message", payload)
}

pub fn answer(seq: u64, text: &str) -> Entry {
    let payload = json!({"role": "assistant", "text": text});
    entry(seq, &format!("e_{seq}"), None, "message", payload)
}

/// An intent with the id `i<seq>`, the way the effect sandwich writes it.
pub fn intent(seq: u64, tool: &str, input: Value) -> Entry {
    let payload = json!({"tool": tool, "input": input, "replay": "safe"});
    entry(seq, &format!("i{seq}"), None, "intent", payload)
}

/// The settlement of the intent `i<of>`.
pub fn result(seq: u64, of: u64, output: Value) -> Entry {
    let payload = json!({"ok": true, "output": output});
    entry(
        seq,
        &format!("r{seq}"),
        Some(&format!("i{of}")),
        "tool_result",
        payload,
    )
}

pub fn failure(seq: u64, of: u64) -> Entry {
    let payload = json!({"ok": false, "error": "boom"});
    entry(
        seq,
        &format!("r{seq}"),
        Some(&format!("i{of}")),
        "error",
        payload,
    )
}

pub fn approval(id: &str, status: &str) -> Approval {
    let row = json!({"id": id, "tool": "write_file", "status": status, "sessionId": "s1"});
    serde_json::from_value(row).expect("an approval")
}

/// A frame decoded by the client, as the stream hands it over.
pub fn frame(name: &str, data: Value) -> StreamItem {
    let frame = Frame {
        id: None,
        event: name.into(),
        data: data.to_string(),
    };
    StreamItem::Event(Event::from_frame(&frame).expect("a frame"))
}

pub fn entry_frame(e: &Entry) -> StreamItem {
    frame("entry", serde_json::to_value(e).expect("json"))
}

pub fn ready(busy: bool) -> StreamItem {
    let data = json!({"id": "s1", "after": 0, "lastSeq": 0, "busy": busy});
    frame("ready", data)
}

pub fn delta(text: &str) -> StreamItem {
    frame("text_delta", json!({"text": text}))
}

pub fn approval_frame(name: &str, id: &str, status: &str) -> StreamItem {
    frame(
        name,
        json!({"id": id, "tool": "write_file", "status": status}),
    )
}

/// The task as a window would hold it, fed from entries and frames.
pub struct Task {
    pub state: TaskState,
    pub mapper: EntryMapper,
}

impl Task {
    pub fn new() -> Self {
        Task {
            state: TaskState::new(false),
            mapper: EntryMapper::new(),
        }
    }

    pub fn apply(&mut self, events: Vec<TaskEvent>) {
        events.into_iter().for_each(|e| self.state.apply(e));
    }

    pub fn entries(&mut self, entries: &[Entry]) {
        for e in entries {
            let events = self.mapper.entry(e);
            self.apply(events);
        }
    }

    pub fn live(&mut self, items: &[StreamItem]) {
        for item in items {
            let events = stream_item(&mut self.mapper, item);
            self.apply(events);
        }
    }

    /// The rows in short: U user, s steer, N note, A answer, a draft, W3 a group
    /// of 3 steps, P plan.
    pub fn shape(&self) -> String {
        let row = |i: &Item| match i {
            Item::User { steer: false, .. } => "U".to_string(),
            Item::User { .. } => "s".to_string(),
            Item::Note(_) => "N".to_string(),
            Item::Answer {
                streaming: true, ..
            } => "a".to_string(),
            Item::Answer { .. } => "A".to_string(),
            Item::Work(g) => format!("W{}", g.steps.len()),
            Item::Plan(_) => "P".to_string(),
        };
        let rows: Vec<_> = self.state.transcript.items.iter().map(row).collect();
        rows.join(" ")
    }
}
