//! The event stream that survives a dropped connection.

use crate::client::Client;
use crate::error::{Error, Result};
use crate::http::ResponseBody;
use crate::sse::{Event, FrameReader, SseItem};
use std::time::{Duration, Instant};

/// How hard [`ResumingStream`] tries to get a dropped stream back.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Drops and failed connects in a row before the stream gives up with
    /// [`Error::ReconnectExhausted`]. The count starts over when an entry with
    /// a new seq arrives (progress), or when a connection lived at least
    /// `backoff_max` before it dropped (a healthy idle one). A connection
    /// that opens and closes at once counts as a failure even if it sent
    /// `ready` or a heartbeat first, so a flapping server cannot be retried
    /// forever.
    pub max_attempts: u32,
    /// Pause before the first retry; doubles up to `backoff_max`.
    pub backoff_initial: Duration,
    /// Longest pause between retries, and the lifetime that makes a dropped
    /// connection count as healthy (see `max_attempts`).
    pub backoff_max: Duration,
    /// Silence after which a connection counts as dead. Serve sends a
    /// heartbeat every 15 s, so the default 45 s is three missed beats.
    pub idle_timeout: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 8,
            backoff_initial: Duration::from_millis(100),
            backoff_max: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(45),
        }
    }
}

/// What a [`ResumingStream`] yields.
///
/// `Event` is much larger than the other variants on some targets (240 bytes
/// on macOS), which clippy's `large_enum_variant` flags. It is not boxed: the
/// value is moved once per frame and then matched, and a `Box` would stop
/// callers from matching `StreamItem::Event(Event::Entry(e))` in one pattern.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum StreamItem {
    Event(Event),
    /// The server's keep-alive; the connection is healthy.
    Heartbeat,
    /// The connection was re-established, resuming after entry `after`.
    /// Durable entries missed meanwhile follow, once each. Everything else is
    /// best effort and not replayed:
    ///
    /// - text, reasoning and tool frames sent while disconnected are gone:
    ///   treat the streamed text of an unfinished turn as stale and take the
    ///   final answer from its entry;
    /// - an `approval_resolved` for an approval decided before the stream
    ///   reopened is never sent, so after `Reconnected` refresh the queue with
    ///   [`Client::approvals`](crate::Client::approvals);
    /// - `approval_requested` for approvals still pending is sent again after
    ///   every reconnect: dedupe by approval id.
    Reconnected {
        after: u64,
    },
}

/// Follows one session's events across reconnects. Entry frames are delivered
/// exactly once, in order: it resumes with `after=<last seq>` and drops any
/// entry at or below that seq. That guarantee is for entries only; see
/// [`StreamItem::Reconnected`] for what other frames may lose or repeat.
pub struct ResumingStream {
    client: Client,
    session: String,
    last_seq: u64,
    policy: ReconnectPolicy,
    reader: Option<FrameReader<ResponseBody>>,
    failures: u32,
    connected_before: bool,
    closed: bool,
    /// When the current connection was opened.
    connected_at: Option<Instant>,
    /// An error was returned to the caller: the stream is over.
    terminal: bool,
}

impl ResumingStream {
    pub(crate) fn new(
        client: Client,
        session: String,
        after: u64,
        policy: ReconnectPolicy,
    ) -> Self {
        Self {
            client,
            session,
            last_seq: after,
            policy,
            reader: None,
            failures: 0,
            connected_before: false,
            closed: false,
            connected_at: None,
            terminal: false,
        }
    }

    /// `seq` of the last entry delivered (the `after` a resume would use).
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// The next item. `Ok(None)` once the server said `closed`. Blocks, also
    /// while reconnecting.
    ///
    /// The stream is fused: after an `Err` (a refusal such as 401 or 404, a
    /// protocol error, or [`Error::ReconnectExhausted`]) it returns `Ok(None)`
    /// and never connects again, so a loop that logs errors and keeps
    /// iterating cannot hammer the server. Make a new stream, from
    /// [`ResumingStream::last_seq`], to try again.
    pub fn next_item(&mut self) -> Result<Option<StreamItem>> {
        if self.terminal {
            return Ok(None);
        }
        let r = self.advance();
        if r.is_err() {
            self.terminal = true;
            self.reader = None;
        }
        r
    }

    fn advance(&mut self) -> Result<Option<StreamItem>> {
        loop {
            if self.closed {
                return Ok(None);
            }
            let Some(reader) = self.reader.as_mut() else {
                match self.client.events(
                    &self.session,
                    self.last_seq,
                    Some(self.policy.idle_timeout),
                ) {
                    Ok(r) => {
                        self.reader = Some(r);
                        self.connected_at = Some(Instant::now());
                        if std::mem::replace(&mut self.connected_before, true) {
                            return Ok(Some(StreamItem::Reconnected {
                                after: self.last_seq,
                            }));
                        }
                    }
                    Err(e) if retry_get(&e) => self.fail()?,
                    Err(e) => return Err(e),
                }
                continue;
            };
            match reader.next_item() {
                Ok(Some(SseItem::Heartbeat)) => {
                    return Ok(Some(StreamItem::Heartbeat));
                }
                Ok(Some(SseItem::Frame(frame))) => {
                    let event = Event::from_frame(&frame)?;
                    match &event {
                        Event::Entry(e) if e.seq <= self.last_seq => continue,
                        Event::Entry(e) => {
                            // Progress: the only frame that proves the stream works.
                            self.last_seq = e.seq;
                            self.failures = 0;
                        }
                        Event::Closed { .. } => self.closed = true,
                        _ => {}
                    }
                    return Ok(Some(StreamItem::Event(event)));
                }
                // The server only ends a stream with `closed`: a bare end, or
                // a read error or timeout, is a drop.
                Ok(None) | Err(Error::Io(_)) => {
                    self.reader = None;
                    // A connection that lived a while was healthy, just idle.
                    let lived = self.connected_at.take().map(|t| t.elapsed());
                    if lived.is_some_and(|l| l >= self.policy.backoff_max) {
                        self.failures = 0;
                    }
                    self.fail()?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Count a failed attempt and wait before the next one.
    fn fail(&mut self) -> Result<()> {
        self.failures += 1;
        if self.failures > self.policy.max_attempts {
            return Err(Error::ReconnectExhausted(self.policy.max_attempts));
        }
        let shift = (self.failures - 1).min(16);
        let pause = self
            .policy
            .backoff_initial
            .saturating_mul(1 << shift)
            .min(self.policy.backoff_max);
        std::thread::sleep(pause);
        Ok(())
    }
}

/// Whether a failed attempt to open the stream is worth another one. The
/// stream is a GET, so a retry can never repeat an action: besides the
/// transient errors, a timeout (the server accepted and stayed silent past the
/// idle bound) counts. A refusal, a malformed answer or a bad setting does not.
fn retry_get(e: &Error) -> bool {
    e.is_transient() || matches!(e, Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut)
}

impl Iterator for ResumingStream {
    type Item = Result<StreamItem>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_item().transpose()
    }
}
