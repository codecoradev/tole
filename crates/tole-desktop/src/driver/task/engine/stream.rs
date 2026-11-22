//! The live stream: its items go through the adapter into the model.
//!
//! The reader thread reports when a stream is up (its first item arrived; a
//! stream opened again after a loss is `reopened`) and every item and error;
//! `stream_item` and `stream_result` map them, `Reconnected` and a first open
//! each ask for the queue of approvals to be read (`refresh`).

use tole_client::{Error, StreamItem};

use super::{Engine, Now};
use crate::adapter::task::{stream_item, stream_result};

impl Engine {
    pub(super) fn up(&mut self, reopened: bool, now: Now) -> bool {
        {
            let mut shared = self.store.lock();
            shared.facts.connected = true;
            shared.facts.stream_up = true;
            shared.facts.parked = false;
        }
        if reopened {
            // The same as a stream that reconnected by itself: text is stale,
            // the link is live, the queue is read again.
            let events = stream_item(&mut self.mapper, &StreamItem::Reconnected { after: 0 });
            self.apply(events);
        }
        self.refresh.want(now);
        true
    }

    pub(super) fn stream(&mut self, next: Result<Option<StreamItem>, Error>, now: Now) -> bool {
        if matches!(next, Ok(Some(StreamItem::Reconnected { .. }))) {
            self.refresh.want(now);
        }
        if next.is_err() {
            self.store.lock().facts.stream_up = false;
        }
        let events = stream_result(&mut self.mapper, &next);
        self.apply(events)
    }
}
