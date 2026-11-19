use super::*;
use crate::approvals::{ApprovalInfo, ApprovalQueue, ApprovalStatus, EXPIRY_SECS};
use crate::webhook_out::{Destination, KeySource};
use std::collections::BTreeSet;
use std::time::Duration;
use tole_core::web::AddrPolicy;

const NOW: i64 = 1_795_000_000;

fn dest(events: &[Event]) -> Destination {
    Destination {
        key: KeySource::Env("K".into()),
        url: "https://hooks.example.com/tole?token=abc".into(),
        events: events.iter().copied().collect::<BTreeSet<_>>(),
        policy: AddrPolicy::default(),
        allow_agent_jobs: false,
        timeout: Duration::from_secs(10),
    }
}

fn dests(list: &[(&str, &[Event])]) -> Destinations {
    Destinations(list.iter().map(|(n, e)| (n.to_string(), dest(e))).collect())
}

fn info(id: &str) -> ApprovalInfo {
    // ApprovalInfo is non_exhaustive: build one through the queue's own type.
    let q = ApprovalQueue::default();
    let sink = Arc::new(Mutex::new(None));
    let s2 = Arc::clone(&sink);
    q.set_notifier(Some(Arc::new(move |e| {
        if let ApprovalEvent::Requested(i) = e {
            *s2.lock().unwrap() = Some(i);
        }
    })));
    let real = q.queue_entry(
        "s1",
        "write_file",
        "write notes.md",
        7,
        "/home/me/x.jsonl".into(),
    );
    let mut i = sink.lock().unwrap().take().unwrap();
    // Keep the real id (valid) unless a test wants another.
    if id != "real" {
        i.id = id.to_string();
    } else {
        assert_eq!(i.id, real);
    }
    i
}

fn identity(s: &str) -> String {
    s.to_string()
}

fn queued(dest: &str, id: &str, next_at: i64, deadline: i64) -> Queued {
    Queued {
        dest: dest.into(),
        id: id.into(),
        event: Event::ApprovalRequested,
        body: "{}".into(),
        deadline,
        attempts: 0,
        next_at,
        session: None,
    }
}

#[test]
fn a_request_is_built_for_each_destination_that_lists_it() {
    let d = dests(&[
        ("both", &[Event::ApprovalRequested, Event::ApprovalResolved]),
        ("reqonly", &[Event::ApprovalRequested]),
        ("cron", &[Event::CronRunFinished]),
    ]);
    let ev = ApprovalEvent::Requested(info("real"));
    let built = build(&d, &ev, NOW, &identity);
    let names: Vec<&str> = built.iter().map(|q| q.dest.as_str()).collect();
    assert_eq!(names, ["both", "reqonly"]);
    for q in &built {
        let v: serde_json::Value = serde_json::from_str(&q.body).unwrap();
        assert_eq!(v["type"], "approval_requested");
        assert_eq!(v["destination"], q.dest.as_str());
        assert_eq!(v["id"], q.id.as_str());
        assert_eq!(v["data"]["tool"], "write_file");
        assert_eq!(v["data"]["session_id"], "s1");
        assert_eq!(v["data"]["source"], "serve");
        assert_eq!(v["data"]["risk"], "write");
        assert!(q.id.starts_with("approval:") && q.id.ends_with(":requested"));
        assert_eq!(q.event, Event::ApprovalRequested);
    }
}

#[test]
fn a_resolution_says_how_the_approval_ended() {
    let d = dests(&[("g", &[Event::ApprovalResolved])]);
    for (status, word) in [
        (ApprovalStatus::Approved, "approved"),
        (ApprovalStatus::Denied, "denied"),
        (ApprovalStatus::Expired, "expired"),
    ] {
        let ev = ApprovalEvent::Resolved {
            info: info("real"),
            status,
        };
        let built = build(&d, &ev, NOW, &identity);
        assert_eq!(built.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&built[0].body).unwrap();
        assert_eq!(v["type"], "approval_resolved");
        assert_eq!(v["data"]["status"], word);
        assert!(built[0].id.ends_with(":resolved"));
    }
    // A destination that lists only `approval_requested` gets no resolution.
    let only_req = dests(&[("g", &[Event::ApprovalRequested])]);
    let ev = ApprovalEvent::Resolved {
        info: info("real"),
        status: ApprovalStatus::Approved,
    };
    assert!(build(&only_req, &ev, NOW, &identity).is_empty());
    // Pending is not a resolution.
    let ev = ApprovalEvent::Resolved {
        info: info("real"),
        status: ApprovalStatus::Pending,
    };
    assert!(build(&d, &ev, NOW, &identity).is_empty());
}

