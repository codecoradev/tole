//! Event stream plumbing for remote UI clients (#524 part 3, epic #523).
//!
//! `GET /sessions/{id}/events` is a Server-Sent Events stream with two kinds
//! of frames, deliberately separate:
//!
//! - **Durable** (`event: entry`, `id: <seq>`): entries from the session's
//!   append-only JSONL, each carrying its `seq`. Because the log is the
//!   source, `after=<seq>` (or `Last-Event-ID`) replays losslessly after a
//!   dropped connection. [`EntryTail`] reads them.
//! - **Ephemeral** (no `id`): streamed text, reasoning and tool start/finish
//!   from the turn's [`TurnObserver`], plus approval frames. Best-effort and
//!   NOT replayable (deltas are never stored; the final answer is an entry).
//!   A slow client loses ephemeral frames, never the turn: the bus is
//!   bounded, never blocks, and the stream says `resync` when it dropped some.
//!
//! This module holds the pure, unit-tested pieces; the blocking stream loop
//! lives in `serve.rs` next to the HTTP plumbing.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use tole_core::entry::Entry;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::turn::TurnObserver;

use crate::approvals::{approval_json, ApprovalStatus, PendingApproval};

/// Seconds of silence after which a heartbeat comment is sent.
pub const HEARTBEAT_SECS: u64 = 15;
/// Most concurrently open event streams per server. A stream pins one
/// connection thread, so streams are capped apart from (and counted within)
/// the server's overall connection cap: they must not starve `/approvals`
/// and `/prompt`.
pub const MAX_STREAMS: usize = 8;
/// Per-subscriber backlog of ephemeral frames before new ones are dropped.
pub const SUBSCRIBER_BACKLOG: usize = 256;

/// One SSE frame. `id` makes the browser/client remember a durable position;
/// ephemeral frames carry none so a reconnect never skips real entries.
/// `data` is compact JSON, which never contains a raw newline.
pub fn frame(event: &str, id: Option<u64>, data: &Value) -> String {
    let mut out = String::new();
    if let Some(id) = id {
        out.push_str(&format!("id: {id}\n"));
    }
    out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    out
}

/// Env var overriding [`HEARTBEAT_SECS`] (whole seconds, 1 to 3600): for a
/// proxy with a short idle timeout in front of `tole serve`, and for tests.
pub const HEARTBEAT_ENV: &str = "TOLE_SERVE_HEARTBEAT_SECS";

/// The heartbeat interval for `value` of [`HEARTBEAT_ENV`]: the default for
/// an unset, blank or unparsable value, otherwise clamped to 1..=3600 s.
pub fn heartbeat_interval(value: Option<&str>) -> std::time::Duration {
    let secs = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|n| n.clamp(1, 3600))
        .unwrap_or(HEARTBEAT_SECS);
    std::time::Duration::from_secs(secs)
}

/// When the next heartbeat is due: `every` after the last time bytes were
/// ACTUALLY written to the peer. Only a real write calls [`Self::note_write`];
/// a pass that wrote nothing must not, or an idle stream would never
/// heartbeat and a dead peer would go unnoticed. Time is injected so the rule
/// is unit-testable without waiting.
pub struct Heartbeat {
    last_write: std::time::Instant,
    every: std::time::Duration,
}

impl Heartbeat {
    pub fn new(every: std::time::Duration, now: std::time::Instant) -> Self {
        Self {
            last_write: now,
            every,
        }
    }

    /// Bytes reached the socket at `now`.
    pub fn note_write(&mut self, now: std::time::Instant) {
        self.last_write = now;
    }

    /// A heartbeat should be written at `now`.
    pub fn due(&self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.last_write) >= self.every
    }
}

/// A comment frame: ignored by clients, keeps the connection alive and lets
/// a dead peer surface as a write error.
pub const HEARTBEAT: &str = ": ping\n\n";

/// The durable frame for one log entry.
pub fn entry_frame(e: &Entry) -> String {
    frame(
        "entry",
        Some(e.seq),
        &serde_json::to_value(e).unwrap_or(Value::Null),
    )
}

/// A best-effort, non-replayable event from a running turn.
#[derive(Clone, Debug, PartialEq)]
pub enum Ephemeral {
    TextDelta(String),
    Reasoning(String),
    ToolStarted {
        tool: String,
        input: Value,
    },
    ToolFinished {
        tool: String,
        ok: bool,
        preview: String,
    },
}

