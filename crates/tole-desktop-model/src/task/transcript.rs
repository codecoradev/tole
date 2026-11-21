//! The transcript: what the task view lists, built from [`TaskEvent`]s.
//!
//! - An entry whose `seq` is not above the last seen is ignored
//!   ([`Transcript::apply`] returns false).
//! - Tool calls come from `intent` and settlement entries only; calls with
//!   nothing between them are one collapsible [`WorkGroup`]. Plan tools make
//!   or replace an [`Item::Plan`] and are never steps.
//! - Streamed text is a draft, and stands in for the turn's answer until the
//!   answer entry replaces it, whether or not the turn already ended (the
//!   prompt reply usually comes first). A draft a tool call follows stays as a
//!   note that a reload will not have. After a reconnect an unfinished draft
//!   is dropped.
//! - A Change step that failed, did not finish, or whose settlement was lost
//!   makes "may have changed" (core calls those outcomes unknown), never
//!   "nothing changed".
//! - Text a model wrote is made printable and cut when it enters: names, targets
//!   and notes by `printable`; the answer, entry and draft alike, by
//!   `printable_text` (lines kept, capped at `MAX_ANSWER_CHARS`). The owner's
//!   own prompt and steer text stay as typed.

use super::event::{
    tool_effect, PlanItem, TaskEvent, ToolEffect, UserKind, MAX_ANSWER_CHARS, MAX_NOTE_CHARS,
    MAX_PLAN_ITEMS, MAX_PLAN_TEXT_CHARS, MAX_TARGET_CHARS, MAX_TOOL_CHARS,
};
use crate::plain::{clean_text, printable, printable_text};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Done,
    Failed,
    /// The turn ended and no settlement came.
    Unfinished,
}

/// One tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The intent entry id.
    pub id: String,
    pub tool: String,
    /// Path or command, shown under "technical details".
    pub target: Option<String>,
    pub effect: ToolEffect,
    pub state: StepState,
}

/// The "Working · N steps" block; collapsed by default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkGroup {
    pub steps: Vec<Step>,
    pub expanded: bool,
}

impl WorkGroup {
    pub fn running(&self) -> bool {
        self.steps.iter().any(|s| s.state == StepState::Running)
    }
}

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// The owner's words; `steer` for a note sent to a running turn.
    User {
        text: String,
        images: u32,
        steer: bool,
    },
    /// Tole's own note to the model (stop gate, denied call): not the owner's.
    Note(String),
    /// Tole's words as Markdown, made printable; `streaming` while still being
    /// written, `cut` when it hit `MAX_ANSWER_CHARS` and ends in an ellipsis.
    Answer {
        markdown: String,
        streaming: bool,
        cut: bool,
    },
    Work(WorkGroup),
    Plan(Vec<PlanItem>),
}

/// "38 read · 0 changed", for the events fed so far: the whole task only when
/// the history was fed completely (see `TaskState::history_complete`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub read: u32,
    /// Calls that may change something and succeeded.
    pub changed: u32,
    /// Calls that may change something and did not settle cleanly: failed,
    /// unfinished, running or settled without being seen.
    pub unsure: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub items: Vec<Item>,
    pub counts: Counts,
    last_seq: Option<u64>,
    /// The row standing in for the current turn's answer until its entry comes.
    standin: Option<usize>,
    /// Characters in the draft being written (it is capped).
    draft_chars: usize,
    /// Intents of plan tools (not steps): their settlements are not unknowns.
    plan_ids: HashSet<String>,
}

/// What a step adds to (changed, unsure).
fn tally(step: &Step) -> (u32, u32) {
    match (step.effect, step.state) {
        (ToolEffect::Change, StepState::Done) => (1, 0),
        (ToolEffect::Change, _) => (0, 1),
        _ => (0, 0),
    }
}

impl Transcript {
    /// Calls made so far, plans excluded.
    pub fn step_count(&self) -> usize {
        let steps = |i: &Item| {
            if let Item::Work(g) = i {
                g.steps.len()
            } else {
                0
            }
        };
        self.items.iter().map(steps).sum()
    }

    /// The group at `index` (not a group: ignored).
    pub fn toggle_group(&mut self, index: usize) {
        if let Some(Item::Work(g)) = self.items.get_mut(index) {
            g.expanded = !g.expanded;
        }
    }

    /// The turn is over without an answer entry in hand: a step still running
    /// will not be settled, and a draft stops growing.
    pub fn abandon_running(&mut self) {
        self.finish_draft();
        let steps = self.items.iter_mut().filter_map(|i| match i {
            Item::Work(g) => Some(&mut g.steps),
            _ => None,
        });
        for step in steps.flatten().filter(|s| s.state == StepState::Running) {
            step.state = StepState::Unfinished;
        }
    }

    fn drafting(&self) -> bool {
        matches!(
            self.items.last(),
            Some(Item::Answer {
                streaming: true,
                ..
            })
        )
    }

