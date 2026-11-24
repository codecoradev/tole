//! Durable entries (history pages and live `entry` frames) to events.
//!
//! The rules are the table in the module doc of the model's `task/event.rs`.
//! Decisions where the table is silent, all on the careful side:
//!
//! - A `message` is read by `payload.role`. `user`: a flag counts only when it
//!   is the JSON `true` (as core's `opens_turn` reads it). `stop_gate_feedback`
//!   or `denial_note` make it `Synthetic`, even next to `interrupt` (tole's own
//!   words are never shown as the owner's); `interrupt` alone is a `Steer`; no
//!   flag is a `Prompt`. A missing or non-string `text` is `""`, so the turn
//!   structure of a history is kept. `assistant`: a missing or non-string `text`
//!   maps to nothing (an empty answer would end a turn on a malformed row). Any
//!   other role, or none, maps to nothing.
//! - An `intent` without a string `tool` maps to nothing; its settlement then
//!   reaches the model as one it never saw, which counts as "may have changed".
//! - An `interrupt` message is a `Steer` only when its `source` is `owner` (or
//!   absent: entries of a core before the key existed were all owner steers).
//!   Any other source, `notice` today (a sibling session wrote files, a started
//!   job exited), is `Synthetic`: its text names files and commands, so the model
//!   must clean it, and it is not the owner's words. Prompt and Steer text is the
//!   one class the model does not clean, so only owner-authored text may use it.
//! - Core writes an owner's steer note with a wrapper the model reads
//!   (`[owner interrupt — injected mid-turn, not part of the original request] `,
//!   and `owner interrupt · urgent` for an urgent one); the transcript shows
//!   the owner's own words, so that exact prefix is taken off a `Steer` (once,
//!   and only that prefix: anything else the owner wrote stays).
//! - A `tool_result` or `error` without a `parentId` maps to nothing: it cannot
//!   be tied to a call.
//! - Core writes two shapes of `error`. A tool failure (`settle_err`) is a child
//!   of its intent with `{ok:false, error}`. A turn error (`append_turn_error`:
//!   cancelled, approval required, denied, provider failed, budget, stop-gate
//!   cap) is a child of the turn's latest USER message with `{error, detail}`
//!   and settles no call, so it maps to nothing: counting it as the settlement
//!   of an unknown call would make "nothing was changed" unreachable after any
//!   Stop or denial. It is a turn error when its parent is a user message this
//!   mapper saw (the last 16 are remembered), or when its payload has a `detail`
//!   and no `ok` key. Anything else stays a settlement, the careful side.
//! - `target` is `input.path`, else `input.command`, when a non-empty string.
//! - A `todo_write` list comes from its result, so the mapper remembers the ids
//!   of recent `todo_write` intents ([`EntryMapper`] is the only state of the
//!   adapter's history side). A result whose intent was not seen by this mapper
//!   publishes no plan.
//! - Every other entry type maps to nothing.

use super::approval::MAX_REFRESH_ROWS;
use super::plan;
use serde_json::Value;
use std::collections::VecDeque;
use tole_client::Entry;
use tole_desktop_model::task::{TaskEvent, UserKind};

/// `todo_write` intents remembered until their settlement.
const MAX_OPEN_TODO_INTENTS: usize = 32;
/// An id longer than this is not remembered (its plan is lost, nothing else).
const MAX_ID_BYTES: usize = 256;
/// User-role message ids remembered, to tell a turn error from a tool failure.
const MAX_USER_IDS: usize = 16;

/// Maps entries in order. Feed history and live entries to the same mapper.
#[derive(Debug, Default)]
pub struct EntryMapper {
    todo_intents: VecDeque<String>,
    user_ids: VecDeque<String>,
    /// Approval ids announced by the stream and not resolved yet.
    open_approvals: Vec<String>,
}

fn flag(payload: &Value, key: &str) -> bool {
    payload.get(key).and_then(Value::as_bool) == Some(true)
}

fn str_of<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn user_kind(payload: &Value) -> UserKind {
    if flag(payload, "stop_gate_feedback") || flag(payload, "denial_note") {
        UserKind::Synthetic
    } else if flag(payload, "interrupt") {
        match payload.get("source") {
            None | Some(Value::Null) => UserKind::Steer,
            Some(v) if v.as_str() == Some("owner") => UserKind::Steer,
            Some(_) => UserKind::Synthetic,
        }
    } else {
        UserKind::Prompt
    }
}

/// What core puts before an owner's steer note (`turn::deliver_interrupts`).
const STEER_WRAPPERS: [&str; 2] = [
    "[owner interrupt \u{2014} injected mid-turn, not part of the original request] ",
    "[owner interrupt \u{b7} urgent \u{2014} injected mid-turn, not part of the original request] ",
];

