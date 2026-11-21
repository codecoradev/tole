//! What the task view is built from: the model's own event type.
//!
//! This crate may not depend on `tole-client`, so [`TaskEvent`] mirrors what
//! the typed client yields. The adapter in `tole-desktop` (a later change)
//! maps client values to it with these rules:
//!
//! | Client value | Model event |
//! |---|---|
//! | entry `message`, role `user` | [`TaskEvent::UserMessage`] (`images` = `payload.images.len()`) |
//! | entry `message`, role `assistant` | [`TaskEvent::AnswerEntry`] |
//! | entry `intent` | [`TaskEvent::ToolIntent`] (`id` = entry id, `target` = path or command of `payload.input`) |
//! | entry `tool_result` or `error` | [`TaskEvent::ToolSettled`] (`intent_id` = `parent_id`, `ok` = it is a `tool_result`) |
//! | `update_plan` intent (input `entries`), `todo_write` result (output list) | also [`TaskEvent::PlanUpdated`], the full list |
//! | `text_delta`, `reasoning` | [`TaskEvent::TextDelta`], [`TaskEvent::Reasoning`] (text not kept) |
//! | `ready`, or `busy` of `GET /status` (after an allow: its resumed turn has no observer) | [`TaskEvent::Ready`] |
//! | `approval_requested`, `approval_resolved` | [`TaskEvent::ApprovalRequested`], [`TaskEvent::ApprovalResolved`] (`approved` = status `approved`) |
//! | `resync`, `closed` | [`TaskEvent::Resync`], [`TaskEvent::Closed`] |
//! | `StreamItem::Reconnected` | [`TaskEvent::Reconnected`], then [`TaskEvent::ApprovalsRefreshed`] from `Client::approvals` |
//! | `ReconnectExhausted` or a refusal | [`TaskEvent::ConnectionLost`] |
//! | the prompt call returned | [`TaskEvent::ApprovalRequested`] for the reply's `approval` row if any, then [`TaskEvent::TurnEnded`] |
//!
//! Not mapped: `tool_started`, `tool_finished` (the durable entries say the
//! same), heartbeats, unknown frames. History and live use the same events.

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

/// One thing that happened in a task.
#[rustfmt::skip] // one variant per line: it is a table of the wire frames
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    /// Entries are durable and ordered: one whose `seq` is not above the last seen is ignored.
    UserMessage { seq: u64, text: String, images: u32 },
    /// The final answer of a turn, Markdown. It replaces any streamed text.
    AnswerEntry { seq: u64, text: String },
    /// A tool call was recorded; `id` is the intent entry id.
    ToolIntent { seq: u64, id: String, tool: String, target: Option<String> },
    ToolSettled { seq: u64, intent_id: String, ok: bool },
    /// The full plan list; replaces the plan of the current turn.
    PlanUpdated { items: Vec<PlanItem> },
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