impl Ephemeral {
    /// The SSE `event:` name.
    pub fn name(&self) -> &'static str {
        match self {
            Ephemeral::TextDelta(_) => "text_delta",
            Ephemeral::Reasoning(_) => "reasoning",
            Ephemeral::ToolStarted { .. } => "tool_started",
            Ephemeral::ToolFinished { .. } => "tool_finished",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            Ephemeral::TextDelta(t) | Ephemeral::Reasoning(t) => json!({"text": t}),
            Ephemeral::ToolStarted { tool, input } => json!({"tool": tool, "input": input}),
            Ephemeral::ToolFinished { tool, ok, preview } => {
                json!({"tool": tool, "ok": ok, "preview": preview})
            }
        }
    }

    pub fn to_frame(&self) -> String {
        frame(self.name(), None, &self.data())
    }
}

struct Sub {
    id: u64,
    tx: SyncSender<Ephemeral>,
    dropped: Arc<AtomicU64>,
}

/// Fan-out of one session's ephemeral events to its open streams.
#[derive(Default)]
pub struct Bus {
    subs: Mutex<Vec<Sub>>,
    next_id: AtomicU64,
}

impl Bus {
    /// Register a stream. Dropping the [`Subscription`] unregisters it.
    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        let (tx, rx) = sync_channel(SUBSCRIBER_BACKLOG);
        let dropped = Arc::new(AtomicU64::new(0));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.subs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Sub {
                id,
                tx,
                dropped: Arc::clone(&dropped),
            });
        Subscription {
            rx,
            dropped,
            id,
            bus: Arc::clone(self),
        }
    }

    /// Hand `e` to every stream without ever blocking the turn: a full
    /// backlog drops the event for that stream and counts it, a closed
    /// stream is removed.
    pub fn publish(&self, e: Ephemeral) {
        let mut subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        subs.retain(|s| match s.tx.try_send(e.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                s.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn has_subscribers(&self) -> bool {
        !self
            .subs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
    }
}

/// One open stream's view of the bus.
pub struct Subscription {
    pub rx: Receiver<Ephemeral>,
    dropped: Arc<AtomicU64>,
    id: u64,
    bus: Arc<Bus>,
}

impl Subscription {
    /// Ephemeral frames lost to a full backlog since the last call.
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus
            .subs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|s| s.id != self.id);
    }
}

/// Publishes a turn's observer callbacks to a [`Bus`].
pub struct BusObserver(Arc<Bus>);

impl TurnObserver for BusObserver {
    fn tool_started(&self, tool: &str, input: &Value) {
        self.0.publish(Ephemeral::ToolStarted {
            tool: tool.to_string(),
            input: input.clone(),
        });
    }
    fn tool_finished(&self, tool: &str, ok: bool, preview: &str) {
        self.0.publish(Ephemeral::ToolFinished {
            tool: tool.to_string(),
            ok,
            preview: preview.to_string(),
        });
    }
    fn reasoning(&self, text: &str) {
        self.0.publish(Ephemeral::Reasoning(text.to_string()));
    }
    fn text_delta(&self, text: &str) {
        self.0.publish(Ephemeral::TextDelta(text.to_string()));
    }
}

/// The per-session buses of a serve process.
#[derive(Default)]
pub struct BusRegistry {
    buses: Mutex<HashMap<String, Arc<Bus>>>,
}

impl BusRegistry {
    /// Open a stream on `session` (creating its bus on first use).
    pub fn subscribe(&self, session: &str) -> Subscription {
        let mut buses = self.buses.lock().unwrap_or_else(|p| p.into_inner());
        // Sessions nobody watches any more must not accumulate.
        buses.retain(|_, b| b.has_subscribers());
        let bus = buses
            .entry(session.to_string())
            .or_insert_with(|| Arc::new(Bus::default()));
        bus.subscribe()
    }

    /// The observer a turn on `session` should use: `Some` only while a
    /// stream is open, so a session nobody watches runs exactly as it did
    /// before streams existed (attaching an observer turns on provider
    /// streaming). A stream that opens mid-turn gets durable entries but no
    /// text deltas for that turn.
    pub fn observer_for(&self, session: &str) -> Option<Arc<dyn TurnObserver>> {
        let bus = self
            .buses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(session)
            .filter(|b| b.has_subscribers())
            .cloned()?;
        Some(Arc::new(BusObserver(bus)))
    }
}