#[test]
fn no_destination_listing_approvals_means_nothing_is_built() {
    let d = dests(&[("cron", &[Event::CronRunFinished])]);
    let ev = ApprovalEvent::Requested(info("real"));
    assert!(build(&d, &ev, NOW, &identity).is_empty());
    assert!(build(&Destinations::default(), &ev, NOW, &identity).is_empty());
}

#[test]
fn the_body_is_redacted_and_carries_no_path_or_url() {
    let canary = "approval-event-canary-5d4c3b2a1f";
    let d = dests(&[("g", &[Event::ApprovalRequested])]);
    let mut i = info("real");
    i.description = format!("curl -H 'X: {canary}' https://example.com");
    let redact = |s: &str| s.replace(canary, "«canary»");
    let built = build(&d, &ApprovalEvent::Requested(i), NOW, &redact);
    let body = &built[0].body;
    assert!(!body.contains(canary), "{body}");
    assert!(body.contains("«canary»"), "{body}");
    assert!(!body.contains("/home/me"), "{body}");
    assert!(
        !body.contains("token=abc") && !body.contains("hooks.example.com"),
        "{body}"
    );
}

#[test]
fn an_approval_id_that_is_not_a_valid_delivery_id_is_never_built() {
    let d = dests(&[("g", &[Event::ApprovalRequested, Event::ApprovalResolved])]);
    let bad = info("apr-with space");
    assert!(build(&d, &ApprovalEvent::Requested(bad.clone()), NOW, &identity).is_empty());
    let r = ApprovalEvent::Resolved {
        info: bad,
        status: ApprovalStatus::Denied,
    };
    assert!(build(&d, &r, NOW, &identity).is_empty());
}

#[test]
fn a_request_is_worth_sending_until_it_would_have_expired() {
    let d = dests(&[("g", &[Event::ApprovalRequested, Event::ApprovalResolved])]);
    let i = info("real");
    let created = (i.created_at_ms / 1000) as i64;
    let req = build(&d, &ApprovalEvent::Requested(i.clone()), created, &identity);
    assert_eq!(req[0].deadline, created + EXPIRY_SECS as i64);
    // A resolution that happens at `at` is worth sending for as long as an
    // approval lives, not until the approval's own (already passed) expiry.
    let at = created + EXPIRY_SECS as i64 + 5;
    let res = build(
        &d,
        &ApprovalEvent::Resolved {
            info: i,
            status: ApprovalStatus::Expired,
        },
        at,
        &identity,
    );
    assert_eq!(res[0].deadline, at + EXPIRY_SECS as i64);
}

#[test]
fn the_queue_holds_256_per_destination_and_counts_what_it_drops() {
    let q = VolatileQueue::new();
    for n in 0..MAX_PER_DESTINATION {
        q.push(queued("a", &format!("e{n}"), 0, NOW + 900)).unwrap();
    }
    assert_eq!(q.len("a"), 256);
    assert_eq!(
        q.push(queued("a", "one-more", 0, NOW + 900)),
        Err(Refused::Full)
    );
    assert_eq!(q.dropped(), 1);
    // The bound is per destination.
    q.push(queued("b", "x", 0, NOW + 900)).unwrap();
    assert_eq!(q.len("b"), 1);
    // The ones that fit are still there, in order (the newest was dropped).
    let taken = q.take_due(NOW);
    assert_eq!(taken.due.iter().find(|e| e.dest == "a").unwrap().id, "e0");
}

