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
        let mut cur = self.open.load(Ordering::Relaxed);
        loop {
            if cur >= self.max {
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
        let len = std::fs::metadata(&self.path)
            .map_err(|e| e.to_string())?
            .len();
        if self.last_len == Some(len) {
            return Ok(Vec::new());
        }
        let storage = JsonlStorage::open_read_only(&self.path).map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    #[test]
    fn frames_are_well_formed_sse() {
        let f = frame("entry", Some(7), &json!({"a": "x\ny"}));
        assert_eq!(f, "id: 7\nevent: entry\ndata: {\"a\":\"x\\ny\"}\n\n");
        // A newline in a string value is escaped: one data line only.
        assert_eq!(f.matches("data:").count(), 1);
        assert!(!f.contains("x\ny"));
        let e = frame("text_delta", None, &json!({"text": "hi"}));
        assert!(!e.starts_with("id:"), "ephemeral frames carry no id");
        assert_eq!(e, "event: text_delta\ndata: {\"text\":\"hi\"}\n\n");
        assert!(HEARTBEAT.starts_with(':') && HEARTBEAT.ends_with("\n\n"));
    }

    #[test]
    fn ephemeral_names_and_shapes() {
        let cases = [
            (
                Ephemeral::TextDelta("a".into()),
                "text_delta",
                json!({"text": "a"}),
            ),
            (
                Ephemeral::Reasoning("r".into()),
                "reasoning",
                json!({"text": "r"}),
            ),
            (
                Ephemeral::ToolStarted {
                    tool: "t".into(),
                    input: json!({"k": 1}),
                },
                "tool_started",
                json!({"tool": "t", "input": {"k": 1}}),
            ),
            (
                Ephemeral::ToolFinished {
                    tool: "t".into(),
                    ok: false,
                    preview: "p".into(),
                },
                "tool_finished",
                json!({"tool": "t", "ok": false, "preview": "p"}),
            ),
        ];
        for (e, name, data) in cases {
            assert_eq!((e.name(), e.data()), (name, data));
            assert!(e.to_frame().starts_with(&format!("event: {name}\n")));
        }
    }

    #[test]
    fn the_bus_fans_out_never_blocks_and_counts_drops() {
        let bus = Arc::new(Bus::default());
        let a = bus.subscribe();
        let b = bus.subscribe();
        bus.publish(Ephemeral::TextDelta("x".into()));
        assert_eq!(
            a.rx.recv_timeout(Duration::from_millis(50)).unwrap().name(),
            "text_delta"
        );
        assert_eq!(
            b.rx.recv_timeout(Duration::from_millis(50)).unwrap().name(),
            "text_delta"
        );

        // A stream that never reads: publishing past its backlog must not
        // block, and the loss is counted (then reset by take_dropped).
        for _ in 0..SUBSCRIBER_BACKLOG + 10 {
            bus.publish(Ephemeral::TextDelta("y".into()));
        }
        assert_eq!(
            a.take_dropped(),
            10,
            "backlog 256 filled by 'y' minus the drained 'x'"
        );
        assert_eq!(a.take_dropped(), 0);
        // The other stream's queue is independent.
        assert!(b.rx.try_recv().is_ok());
    }

    #[test]
    fn dropping_a_subscription_unregisters_it() {
        let bus = Arc::new(Bus::default());
        assert!(!bus.has_subscribers());
        let s = bus.subscribe();
        assert!(bus.has_subscribers());
        drop(s);
        assert!(!bus.has_subscribers());
        // Publishing with nobody listening is a no-op, not a panic.
        bus.publish(Ephemeral::Reasoning("z".into()));
    }

    #[test]
    fn the_observer_exists_only_while_a_stream_is_open() {
        let reg = BusRegistry::default();
        assert!(
            reg.observer_for("s1").is_none(),
            "nobody watching: no observer"
        );
        let sub = reg.subscribe("s1");
        let obs = reg.observer_for("s1").expect("watched");
        assert!(reg.observer_for("s2").is_none(), "per session");
        obs.text_delta("hello");
        obs.tool_started("write_file", &json!({"path": "a"}));
        obs.tool_finished("write_file", true, "ok");
        obs.reasoning("because");
        let got: Vec<_> = std::iter::from_fn(|| sub.rx.try_recv().ok()).collect();
        assert_eq!(
            got.iter().map(Ephemeral::name).collect::<Vec<_>>(),
            ["text_delta", "tool_started", "tool_finished", "reasoning"]
        );
        drop(sub);
        assert!(reg.observer_for("s1").is_none(), "last stream closed");
        // A new stream after cleanup works.
        let again = reg.subscribe("s1");
        assert!(matches!(
            again.rx.recv_timeout(Duration::from_millis(5)),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn the_stream_limiter_caps_and_frees_slots() {
        let lim = StreamLimiter::new(2);
        let a = lim.try_acquire().unwrap();
        let _b = lim.try_acquire().unwrap();
        assert!(lim.try_acquire().is_none(), "third stream refused");
        assert_eq!(lim.open(), 2);
        drop(a);
        assert_eq!(lim.open(), 1);
        assert!(lim.try_acquire().is_some(), "slot freed by drop");
    }

    fn pa(id: &str, status: ApprovalStatus) -> PendingApproval {
        PendingApproval {
            id: id.into(),
            session_id: "s".into(),
            tool: "write_file".into(),
            description: "d".into(),
            fingerprint: 1,
            created_at_ms: 0,
            status,
            storage_path: PathBuf::new(),
        }
    }

    #[test]
    fn approval_frames_follow_the_queue() {
        let mut w = ApprovalWatch::default();
        // Late joiner: pending is reported, an old decision is not.
        let first = w.poll(&[
            pa("a1", ApprovalStatus::Pending),
            pa("a0", ApprovalStatus::Approved),
        ]);
        assert_eq!(
            first.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["approval_requested"]
        );
        assert_eq!(first[0].1["id"], "a1");
        // Nothing changed: silence.
        assert!(w
            .poll(&[
                pa("a1", ApprovalStatus::Pending),
                pa("a0", ApprovalStatus::Approved)
            ])
            .is_empty());
        // Decided: one resolved frame, with the new status; then silence.
        let r = w.poll(&[
            pa("a1", ApprovalStatus::Approved),
            pa("a0", ApprovalStatus::Approved),
        ]);
        assert_eq!(
            r.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["approval_resolved"]
        );
        assert_eq!(r[0].1["status"], "approved");
        assert!(w.poll(&[pa("a1", ApprovalStatus::Approved)]).is_empty());
        // New and decided between two polls: both frames, in order.
        let both = w.poll(&[pa("a2", ApprovalStatus::Denied)]);
        assert_eq!(
            both.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["approval_requested", "approval_resolved"]
        );
        // Expiry is a resolution too.
        w.poll(&[pa("a3", ApprovalStatus::Pending)]);
        let ex = w.poll(&[pa("a3", ApprovalStatus::Expired)]);
        assert_eq!(ex[0].1["status"], "expired");
    }

    #[test]
    fn the_tail_yields_each_entry_once_in_order_and_survives_a_torn_tail() {
        use tole_core::entry::{EntryType, NewEntry};
        use tole_core::storage::Commit;
        let dir = std::env::temp_dir().join(format!(
            "tole-events-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = JsonlStorage::create(&dir, "t1", None).unwrap();
        let add = |s: &mut JsonlStorage, text: &str| {
            let kind = EntryType::new(EntryType::MESSAGE);
            let payload = json!({"role": "user", "text": text});
            let new = match s.entries().last() {
                None => NewEntry::root(kind, payload),
                Some(p) => NewEntry::with_parent(p.id.clone(), kind, payload),
            };
            s.commit(Commit::new().entry(new)).unwrap();
        };
        add(&mut s, "one");
        add(&mut s, "two");
        let path = dir.join("t1.jsonl");
        let mut tail = EntryTail::new(path.clone(), 0);
        let first = tail.poll().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(tail.last_seq(), first[1].seq);
        assert!(
            tail.poll().unwrap().is_empty(),
            "unchanged file: nothing, no replay"
        );
        // A new commit is picked up exactly once.
        add(&mut s, "three");
        let next = tail.poll().unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].payload["text"], "three");
        assert!(tail.poll().unwrap().is_empty());
        // A writer mid-append (newline-less fragment) yields nothing and breaks nothing.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"[{\"kind\":\"entry\",\"seq\":99").unwrap();
        }
        assert!(tail.poll().unwrap().is_empty());
        // Starting after a seq skips what the client already has.
        let mut late = EntryTail::new(path.clone(), first[0].seq);
        let got = late.poll().unwrap();
        assert_eq!(
            got.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [first[1].seq, next[0].seq]
        );
        // An unreadable file is an error the stream can retry, not a panic.
        assert!(EntryTail::new(dir.join("nope.jsonl"), 0).poll().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
