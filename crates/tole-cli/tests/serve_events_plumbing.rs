//! The pure pieces of the event stream (#524): frames, heartbeat, the bus, the stream limiter, the
//! approval watch and the entry tail. Moved from the unit tests of `serve_support/events.rs`
//! (#1320) with the test names unchanged.

use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tole_cli::approvals::{ApprovalStatus, PendingApproval};
use tole_cli::events::*;
use tole_core::storage::{JsonlStorage, Storage};

mod common;

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
fn the_heartbeat_is_due_after_silence_and_only_a_real_write_resets_it() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let mut hb = Heartbeat::new(Duration::from_secs(15), t0);
    assert!(!hb.due(t0 + Duration::from_secs(14)));
    assert!(hb.due(t0 + Duration::from_secs(15)));
    // Polls that wrote nothing never call note_write, so the deadline
    // does not move however many passes happen in between.
    for s in 1..60 {
        let _ = hb.due(t0 + Duration::from_secs(s));
    }
    assert!(hb.due(t0 + Duration::from_secs(60)));
    // A real write at t=20 pushes the deadline out to t=35.
    hb.note_write(t0 + Duration::from_secs(20));
    assert!(!hb.due(t0 + Duration::from_secs(34)));
    assert!(hb.due(t0 + Duration::from_secs(35)));
    // A clock that appears to go backwards is never "due".
    assert!(!hb.due(t0));
}

#[test]
fn the_heartbeat_interval_defaults_and_clamps() {
    use std::time::Duration;
    let d = Duration::from_secs(HEARTBEAT_SECS);
    assert_eq!(heartbeat_interval(None), d);
    assert_eq!(heartbeat_interval(Some("")), d);
    assert_eq!(heartbeat_interval(Some("soon")), d);
    assert_eq!(heartbeat_interval(Some("-3")), d);
    assert_eq!(heartbeat_interval(Some(" 2 ")), Duration::from_secs(2));
    assert_eq!(heartbeat_interval(Some("0")), Duration::from_secs(1));
    assert_eq!(
        heartbeat_interval(Some("999999")),
        Duration::from_secs(3600)
    );
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
        seq: 0,
        summary: None,
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
    let dir = common::TestDir::new("tole-events");
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
