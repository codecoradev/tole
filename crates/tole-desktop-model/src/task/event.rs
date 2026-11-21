//! What the task view is built from: the model's own event type.
//!
//! This crate may not depend on `tole-client`, so [`TaskEvent`] mirrors what
//! the typed client yields. The adapter in `tole-desktop` (a later change)
//! maps client values to it with these rules:
//!
//! | Client value | Model event |
//! |---|---|
//! | entry `message`, role `user`, payload flag `stop_gate_feedback` or `denial_note` | [`TaskEvent::UserMessage`], kind `Synthetic` |
//! | the same with payload flag `interrupt` (a steer note) | kind `Steer` |
//! | the same without those flags (`tole_core::turn::transcript::opens_turn`) | kind `Prompt` (`images` = `payload.images.len()`) |
//! | entry `message`, role `assistant` | [`TaskEvent::AnswerEntry`] |
//! | entry `intent` | [`TaskEvent::ToolIntent`] (`id` = entry id, `target` = path or command of `payload.input`) |
//! | entry `tool_result` or `error` | [`TaskEvent::ToolSettled`] (`intent_id` = `parent_id`, `ok` = it is a `tool_result`) |
//! | `update_plan` intent (input `entries`), `todo_write` result (output list) | also [`TaskEvent::PlanUpdated`], the full list, `seq` of that same entry, sent right after its own event |
//! | `text_delta`, `reasoning` | [`TaskEvent::TextDelta`], [`TaskEvent::Reasoning`] (text not kept) |
//! | `ready`, or `busy` of `GET /status` | [`TaskEvent::Ready`] |
//! | `approval_requested`, `approval_resolved` | [`TaskEvent::ApprovalRequested`], [`TaskEvent::ApprovalResolved`] (`approved` = status `approved`) |
//! | `resync`, `closed` | [`TaskEvent::Resync`], [`TaskEvent::Closed`] |
//! | `StreamItem::Reconnected` | [`TaskEvent::Reconnected`], then [`TaskEvent::ApprovalsRefreshed`] from `Client::approvals` |
//! | `ReconnectExhausted` or a refusal | [`TaskEvent::ConnectionLost`] |
//! | the prompt call returned | [`TaskEvent::ApprovalRequested`] for the reply's `approval` row if any, then [`TaskEvent::TurnEnded`] |
//!
//! Not mapped: `tool_started`, `tool_finished` (the durable entries say the
//! same), heartbeats, unknown frames. History and live use the same events.
//!
//! **The adapter passes text through raw.** The model makes every tool name,
//! target, plan line, approval summary and synthetic note printable and cuts it
//! (`MAX_*` below, an ellipsis marks a cut). The answer is Markdown and stays
//! raw: the renderer must not run HTML or open links by itself.
//!
//! **After an allow** (`approved` resolved) the resumed turn has no prompt
//! call to wait for ("unobserved": `Run::unobserved`). The adapter must send
//! [`TaskEvent::Ready`] from `GET /status` and keep polling it until it says
//! not busy, and [`TaskEvent::ResumeTimedOut`] if nothing confirmed the turn
//! after a few seconds or the polling is lost. The model does not rely on it:
//! until something confirms, the phase is `Resuming` (Stop works, steering does
//! not), and `ResumeTimedOut` ends an unobserved turn at any time until it ends.
//! For a Stop the adapter sends `Ready{busy:false}` when cancel answers
//! `cancelled: false`, and calls `TaskState::stop_failed` when the call fails.
//!
//! **Late and old frames.** Text and reasoning frames after the turn ended, or
//! after its answer, are ignored until the next prompt, a `Ready{busy:true}` or
//! an allow. A [`TaskEvent::PlanUpdated`] older than the newest entry seen is
//! ignored. The adapter must call `TaskState::history_complete` once it fed the
//! whole history (a new session has none to load); until then the status says
//! "No changes seen", never "Nothing was changed", because earlier writes may be
//! on a page it did not read. Fetch `GET /approvals` once after opening the
//! stream and send it as [`TaskEvent::ApprovalsRefreshed`]: approvals the
//! stream announced meanwhile are kept, and ones decided meanwhile are not
//! brought back.

use crate::plain::printable;