/// The owner's words of a steer note: the text without core's wrapper.
fn steer_words(text: &str) -> &str {
    STEER_WRAPPERS
        .iter()
        .find_map(|wrapper| text.strip_prefix(wrapper))
        .unwrap_or(text)
}

fn target(input: Option<&Value>) -> Option<String> {
    let input = input?;
    ["path", "command"]
        .into_iter()
        .filter_map(|k| str_of(input, k))
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

impl EntryMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// The events of one entry, in the order they must be applied.
    pub fn entry(&mut self, e: &Entry) -> Vec<TaskEvent> {
        match e.kind.as_str() {
            "message" => {
                self.remember_user(e);
                self.message(e).into_iter().collect()
            }
            "intent" => self.intent(e),
            "tool_result" => self.settled(e, true),
            "error" => self.settled(e, false),
            _ => Vec::new(),
        }
    }

    /// Whether a live `approval_requested` for `id` goes on to the model (see the
    /// module docs of `approval`): a repeat does, a new id does while fewer than
    /// `MAX_REFRESH_ROWS` are open, an id over `MAX_ID_BYTES` never does.
    pub(super) fn admit_approval(&mut self, id: &str) -> bool {
        if id.len() > MAX_ID_BYTES {
            return false;
        }
        if self.open_approvals.iter().any(|open| open == id) {
            return true;
        }
        if self.open_approvals.len() >= MAX_REFRESH_ROWS {
            return false;
        }
        self.open_approvals.push(id.to_string());
        true
    }

    pub(super) fn release_approval(&mut self, id: &str) {
        self.open_approvals.retain(|open| open != id);
    }

    pub(super) fn reset_approvals(&mut self) {
        self.open_approvals.clear();
    }

    fn remember_user(&mut self, e: &Entry) {
        let user = str_of(&e.payload, "role") == Some("user");
        if user && e.id.len() <= MAX_ID_BYTES {
            if self.user_ids.len() >= MAX_USER_IDS {
                self.user_ids.pop_front();
            }
            self.user_ids.push_back(e.id.clone());
        }
    }

    /// An `error` that is the turn's own record, not a call's settlement.
    fn is_turn_error(&self, e: &Entry, parent: &str) -> bool {
        let shaped = e.payload.get("ok").is_none() && e.payload.get("detail").is_some();
        shaped || self.user_ids.iter().any(|id| id == parent)
    }

    fn message(&self, e: &Entry) -> Option<TaskEvent> {
        let text = str_of(&e.payload, "text");
        match str_of(&e.payload, "role")? {
            "user" => {
                let images = e.payload.get("images").and_then(Value::as_array);
                let kind = user_kind(&e.payload);
                let text = text.unwrap_or_default();
                let text = if kind == UserKind::Steer {
                    steer_words(text)
                } else {
                    text
                };
                Some(TaskEvent::UserMessage {
                    seq: e.seq,
                    text: text.to_string(),
                    images: images.map_or(0, |a| u32::try_from(a.len()).unwrap_or(u32::MAX)),
                    kind,
                })
            }
            "assistant" => Some(TaskEvent::AnswerEntry {
                seq: e.seq,
                text: text?.to_string(),
            }),
            _ => None,
        }
    }

    fn intent(&mut self, e: &Entry) -> Vec<TaskEvent> {
        let Some(tool) = str_of(&e.payload, "tool") else {
            return Vec::new();
        };
        let input = e.payload.get("input");
        let mut out = vec![TaskEvent::ToolIntent {
            seq: e.seq,
            id: e.id.clone(),
            tool: tool.to_string(),
            target: target(input),
        }];
        match tool {
            "update_plan" => {
                let items = plan::from_input(input);
                out.extend(items.map(|items| TaskEvent::PlanUpdated { seq: e.seq, items }));
            }
            "todo_write" if e.id.len() <= MAX_ID_BYTES => {
                if self.todo_intents.len() >= MAX_OPEN_TODO_INTENTS {
                    self.todo_intents.pop_front();
                }
                self.todo_intents.push_back(e.id.clone());
            }
            _ => {}
        }
        out
    }

    fn settled(&mut self, e: &Entry, ok: bool) -> Vec<TaskEvent> {
        let Some(parent) = e.parent_id.as_deref() else {
            return Vec::new();
        };
        if !ok && self.is_turn_error(e, parent) {
            return Vec::new();
        }
        let mut out = vec![TaskEvent::ToolSettled {
            seq: e.seq,
            intent_id: parent.to_string(),
            ok,
        }];
        if let Some(at) = self.todo_intents.iter().position(|id| id == parent) {
            self.todo_intents.remove(at);
            if ok {
                let items = plan::from_output(e.payload.get("output"));
                out.extend(items.map(|items| TaskEvent::PlanUpdated { seq: e.seq, items }));
            }
        }
        out
    }
}
