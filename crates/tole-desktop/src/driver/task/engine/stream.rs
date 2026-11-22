//! The live stream: its items go through the adapter into the model.
//!
//! The reader thread reports when a stream is up (its first item arrived; a
//! stream opened again after a loss is `reopened`) and every item and error;
//! `stream_item` and `stream_result` map them, `Reconnected` and a first open
//! each ask for the queue of approvals to be read (`refresh`).

use tole_client::{Error, StreamItem};

use super::{millis, Engine, Now};
use crate::adapter::task::{stream_item, stream_result};
use std::time::Duration;

/// While the stream is not up, the status and the queue are read over REST
/// every 10 s, doubling to 60 s, so Stop and the approval cards work when the
/// stream is slow to open, refused (429 while an old reader of the same page
/// still holds its slot) or parked. The first read comes 5 s after the
/// history, so a stream that opens at once costs nothing.
const POLL_FIRST: Duration = Duration::from_secs(5);
const POLL_BASE: Duration = Duration::from_secs(10);
const POLL_MAX: Duration = Duration::from_secs(60);

/// Whether a stream is up, and when the REST reads are due while it is not.
#[derive(Default)]
pub(super) struct Link {
    pub(super) live: bool,
    due: Option<u64>,
    reads: u32,
}

impl Engine {
    pub(super) fn up(&mut self, reopened: bool, now: Now) -> bool {
        {
            let mut shared = self.store.lock();
            shared.facts.connected = true;
            shared.facts.stream_up = true;
            shared.facts.parked = false;
        }
        self.link = Link {
            live: true,
            ..Link::default()
        };
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
            self.link.live = false;
            self.link.due = None;
            self.store.lock().facts.stream_up = false;
        }
        let events = stream_result(&mut self.mapper, &next);
        self.watch.activity(&events);
        self.apply(events)
    }

    /// Read the status and the queue by REST while no stream is up (see
    /// [`POLL_FIRST`]). Called after every input, it only looks at the time.
    pub(super) fn poll_while_not_live(&mut self, now: Now) {
        if self.link.live || self.hist.reading() {
            return;
        }
        match self.link.due {
            None => self.link.due = Some(now.mono.saturating_add(millis(POLL_FIRST))),
            Some(at) if now.mono >= at => {
                self.refresh.want(now);
                self.watch.check_soon(now);
                let wait = POLL_BASE.saturating_mul(1 << self.link.reads.min(3));
                self.link.reads = self.link.reads.saturating_add(1);
                self.link.due = Some(now.mono.saturating_add(millis(wait.min(POLL_MAX))));
            }
            Some(_) => {}
        }
    }
}