/// Longest tool name, target, plan line, approval summary or note kept.
pub const MAX_TOOL_CHARS: usize = 64;
pub const MAX_TARGET_CHARS: usize = 200;
pub const MAX_PLAN_TEXT_CHARS: usize = 200;
pub const MAX_PLAN_ITEMS: usize = 50;
pub const MAX_SUMMARY_CHARS: usize = 300;
pub const MAX_NOTE_CHARS: usize = 300;

/// What a tool does, for the status row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEffect {
    /// Only looks: counts as "read".
    Read,
    /// May change something: counts as "changed" once it succeeds.
    Change,
    /// Publishes the plan list: a plan, never a step.
    Plan,
}

/// Classify a tool by name. Conservative: anything not known to be harmless
/// is a [`ToolEffect::Change`] (MCP tools are always writes), so "Nothing has
/// been changed yet" is never said by mistake.
pub fn tool_effect(tool: &str) -> ToolEffect {
    match tool {
        "update_plan" | "todo_write" => ToolEffect::Plan,
        "read_file"
        | "read_file_raw"
        | "todo_read"
        | "web_fetch"
        | "web_search"
        | "skill_search"
        | "load_skill"
        | "mcp_read"
        | "job_poll"
        | "cora_search"
        | "uteke_recall"
        | "tole_session_status"
        | "tole_session_list" => ToolEffect::Read,
        _ => ToolEffect::Change,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanStatus,
}

/// An approval waiting for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalInfo {
    /// The key for deduplication.
    pub id: String,
    pub tool: String,
    /// Plain words for what it will do, when the server sent a summary.
    pub summary: Option<String>,
}

impl ApprovalInfo {
    /// The same approval with every text made printable and cut.
    pub(crate) fn cleaned(&self) -> ApprovalInfo {
        let summary = self.summary.as_deref();
        ApprovalInfo {
            id: printable(&self.id, MAX_TOOL_CHARS),
            tool: printable(&self.tool, MAX_TOOL_CHARS),
            summary: summary.map(|s| printable(s, MAX_SUMMARY_CHARS)),
        }
    }
}

/// Who wrote a user-role message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserKind {
    /// The owner's prompt: opens a turn.
    Prompt,
    /// The owner's steer note to a running turn: shown, but opens no turn.
    Steer,
    /// Tole's own note to the model (stop gate, denied call): not the owner's
    /// words, shown as a note, opens no turn.
    Synthetic,
}

/// One thing that happened in a task.
#[rustfmt::skip] // one variant per line: it is a table of the wire frames
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    /// Entries are durable and ordered: one whose `seq` is not above the last seen is ignored.
    UserMessage { seq: u64, text: String, images: u32, kind: UserKind },
    /// The final answer of a turn, Markdown. It replaces any streamed text.
    AnswerEntry { seq: u64, text: String },
    /// A tool call was recorded; `id` is the intent entry id.
    ToolIntent { seq: u64, id: String, tool: String, target: Option<String> },
    ToolSettled { seq: u64, intent_id: String, ok: bool },
    /// The full plan list, from the entry `seq`; replaces the plan of the
    /// current turn, and is ignored when older than the newest entry seen.
    PlanUpdated { seq: u64, items: Vec<PlanItem> },
    /// Best effort: a piece of the answer being written.
    TextDelta { text: String },
    /// Best effort: the model is thinking.
    Reasoning,
    /// Whether a turn is running right now.
    Ready { busy: bool },
    ApprovalRequested(ApprovalInfo),
    /// `approved` is false for a denial or an expiry.
    ApprovalResolved { id: String, approved: bool },
    /// The queue as the server holds it now.
    ApprovalsRefreshed { pending: Vec<ApprovalInfo> },
    /// An unobserved (allowed) turn was not confirmed running in time (see the
    /// module docs): it ends.
    ResumeTimedOut,
    /// The stream dropped ephemeral frames.
    Resync,
    Reconnected,
    /// The stream is over and will not come back by itself.
    ConnectionLost,
    /// The session left the server.
    Closed,
    /// The prompt call returned: finished, stopped, cancelled or waiting for an approval.
    TurnEnded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_tools_count_as_changes() {
        assert_eq!(tool_effect("read_file"), ToolEffect::Read);
        assert_eq!(tool_effect("update_plan"), ToolEffect::Plan);
        for t in ["run_command", "mcp_anything", "agent_poll"] {
            assert_eq!(tool_effect(t), ToolEffect::Change);
        }
    }
}