#[test]
fn take_due_gives_one_oldest_due_event_per_destination() {
    let q = VolatileQueue::new();
    q.push(queued("a", "a1", 0, NOW + 900)).unwrap();
    q.push(queued("a", "a2", 0, NOW + 900)).unwrap();
    q.push(queued("b", "b1", 0, NOW + 900)).unwrap();
    let t = q.take_due(NOW);
    let mut ids: Vec<&str> = t.due.iter().map(|e| e.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["a1", "b1"]);
    assert_eq!(q.len("a"), 1);
    let t = q.take_due(NOW);
    assert_eq!(t.due.len(), 1);
    assert_eq!(t.due[0].id, "a2");
    assert!(q.is_empty());
}

#[test]
fn an_event_waiting_for_a_retry_does_not_hold_back_the_ones_behind_it() {
    let q = VolatileQueue::new();
    q.push(queued("a", "slow", NOW + 30, NOW + 900)).unwrap();
    q.push(queued("a", "next", 0, NOW + 900)).unwrap();
    let t = q.take_due(NOW);
    assert_eq!(t.due.len(), 1);
    assert_eq!(t.due[0].id, "next", "the retry waits, the next one goes");
    // Once its time comes it is taken.
    let t = q.take_due(NOW + 30);
    assert_eq!(t.due[0].id, "slow");
}

#[test]
fn an_event_past_its_deadline_is_returned_as_expired_and_never_due() {
    let q = VolatileQueue::new();
    q.push(queued("a", "stale", 0, NOW - 1)).unwrap();
    q.push(queued("a", "fresh", 0, NOW + 900)).unwrap();
    let t = q.take_due(NOW);
    assert_eq!(
        t.expired.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["stale"]
    );
    assert_eq!(
        t.due.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["fresh"]
    );
}

#[test]
fn a_requeued_event_keeps_its_attempts_and_waits_for_its_time() {
    let q = VolatileQueue::new();
    q.push(queued("a", "e", 0, NOW + 900)).unwrap();
    let mut e = q.take_due(NOW).due.pop().unwrap();
    e.attempts = 1;
    e.next_at = NOW + 2;
    q.requeue(e).unwrap();
    assert!(q.take_due(NOW + 1).due.is_empty());
    let back = q.take_due(NOW + 2).due.pop().unwrap();
    assert_eq!(back.attempts, 1);
}

#[test]
fn debug_of_a_queued_event_hides_the_body() {
    let mut e = queued("a", "e", 0, NOW);
    e.body = "PRIVATE-BODY-TEXT".into();
    assert!(!format!("{e:?}").contains("PRIVATE-BODY-TEXT"));
}

#[test]
fn the_notifier_fills_the_queue_from_a_real_approval_queue() {
    let d = Arc::new(dests(&[(
        "g",
        &[Event::ApprovalRequested, Event::ApprovalResolved],
    )]));
    let q = VolatileQueue::new();
    let approvals = ApprovalQueue::default();
    approvals.set_notifier(Some(notifier(
        Arc::clone(&d),
        Arc::clone(&q),
        Arc::new(|| NOW),
        Arc::new(tole_core::secrets::process_secrets),
    )));
    let id = approvals.queue_entry("s1", "write_file", "write notes.md", 7, "/x".into());
    assert_eq!(q.len("g"), 1, "the request is queued");
    approvals.decide(&id, true);
    approvals.decide(&id, true); // a repeat is not announced
    assert_eq!(q.len("g"), 2, "request + one resolution");
    // The bodies name the approval's own id.
    let taken = q.take_due(NOW);
    let v: serde_json::Value = serde_json::from_str(&taken.due[0].body).unwrap();
    assert_eq!(v["data"]["approval_id"], id.as_str());
}