/// Caps open streams (see [`MAX_STREAMS`]).
pub struct StreamLimiter {
    open: AtomicUsize,
    max: usize,
}

impl StreamLimiter {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            open: AtomicUsize::new(0),
            max,
        })
    }

    /// A slot, or `None` when `max` streams are already open.
    pub fn try_acquire(self: &Arc<Self>) -> Option<StreamSlot> {
        self.try_acquire_keeping(0)
    }

    /// A slot, or `None` when fewer than `keep` would stay free: a read-only token cannot take the
    /// last `keep` slots, so it cannot lock the owner out of the event streams.
    pub fn try_acquire_keeping(self: &Arc<Self>, keep: usize) -> Option<StreamSlot> {
        let mut cur = self.open.load(Ordering::Relaxed);
        loop {
            if cur + keep >= self.max {
                return None;
            }
            match self
                .open
                .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return Some(StreamSlot(Arc::clone(self))),
                Err(now) => cur = now,
            }
        }
    }

    pub fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }
}

/// Frees its slot on drop (including a panicking stream).
pub struct StreamSlot(Arc<StreamLimiter>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Follows a session's JSONL by `seq`: yields each entry once, in order.
///
/// Cheap when nothing changed (one `stat`); otherwise replays the file
/// read-only (a torn tail is skipped, never truncated) and returns what is
/// past `last_seq`, so it works while a turn holds the live storage lock.
pub struct EntryTail {
    path: PathBuf,
    last_seq: u64,
    last_len: Option<u64>,
}

impl EntryTail {
    /// Start after `after` (0 = from the beginning).
    pub fn new(path: PathBuf, after: u64) -> Self {
        Self {
            path,
            last_seq: after,
            last_len: None,
        }
    }

    /// `seq` of the last entry returned, or the starting `after`.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn poll(&mut self) -> Result<Vec<Entry>, String> {
        // Errors carry a kind only, never a path or file content: the host
        // may log them.
        let len = std::fs::metadata(&self.path)
            .map_err(|e| format!("stat: {:?}", e.kind()))?
            .len();
        if self.last_len == Some(len) {
            return Ok(Vec::new());
        }
        let storage = JsonlStorage::open_read_only(&self.path).map_err(|e| match e {
            tole_core::storage::StorageError::Io(io) => format!("read: io {:?}", io.kind()),
            tole_core::storage::StorageError::Corrupt(_) => "read: corrupt log".to_string(),
            _ => "read: log not readable".to_string(),
        })?;
        let fresh: Vec<Entry> = storage
            .entries()
            .iter()
            .filter(|e| e.seq > self.last_seq)
            .cloned()
            .collect();
        if let Some(last) = fresh.last() {
            self.last_seq = last.seq;
        }
        // Recorded AFTER the replay: a write that landed in between makes the
        // next poll see a different length and replay again (seq filters
        // duplicates), never a missed entry.
        self.last_len = Some(len);
        Ok(fresh)
    }
}

/// Turns the approval queue's state into `approval_requested` /
/// `approval_resolved` frames for one session.
#[derive(Default)]
pub struct ApprovalWatch {
    seen: HashMap<String, ApprovalStatus>,
    primed: bool,
}

impl ApprovalWatch {
    /// The frames implied by `now` since the previous call. The first call
    /// reports what is pending (a client that connects late must see it) and
    /// stays silent about decisions made before the stream opened.
    pub fn poll(&mut self, now: &[PendingApproval]) -> Vec<(&'static str, Value)> {
        let first = !self.primed;
        self.primed = true;
        let mut out = Vec::new();
        for e in now {
            let pending = e.status == ApprovalStatus::Pending;
            match self.seen.get(&e.id).copied() {
                None => {
                    self.seen.insert(e.id.clone(), e.status);
                    if pending {
                        out.push(("approval_requested", approval_json(e)));
                    } else if !first {
                        // Queued and decided between two polls.
                        out.push(("approval_requested", approval_json(e)));
                        out.push(("approval_resolved", approval_json(e)));
                    }
                }
                Some(prev) if prev == ApprovalStatus::Pending && !pending => {
                    self.seen.insert(e.id.clone(), e.status);
                    out.push(("approval_resolved", approval_json(e)));
                }
                Some(_) => {}
            }
        }
        out
    }
}
