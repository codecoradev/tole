//! The transcript: what the task view lists, built from [`TaskEvent`]s.
//!
//! - An entry whose `seq` is not above the last seen is ignored.
//! - Tool calls come from `intent` and settlement entries only; calls with
//!   nothing between them are one collapsible [`WorkGroup`]. Plan tools make
//!   or replace an [`Item::Plan`] and are never steps.
//! - Streamed text is a draft; the final answer entry replaces it. A draft a
//!   tool call follows stays as a note (the board shows such a lead-in) that a
//!   reload will not have. After a reconnect an unfinished draft is dropped.

use super::event::{tool_effect, PlanItem, TaskEvent, ToolEffect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Done,
    Failed,
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
    User {
        text: String,
        images: u32,
    },
    /// Tole's words as Markdown; `streaming` while still being written.
    Answer {
        markdown: String,
        streaming: bool,
    },
    Work(WorkGroup),
    Plan(Vec<PlanItem>),
}

/// "38 read · 0 changed", for the whole task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub read: u32,
    /// Calls that may change something and succeeded.
    pub changed: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub items: Vec<Item>,
    pub counts: Counts,
    last_seq: Option<u64>,
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

    fn drafting(&self) -> bool {
        matches!(
            self.items.last(),
            Some(Item::Answer {
                streaming: true,
                ..
            })
        )
    }

    pub fn apply(&mut self, event: &TaskEvent) {
        match event {
            TaskEvent::UserMessage { seq, text, images } if self.fresh(*seq) => {
                self.finish_draft();
                let (text, images) = (text.clone(), *images);
                self.items.push(Item::User { text, images });
            }
            TaskEvent::AnswerEntry { seq, text } if self.fresh(*seq) => {
                if self.drafting() {
                    self.items.pop();
                }
                self.items.push(Item::Answer {
                    markdown: text.clone(),
                    streaming: false,
                });
            }
            TaskEvent::ToolIntent {
                seq,
                id,
                tool,
                target,
            } if self.fresh(*seq) => self.start_step(id, tool, target),
            TaskEvent::ToolSettled { seq, intent_id, ok } if self.fresh(*seq) => {
                self.settle_step(intent_id, *ok)
            }
            TaskEvent::PlanUpdated { items } => self.set_plan(items),
            TaskEvent::TextDelta { text } => match self.items.last_mut() {
                Some(Item::Answer {
                    markdown,
                    streaming: true,
                }) => markdown.push_str(text),
                _ => self.items.push(Item::Answer {
                    markdown: text.clone(),
                    streaming: true,
                }),
            },
            TaskEvent::Reconnected | TaskEvent::Resync if self.drafting() => {
                self.items.pop();
            }
            TaskEvent::TurnEnded | TaskEvent::Closed => self.finish_draft(),
            _ => {}
        }
    }

    fn fresh(&mut self, seq: u64) -> bool {
        let new = self.last_seq.is_none_or(|last| seq > last);
        if new {
            self.last_seq = Some(seq);
        }
        new
    }

    fn finish_draft(&mut self) {
        if let Some(Item::Answer { streaming, .. }) = self.items.last_mut() {
            *streaming = false;
        }
    }

    fn start_step(&mut self, id: &str, tool: &str, target: &Option<String>) {
        self.finish_draft();
        let effect = tool_effect(tool);
        if effect == ToolEffect::Plan {
            return;
        }
        self.counts.read += u32::from(effect == ToolEffect::Read);
        let step = Step {
            id: id.to_string(),
            tool: tool.to_string(),
            target: target.clone(),
            effect,
            state: StepState::Running,
        };
        match self.items.last_mut() {
            Some(Item::Work(g)) => g.steps.push(step),
            _ => self.items.push(Item::Work(WorkGroup {
                steps: vec![step],
                ..WorkGroup::default()
            })),
        }
    }

    fn settle_step(&mut self, intent_id: &str, ok: bool) {
        let found = self.items.iter_mut().rev().find_map(|item| match item {
            Item::Work(g) => g.steps.iter_mut().rev().find(|s| s.id == intent_id),
            _ => None,
        });
        let Some(step) = found else { return };
        step.state = if ok {
            StepState::Done
        } else {
            StepState::Failed
        };
        self.counts.changed += u32::from(ok && step.effect == ToolEffect::Change);
    }

    /// Replace the plan of the current turn, or add one where it first shows.
    fn set_plan(&mut self, items: &[PlanItem]) {
        let current = (self.items.iter_mut().rev())
            .take_while(|i| !matches!(i, Item::User { .. }))
            .find(|i| matches!(i, Item::Plan(_)));
        match current {
            Some(slot) => *slot = Item::Plan(items.to_vec()),
            None if !items.is_empty() => self.items.push(Item::Plan(items.to_vec())),
            None => {}
        }
    }
}
