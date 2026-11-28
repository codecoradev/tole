//! The live side: frames of the event stream and `GET /status` to events.
//!
//! - `entry` frames go through the [`EntryMapper`], like history.
//! - `ready` and a status snapshot are [`TaskEvent::Ready`] (`busy` as sent).
//! - `text_delta` and `reasoning` frames: [`TaskEvent::TextDelta`] (text raw)
//!   and [`TaskEvent::Reasoning`] (the text is dropped here too).
//! - `approval_requested`, `approval_resolved`: see [`super::approval`].
//! - `resync` and `closed`: [`TaskEvent::Resync`], [`TaskEvent::Closed`].
//! - `StreamItem::Reconnected` is [`TaskEvent::Reconnected`] only. The queue
//!   behind it needs a call: the driver fetches `Client::approvals`, then sends
//!   [`super::approvals_refreshed`] (also once after the first open).
//! - Not mapped: `tool_started`, `tool_finished`, heartbeats and unknown frames
//!   (`Event::Other`, which is also what a known ephemeral frame of a changed
//!   shape decodes to).
//! - A stream error is [`TaskEvent::ConnectionLost`], whatever it is: the stream
//!   is fused after an `Err` (`ReconnectExhausted`, a refusal such as 401 or
//!   404, a malformed entry), so none of them comes back by itself.

use super::approval::{approval_requested, approval_resolved, decided};
use super::entry::EntryMapper;
use tole_client::{Error, Event, SessionStatus, StreamItem};
use tole_desktop_model::task::TaskEvent;

/// The events of one item of the stream.
pub fn stream_item(mapper: &mut EntryMapper, item: &StreamItem) -> Vec<TaskEvent> {
    match item {
        StreamItem::Heartbeat => Vec::new(),
        StreamItem::Reconnected { .. } => {
            mapper.reset_approvals();
            vec![TaskEvent::Reconnected]
        }
        StreamItem::Event(event) => match event {
            Event::Entry(e) => mapper.entry(e),
            Event::Ready(r) => vec![TaskEvent::Ready { busy: r.busy }],
            Event::TextDelta { text } => vec![TaskEvent::TextDelta { text: text.clone() }],
            Event::Reasoning { .. } => vec![TaskEvent::Reasoning],
            Event::ApprovalRequested(a) => {
                // A row decided on arrival is resolved by the pair itself: no slot.
                if decided(&a.status) || mapper.admit_approval(&a.id) {
                    approval_requested(a)
                } else {
                    Vec::new()
                }
            }
            Event::ApprovalResolved(a) => {
                mapper.release_approval(&a.id);
                vec![approval_resolved(a)]
            }
            Event::Resync { .. } => vec![TaskEvent::Resync],
            Event::Closed { .. } => vec![TaskEvent::Closed],
            // The discussion stream's frames (#1094) never reach a task's.
            Event::ToolStarted { .. }
            | Event::ToolFinished { .. }
            | Event::Changed(_)
            | Event::DiscussionState { .. }
            | Event::Other { .. } => Vec::new(),
        },
    }
}

/// What `ResumingStream::next_item` returned: an item, the end of the stream
/// (nothing to say, the server's `closed` frame already did) or an error.
pub fn stream_result(
    mapper: &mut EntryMapper,
    next: &Result<Option<StreamItem>, Error>,
) -> Vec<TaskEvent> {
    match next {
        Ok(Some(item)) => stream_item(mapper, item),
        Ok(None) => Vec::new(),
        Err(_) => vec![TaskEvent::ConnectionLost],
    }
}

/// A `GET /status` answer: whether a turn runs on the server.
pub fn status_ready(status: &SessionStatus) -> TaskEvent {
    TaskEvent::Ready { busy: status.busy }
}