    /// Fold in one event. False for an entry that is not new (ignored).
    pub fn apply(&mut self, event: &TaskEvent) -> bool {
        match event {
            TaskEvent::UserMessage {
                seq,
                text,
                images,
                kind,
            } => {
                if !self.fresh(*seq) {
                    return false;
                }
                self.finish_draft();
                self.standin = None;
                self.items.push(match kind {
                    UserKind::Synthetic => Item::Note(printable(text, MAX_NOTE_CHARS)),
                    _ => Item::User {
                        text: text.clone(),
                        images: *images,
                        steer: *kind == UserKind::Steer,
                    },
                });
            }
            TaskEvent::AnswerEntry { seq, text } => {
                if !self.fresh(*seq) {
                    return false;
                }
                let (markdown, cut) = printable_text(text, MAX_ANSWER_CHARS);
                let row = Item::Answer {
                    markdown,
                    streaming: false,
                    cut,
                };
                match self.standin.take().filter(|i| *i < self.items.len()) {
                    Some(i) => self.items[i] = row,
                    None => self.items.push(row),
                }
            }
            TaskEvent::ToolIntent {
                seq,
                id,
                tool,
                target,
            } => {
                if !self.fresh(*seq) {
                    return false;
                }
                self.start_step(id, tool, target);
            }
            TaskEvent::ToolSettled { seq, intent_id, ok } => {
                if !self.fresh(*seq) {
                    return false;
                }
                self.settle_step(intent_id, *ok);
            }
            TaskEvent::PlanUpdated { seq, items } => {
                // Same entry as the intent just seen is fine; an older one is a replay.
                if self.last_seq.is_some_and(|last| *seq < last) {
                    return false;
                }
                self.set_plan(items);
            }
            TaskEvent::TextDelta { text } => self.append_draft(&clean_text(text)),
            TaskEvent::Reconnected | TaskEvent::Resync if self.drafting() => {
                self.items.pop();
                self.standin = None;
            }
            TaskEvent::TurnEnded | TaskEvent::Closed => self.finish_draft(),
            _ => {}
        }
        true
    }

    /// Add already-clean text to the draft, which stops at the cap: the last
    /// character is an ellipsis, `cut` is set, and later text is ignored.
    fn append_draft(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.drafting() {
            self.items.push(Item::Answer {
                markdown: String::new(),
                streaming: true,
                cut: false,
            });
            self.standin = Some(self.items.len() - 1);
            self.draft_chars = 0;
        }
        let Some(Item::Answer { markdown, cut, .. }) = self.items.last_mut() else {
            return;
        };
        if *cut {
            return;
        }
        let room = MAX_ANSWER_CHARS - self.draft_chars;
        let mut chars = text.chars();
        let head: String = chars.by_ref().take(room).collect();
        self.draft_chars += head.chars().count();
        markdown.push_str(&head);
        if chars.next().is_some() {
            markdown.pop(); // the last character makes room for the ellipsis
            markdown.push('\u{2026}');
            *cut = true;
        }
    }

    fn fresh(&mut self, seq: u64) -> bool {
        let new = self.last_seq.is_none_or(|last| seq > last);
        if new {
            self.last_seq = Some(seq);
        }
        new
    }

    /// The draft stops growing; it keeps standing in for the answer.
    fn finish_draft(&mut self) {
        if let Some(Item::Answer { streaming, .. }) = self.items.last_mut() {
            *streaming = false;
        }
    }

    fn start_step(&mut self, id: &str, tool: &str, target: &Option<String>) {
        // A draft a tool call follows was a lead-in, not the answer.
        self.finish_draft();
        self.standin = None;
        let effect = tool_effect(tool);
        if effect == ToolEffect::Plan {
            self.plan_ids.insert(id.to_string());
            return;
        }
        let step = Step {
            id: id.to_string(),
            tool: printable(tool, MAX_TOOL_CHARS),
            target: (target.as_deref())
                .map(|t| printable(t, MAX_TARGET_CHARS))
                .filter(|t| !t.is_empty()),
            effect,
            state: StepState::Running,
        };
        self.counts.read += u32::from(effect == ToolEffect::Read);
        self.counts.unsure += tally(&step).1;
        match self.items.last_mut() {
            Some(Item::Work(g)) => g.steps.push(step),
            _ => self.items.push(Item::Work(WorkGroup {
                steps: vec![step],
                expanded: false,
            })),
        }
    }

    fn settle_step(&mut self, intent_id: &str, ok: bool) {
        let found = self.items.iter_mut().rev().find_map(|item| match item {
            Item::Work(g) => g.steps.iter_mut().rev().find(|s| s.id == intent_id),
            _ => None,
        });
        let Some(step) = found else {
            // Not a step we know: it may have been a change we never saw.
            self.counts.unsure += u32::from(!self.plan_ids.contains(intent_id));
            return;
        };
        let (changed, unsure) = tally(step);
        step.state = if ok {
            StepState::Done
        } else {
            StepState::Failed
        };
        let (now_changed, now_unsure) = tally(step);
        self.counts.changed = self.counts.changed - changed + now_changed;
        self.counts.unsure = self.counts.unsure - unsure + now_unsure;
    }

    /// Replace the plan of the current turn, or add one where it first shows.
    fn set_plan(&mut self, items: &[PlanItem]) {
        let items: Vec<PlanItem> = items
            .iter()
            .take(MAX_PLAN_ITEMS)
            .map(|p| PlanItem {
                text: printable(&p.text, MAX_PLAN_TEXT_CHARS),
                status: p.status,
            })
            .collect();
        let current = (self.items.iter_mut().rev())
            .take_while(|i| !matches!(i, Item::User { steer: false, .. }))
            .find(|i| matches!(i, Item::Plan(_)));
        match current {
            Some(slot) => *slot = Item::Plan(items),
            None if !items.is_empty() => self.items.push(Item::Plan(items)),
            None => {}
        }
    }
}