#[test]
fn the_notifier_never_blocks_on_a_full_queue_and_never_fails_an_approval() {
    let d = Arc::new(dests(&[("g", &[Event::ApprovalRequested])]));
    let q = VolatileQueue::new();
    let approvals = ApprovalQueue::default();
    approvals.set_notifier(Some(notifier(
        d,
        Arc::clone(&q),
        Arc::new(|| NOW),
        Arc::new(tole_core::secrets::process_secrets),
    )));
    let started = std::time::Instant::now();
    for n in 0..(MAX_PER_DESTINATION + 50) {
        approvals.queue_entry("s1", "write_file", &format!("d{n}"), n as u64, "/x".into());
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(q.len("g"), MAX_PER_DESTINATION);
    assert_eq!(q.dropped(), 50);
    // Every approval is still there and decidable.
    assert_eq!(approvals.list(None).len(), MAX_PER_DESTINATION + 50);
}

// ---- turn_finished (#962) ----

use crate::webhook_out::turn::{TurnCode, TurnEnd, TurnSignal, TurnStatus};

fn turn_queued(dest: &str, session: &str, n: u64) -> Queued {
    Queued {
        event: Event::TurnFinished,
        session: Some(session.into()),
        ..queued(dest, &format!("turn:{session}:{n}"), 0, NOW + 900)
    }
}

fn signal(session: &str, status: TurnStatus, seq: u64) -> TurnSignal {
    TurnSignal {
        session_id: session.into(),
        end: TurnEnd {
            status,
            code: None,
            last_seq: seq,
            settle: 1,
        },
        approval_id: None,
    }
}

fn ids(q: &VolatileQueue, dest: &str) -> Vec<String> {
    let m = q.by_dest.lock().unwrap();
    m.get(dest)
        .map(|v| v.iter().map(|e| e.id.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn a_full_queue_replaces_the_oldest_event_of_the_same_session() {
    let q = VolatileQueue::new();
    // 256 events, one per session; the 17th session is the one that comes back.
    for n in 0..MAX_PER_DESTINATION as u64 {
        q.push(turn_queued("g", &format!("s{n}"), 1)).unwrap();
    }
    q.push(turn_queued("g", "s17", 2)).unwrap();
    assert_eq!(q.len("g"), MAX_PER_DESTINATION, "still bounded");
    assert_eq!(q.dropped(), 1, "the replaced one is counted");
    let all = ids(&q, "g");
    assert!(
        !all.contains(&"turn:s17:1".to_string()),
        "oldest of s17 gone"
    );
    assert_eq!(all.last().unwrap(), "turn:s17:2", "the newest stands");
    assert!(all.contains(&"turn:s16:1".to_string()), "others untouched");
}

#[test]
fn a_full_queue_of_distinct_sessions_drops_the_new_event_and_counts_it() {
    let q = VolatileQueue::new();
    for n in 0..MAX_PER_DESTINATION as u64 {
        q.push(turn_queued("g", &format!("s{n}"), 1)).unwrap();
    }
    assert_eq!(q.push(turn_queued("g", "fresh", 1)), Err(Refused::Full));
    assert_eq!(q.len("g"), MAX_PER_DESTINATION);
    assert_eq!(q.dropped(), 1);
    assert!(!ids(&q, "g").contains(&"turn:fresh:1".to_string()));
}

#[test]
fn a_full_queue_drops_a_superseded_event_of_another_session_first() {
    let q = VolatileQueue::new();
    q.push(turn_queued("g", "busy", 1)).unwrap();
    q.push(turn_queued("g", "busy", 2)).unwrap(); // busy:1 is superseded
    for n in 2..MAX_PER_DESTINATION as u64 {
        q.push(turn_queued("g", &format!("s{n}"), 1)).unwrap();
    }
    q.push(turn_queued("g", "fresh", 1)).unwrap();
    let all = ids(&q, "g");
    assert!(!all.contains(&"turn:busy:1".to_string()));
    assert!(
        all.contains(&"turn:busy:2".to_string()),
        "its latest stands"
    );
    assert!(all.contains(&"turn:fresh:1".to_string()));
    assert_eq!(q.len("g"), MAX_PER_DESTINATION);
}

#[test]
fn an_approval_event_on_a_full_queue_is_still_dropped_not_swapped() {
    let q = VolatileQueue::new();
    for n in 0..MAX_PER_DESTINATION as u64 {
        q.push(turn_queued("g", &format!("s{n}"), 1)).unwrap();
    }
    assert_eq!(
        q.push(queued("g", "approval:x:requested", 0, NOW + 900)),
        Err(Refused::Full)
    );
}

#[test]
fn a_turn_event_is_built_only_for_the_destinations_that_list_it() {
    let d = dests(&[
        ("both", &[Event::TurnFinished, Event::ApprovalRequested]),
        ("cron", &[Event::CronRunFinished]),
        (
            "approvals",
            &[Event::ApprovalRequested, Event::ApprovalResolved],
        ),
    ]);
    let mut refused = Vec::new();
    let out = build_turn(
        &d,
        &signal("s-1", TurnStatus::Completed, 14),
        NOW,
        &identity,
        &mut |n, c| refused.push((n.to_string(), c.to_string())),
    );
    assert!(refused.is_empty());
    assert_eq!(out.len(), 1);
    let q = &out[0];
    assert_eq!((q.dest.as_str(), q.event), ("both", Event::TurnFinished));
    assert_eq!(q.session.as_deref(), Some("s-1"));
    assert_eq!(
        q.deadline,
        NOW + crate::webhook_out::turn::RETRY_HORIZON_SECS
    );
    let v: serde_json::Value = serde_json::from_str(&q.body).unwrap();
    assert_eq!(v["type"], "turn_finished");
    assert_eq!(v["data"]["last_seq"], 14);
    assert_eq!(v["id"], q.id.as_str());
    // Nobody lists it: no event at all.
    let none = dests(&[("cron", &[Event::CronRunFinished])]);
    assert!(build_turn(
        &none,
        &signal("s-1", TurnStatus::Completed, 1),
        NOW,
        &identity,
        &mut |_, _| {}
    )
    .is_empty());
}

#[test]
fn an_unsendable_id_is_refused_with_a_closed_code_and_no_text() {
    let d = dests(&[("g", &[Event::TurnFinished])]);
    let mut refused = Vec::new();
    let out = build_turn(
        &d,
        &signal("a b", TurnStatus::Completed, 1),
        NOW,
        &identity,
        &mut |n, c| refused.push((n.to_string(), c.to_string())),
    );
    assert!(out.is_empty());
    assert_eq!(refused, vec![("g".to_string(), "bad_id".to_string())]);
}

#[test]
fn an_approval_stop_without_a_pending_entry_is_blocked_with_code_approval() {
    let d = dests(&[("g", &[Event::TurnFinished])]);
    let out = build_turn(
        &d,
        &signal("s-1", TurnStatus::ApprovalPending, 5),
        NOW,
        &identity,
        &mut |_, _| {},
    );
    let v: serde_json::Value = serde_json::from_str(&out[0].body).unwrap();
    assert_eq!(v["data"]["status"], "blocked");
    assert_eq!(v["data"]["code"], TurnCode::Approval.as_str());
    assert!(v["data"].get("approval_id").is_none());
    let mut with = signal("s-1", TurnStatus::ApprovalPending, 5);
    with.approval_id = Some("ap-1".into());
    let out = build_turn(&d, &with, NOW, &identity, &mut |_, _| {});
    let v: serde_json::Value = serde_json::from_str(&out[0].body).unwrap();
    assert_eq!(v["data"]["status"], "approval_pending");
    assert_eq!(v["data"]["approval_id"], "ap-1");
}

#[test]
fn the_turn_notifier_pushes_without_io_and_survives_a_full_queue() {
    let d = Arc::new(dests(&[("g", &[Event::TurnFinished])]));
    let q = VolatileQueue::new();
    let n = turn_notifier(
        d,
        Arc::clone(&q),
        Arc::new(|| NOW),
        Arc::new(tole_core::secrets::process_secrets),
    );
    for i in 0..(MAX_PER_DESTINATION + 20) {
        n(signal(&format!("s{i}"), TurnStatus::Completed, 1));
    }
    assert_eq!(q.len("g"), MAX_PER_DESTINATION);
    assert_eq!(q.dropped(), 20);
}
