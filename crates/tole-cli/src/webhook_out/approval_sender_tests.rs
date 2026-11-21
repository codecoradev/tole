use super::*;
use crate::approvals::ApprovalQueue;
use crate::webhook_out::send::Reply;
use crate::webhook_out::volatile::notifier;
use crate::webhook_out::{Destination, Event, KeySource};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tole_core::web::{AddrPolicy, PostError};

const NOW: i64 = 1_795_000_000;
const KEY: &[u8] = b"tole-test-key-0123456789abcdef";

struct Seen {
    url: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

/// Answers from a script, one result per request, then 200s.
struct Fake {
    results: RefCell<Vec<Result<Reply, PostError>>>,
    seen: RefCell<Vec<Seen>>,
}

impl Fake {
    fn new(results: Vec<Result<Reply, PostError>>) -> Fake {
        Fake {
            results: RefCell::new(results),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Poster for Fake {
    fn post(&self, req: &Request<'_>) -> Result<Reply, PostError> {
        self.seen.borrow_mut().push(Seen {
            url: req.url.into(),
            body: req.body.to_vec(),
            headers: req.headers.to_vec(),
        });
        let mut q = self.results.borrow_mut();
        if q.is_empty() {
            status(200)
        } else {
            q.remove(0)
        }
    }
}

fn status(s: u16) -> Result<Reply, PostError> {
    Ok(Reply {
        status: s,
        retry_after: None,
    })
}

struct FixedKey(Option<&'static [u8]>);

impl Keys for FixedKey {
    fn key(&self, _: &Destination) -> Result<Vec<u8>, &'static str> {
        self.0.map(<[u8]>::to_vec).ok_or("key_unavailable")
    }
}

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

fn dests(list: &[(&str, &[Event])]) -> Arc<Destinations> {
    Arc::new(Destinations(
        list.iter().map(|(n, e)| (n.to_string(), dest(e))).collect(),
    ))
}

const BOTH: &[Event] = &[Event::ApprovalRequested, Event::ApprovalResolved];

/// An approval queue wired to a volatile queue, and a sender over it.
struct Rig {
    approvals: Arc<ApprovalQueue>,
    queue: Arc<VolatileQueue>,
    sender: ApprovalSender,
    expired: Arc<AtomicUsize>,
    dests: Arc<Destinations>,
}

fn rig(dests: Arc<Destinations>) -> Rig {
    let approvals = Arc::new(ApprovalQueue::default());
    let queue = VolatileQueue::new();
    approvals.set_notifier(Some(notifier(
        Arc::clone(&dests),
        Arc::clone(&queue),
        Arc::new(|| NOW),
        Arc::new(tole_core::secrets::process_secrets),
    )));
    let expired = Arc::new(AtomicUsize::new(0));
    let e = Arc::clone(&expired);
    let sender = ApprovalSender::new(
        Arc::clone(&queue),
        Box::new(move || {
            e.fetch_add(1, Ordering::SeqCst);
        }),
    );
    Rig {
        approvals,
        queue,
        sender,
        expired,
        dests,
    }
}

impl Rig {
    fn look(
        &mut self,
        now: i64,
        fake: &Fake,
        key: Option<&'static [u8]>,
    ) -> (Looked, Vec<(String, String)>) {
        let failed = RefCell::new(Vec::new());
        let l = self
            .sender
            .look(&self.dests, now, fake, &FixedKey(key), &mut |d, c| {
                failed.borrow_mut().push((d.to_string(), c.to_string()))
            });
        (l, failed.into_inner())
    }
}

fn header<'a>(h: &'a [(String, String)], k: &str) -> &'a str {
    h.iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
        .unwrap()
}

#[test]
fn a_request_and_its_resolution_are_sent_signed() {
    let mut r = rig(dests(&[("g", BOTH)]));
    let fake = Fake::new(vec![]);
    let id = r
        .approvals
        .queue_entry("s1", "write_file", "write notes.md", 7, "/x".into());
    let (l, failed) = r.look(NOW, &fake, Some(KEY));
    assert_eq!((l.sent, l.retried, l.gave_up), (1, 0, 0));
    assert!(failed.is_empty());
    r.approvals.decide(&id, true);
    let (l, _) = r.look(NOW + 1, &fake, Some(KEY));
    assert_eq!(l.sent, 1);

    let seen = fake.seen.borrow();
    assert_eq!(seen.len(), 2);
    for (q, ty) in seen.iter().zip(["approval_requested", "approval_resolved"]) {
        assert_eq!(q.url, "https://hooks.example.com/tole?token=abc");
        let body: serde_json::Value = serde_json::from_slice(&q.body).unwrap();
        assert_eq!(body["type"], ty);
        assert_eq!(body["data"]["approval_id"], id.as_str());
        assert_eq!(header(&q.headers, "X-Tole-Event"), ty);
        // The native v2 signature over exactly these bytes.
        let ts = header(&q.headers, "X-Tole-Timestamp");
        assert_eq!(
            header(&q.headers, "X-Tole-Signature"),
            tole_core::webhook::sign(
                KEY,
                ts,
                header(&q.headers, "X-Tole-Delivery"),
                Some(ty),
                &q.body
            )
        );
    }
    assert_eq!(
        seen[1]
            .headers
            .iter()
            .find(|(k, _)| k == "X-Tole-Timestamp")
            .unwrap()
            .1,
        (NOW + 1).to_string()
    );
}

#[test]
fn a_transient_failure_is_retried_2_10_30_120_seconds_then_given_up() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    let fake = Fake::new(vec![status(500); 5]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    let mut at = NOW;
    let mut sends = vec![];
    let mut gave_up = vec![];
    for wait in [0, 2, 10, 30, 120] {
        at += wait;
        // Not before its time.
        if wait > 0 {
            let (l, _) = r.look(at - 1, &fake, Some(KEY));
            assert_eq!(
                (l.sent, l.retried, l.gave_up),
                (0, 0, 0),
                "early at +{wait}"
            );
        }
        let (l, failed) = r.look(at, &fake, Some(KEY));
        sends.push(l.retried + l.gave_up);
        gave_up.extend(failed);
    }
    assert_eq!(fake.seen.borrow().len(), 5, "five attempts in all");
    assert_eq!(sends, [1, 1, 1, 1, 1]);
    assert_eq!(gave_up, vec![("g".to_string(), "http_500".to_string())]);
    // Nothing is left to send.
    assert!(r.queue.is_empty());
}

#[test]
fn every_retry_carries_the_same_id_and_body_with_a_new_timestamp() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    let fake = Fake::new(vec![status(503), status(200)]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    r.look(NOW, &fake, Some(KEY));
    r.look(NOW + 2, &fake, Some(KEY));
    let seen = fake.seen.borrow();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, seen[1].body);
    assert_eq!(
        header(&seen[0].headers, "X-Tole-Delivery"),
        header(&seen[1].headers, "X-Tole-Delivery")
    );
    assert_ne!(
        header(&seen[0].headers, "X-Tole-Timestamp"),
        header(&seen[1].headers, "X-Tole-Timestamp")
    );
}

#[test]
fn a_4xx_is_final_and_reported_by_name_and_code() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    let fake = Fake::new(vec![status(403)]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    let (l, failed) = r.look(NOW, &fake, Some(KEY));
    assert_eq!(l.gave_up, 1);
    assert_eq!(failed, vec![("g".to_string(), "http_403".to_string())]);
    r.look(NOW + 500, &fake, Some(KEY));
    assert_eq!(fake.seen.borrow().len(), 1, "never retried");
}

/// When the request of the first queued approval stops being worth sending.
fn deadline_of(r: &Rig) -> i64 {
    r.approvals.list(None)[0].created_at_ms as i64 / 1000 + EXPIRY
}

const EXPIRY: i64 = crate::approvals::EXPIRY_SECS as i64;

#[test]
fn an_event_is_never_sent_past_its_deadline() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    let fake = Fake::new(vec![]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    // The sender only looks after the request would have expired: stale.
    let (l, _) = r.look(deadline_of(&r) + 1, &fake, Some(KEY));
    assert_eq!((l.sent, l.expired_unsent), (0, 1));
    assert!(fake.seen.borrow().is_empty());
}

#[test]
fn a_retry_that_would_land_after_the_deadline_is_not_scheduled() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    let fake = Fake::new(vec![status(500)]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    // One second before the deadline the first attempt fails; its 2 s retry
    // would be stale, so it is dropped instead of queued.
    let (l, _) = r.look(deadline_of(&r) - 1, &fake, Some(KEY));
    assert_eq!((l.sent, l.retried, l.expired_unsent), (0, 0, 1));
    assert!(r.queue.is_empty());
}

#[test]
fn one_destination_failing_does_not_hold_back_another() {
    let mut r = rig(dests(&[
        ("a", &[Event::ApprovalRequested]),
        ("b", &[Event::ApprovalRequested]),
    ]));
    // `a` is served first and fails; `b` still gets its event in the same look.
    let fake = Fake::new(vec![status(500), status(200)]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    let (l, _) = r.look(NOW, &fake, Some(KEY));
    assert_eq!((l.sent, l.retried), (1, 1));
}

#[test]
fn events_for_a_destination_go_in_order_one_at_a_time() {
    let mut r = rig(dests(&[("g", BOTH)]));
    let fake = Fake::new(vec![]);
    let a = r
        .approvals
        .queue_entry("s1", "write_file", "first", 1, "/x".into());
    let b = r
        .approvals
        .queue_entry("s1", "edit_file", "second", 2, "/x".into());
    r.look(NOW, &fake, Some(KEY));
    let seen = fake.seen.borrow();
    let ids: Vec<String> = seen
        .iter()
        .map(|q| {
            serde_json::from_slice::<serde_json::Value>(&q.body).unwrap()["data"]["approval_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(ids, [a, b], "oldest first");
}

#[test]
fn a_missing_key_sends_nothing_and_retries() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    let fake = Fake::new(vec![]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    let (l, _) = r.look(NOW, &fake, None);
    assert_eq!((l.sent, l.retried), (0, 1));
    assert!(fake.seen.borrow().is_empty(), "nothing went out unsigned");
}

#[test]
fn a_destination_that_is_gone_or_no_longer_lists_the_event_is_not_sent_to() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x".into());
    let fake = Fake::new(vec![]);
    r.dests = dests(&[("g", &[Event::CronRunFinished])]);
    let (l, _) = r.look(NOW, &fake, Some(KEY));
    assert_eq!((l.sent, l.retried, l.gave_up), (0, 0, 0));
    assert!(fake.seen.borrow().is_empty());
}

#[test]
fn the_expiry_timer_runs_every_30_seconds_and_not_between() {
    let mut r = rig(dests(&[("g", BOTH)]));
    let fake = Fake::new(vec![]);
    r.look(NOW, &fake, Some(KEY));
    assert_eq!(r.expired.load(Ordering::SeqCst), 1, "the first look");
    r.look(NOW + 10, &fake, Some(KEY));
    r.look(NOW + 29, &fake, Some(KEY));
    assert_eq!(r.expired.load(Ordering::SeqCst), 1);
    r.look(NOW + 30, &fake, Some(KEY));
    assert_eq!(r.expired.load(Ordering::SeqCst), 2);
}

#[test]
fn the_timer_runs_the_real_expire_due_and_announces_nothing_before_the_deadline() {
    // Aging an entry 900 s is covered where the queue lives (approvals.rs: an
    // idle expiry is announced once with the full entry); here the timer is the
    // real `expire_due`, and an unexpired entry produces nothing extra.
    let approvals = Arc::new(ApprovalQueue::default());
    let d = dests(&[("g", BOTH)]);
    let queue = VolatileQueue::new();
    approvals.set_notifier(Some(notifier(
        Arc::clone(&d),
        Arc::clone(&queue),
        Arc::new(|| NOW),
        Arc::new(tole_core::secrets::process_secrets),
    )));
    let id = approvals.queue_entry("s1", "write_file", "d", 7, "/x".into());
    // Drain the request first.
    let mut sender = ApprovalSender::new(Arc::clone(&queue), {
        let a = Arc::clone(&approvals);
        Box::new(move || {
            a.expire_due();
        })
    });
    let fake = Fake::new(vec![]);
    sender.look(&d, NOW, &fake, &FixedKey(Some(KEY)), &mut |_, _| {});
    assert_eq!(fake.seen.borrow().len(), 1, "the request");
    // Nothing has expired, so the timer announces nothing.
    sender.look(&d, NOW + 30, &fake, &FixedKey(Some(KEY)), &mut |_, _| {});
    assert_eq!(fake.seen.borrow().len(), 1);
    let _ = id;
}

#[test]
fn a_slow_receiver_does_not_delay_an_approval() {
    // The sender is inside a slow send; an approval queued on another thread,
    // and its decision, must complete at once.
    struct Slow {
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl Poster for Slow {
        fn post(&self, _: &Request<'_>) -> Result<Reply, PostError> {
            // Only the first send is slow; the ones after it (the sender drains
            // what was queued meanwhile) answer at once.
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
                let _ = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5));
            }
            status(200)
        }
    }
    let d = dests(&[("g", BOTH)]);
    let approvals = Arc::new(ApprovalQueue::default());
    let queue = VolatileQueue::new();
    approvals.set_notifier(Some(notifier(
        Arc::clone(&d),
        Arc::clone(&queue),
        Arc::new(|| NOW),
        Arc::new(tole_core::secrets::process_secrets),
    )));
    approvals.queue_entry("s1", "write_file", "first", 1, "/x".into());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let slow = Slow {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    };
    let mut sender = ApprovalSender::new(Arc::clone(&queue), Box::new(|| {}));
    let d2 = Arc::clone(&d);
    let t = std::thread::spawn(move || {
        sender.look(&d2, NOW, &slow, &FixedKey(Some(KEY)), &mut |_, _| {});
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let started = std::time::Instant::now();
    let id = approvals.queue_entry("s2", "edit_file", "second", 2, "/x".into());
    approvals.decide(&id, true);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "an approval waited for the receiver: {:?}",
        started.elapsed()
    );
    let _ = release_tx.send(());
    t.join().unwrap();
}

#[test]
fn nothing_secret_is_reported_or_sent_in_the_clear() {
    let mut r = rig(dests(&[("g", &[Event::ApprovalRequested])]));
    let fake = Fake::new(vec![status(500); 5]);
    r.approvals
        .queue_entry("s1", "write_file", "d", 7, "/x/secret-path.jsonl".into());
    let mut all_failed = Vec::new();
    // The attempts fall at 0, +2, +12, +42 and +162 seconds.
    for at in [0, 2, 12, 42, 162] {
        let (_, f) = r.look(NOW + at, &fake, Some(KEY));
        all_failed.extend(f);
    }
    assert_eq!(fake.seen.borrow().len(), 5);
    let shown = format!("{all_failed:?}");
    assert!(
        !shown.contains("token=abc") && !shown.contains("hooks.example.com"),
        "{shown}"
    );
    let key = std::str::from_utf8(KEY).unwrap();
    for q in fake.seen.borrow().iter() {
        assert!(!String::from_utf8_lossy(&q.body).contains(key));
        assert!(!String::from_utf8_lossy(&q.body).contains("secret-path"));
        for (_, v) in &q.headers {
            assert!(!v.contains(key));
        }
    }
}

#[test]
fn spawn_leaves_the_approval_queue_alone_unless_a_destination_lists_an_approval_event() {
    let approvals = Arc::new(ApprovalQueue::default());
    let told = Arc::new(AtomicUsize::new(0));
    let t = Arc::clone(&told);
    approvals.set_notifier(Some(Arc::new(move |_| {
        t.fetch_add(1, Ordering::SeqCst);
    })));
    // No destination lists an approval event: this must not replace the
    // notifier, start a thread or change anything.
    let spawned = spawn(
        dests(&[("cron", &[Event::CronRunFinished])]),
        Arc::clone(&approvals),
        Arc::default(),
    );
    assert!(spawned.threads.is_empty(), "no thread to wait for");
    approvals.queue_entry("s1", "write_file", "d", 7, "/x".into());
    assert_eq!(
        told.load(Ordering::SeqCst),
        1,
        "the existing notifier is untouched"
    );
}

#[test]
fn any_lists_says_whether_a_destination_wants_approval_events() {
    let none = dests(&[("cron", &[Event::CronRunFinished])]);
    assert!(!none.any_lists(&[Event::ApprovalRequested, Event::ApprovalResolved]));
    assert!(!Destinations::default().any_lists(&[Event::ApprovalRequested]));
    let one = dests(&[("g", &[Event::ApprovalResolved])]);
    assert!(one.any_lists(&[Event::ApprovalRequested, Event::ApprovalResolved]));
    assert!(!one.any_lists(&[Event::ApprovalRequested]));
}

// ---- turn_finished (#962) ----

fn turn_rig(dests: Arc<Destinations>) -> (Rig, crate::webhook_out::turn::TurnNotifier) {
    let r = rig(Arc::clone(&dests));
    let n = crate::webhook_out::volatile::turn_notifier(
        dests,
        Arc::clone(&r.queue),
        Arc::new(|| NOW),
        Arc::new(tole_core::secrets::process_secrets),
    );
    (r, n)
}

fn done(session: &str, seq: u64) -> crate::webhook_out::turn::TurnSignal {
    use crate::webhook_out::turn::*;
    TurnSignal {
        session_id: session.into(),
        end: TurnEnd {
            status: TurnStatus::Completed,
            code: None,
            last_seq: seq,
            settle: 1,
        },
        approval_id: None,
    }
}

#[test]
fn a_turn_event_is_sent_signed_with_its_own_event_header() {
    let (mut r, n) = turn_rig(dests(&[("g", &[Event::TurnFinished])]));
    n(done("s-1", 14));
    let fake = Fake::new(vec![]);
    let (l, failed) = r.look(NOW, &fake, Some(KEY));
    assert_eq!((l.sent, l.retried, l.gave_up), (1, 0, 0));
    assert!(failed.is_empty());
    let seen = fake.seen.borrow();
    let q = &seen[0];
    assert_eq!(header(&q.headers, "X-Tole-Event"), "turn_finished");
    let id = header(&q.headers, "X-Tole-Delivery");
    let body: serde_json::Value = serde_json::from_slice(&q.body).unwrap();
    assert_eq!(body["id"], id);
    assert_eq!(
        header(&q.headers, "X-Tole-Signature"),
        tole_core::webhook::sign(
            KEY,
            header(&q.headers, "X-Tole-Timestamp"),
            id,
            Some("turn_finished"),
            &q.body
        )
    );
}

#[test]
fn a_turn_event_keeps_retrying_every_five_minutes_until_its_horizon() {
    let (mut r, n) = turn_rig(dests(&[("g", &[Event::TurnFinished])]));
    n(done("s-1", 14));
    let fake = Fake::new(vec![status(500); 20]);
    let horizon = crate::webhook_out::turn::RETRY_HORIZON_SECS;
    let mut at = NOW;
    let mut times = vec![at];
    let mut lost = vec![];
    // 2, 10, 30, 120, then every 300 s.
    r.look(at, &fake, Some(KEY));
    for wait in [2, 10, 30, 120, 300, 300, 300, 300] {
        at += wait;
        if at >= NOW + horizon {
            break;
        }
        let (l, failed) = r.look(at, &fake, Some(KEY));
        assert_eq!(
            l.retried + l.gave_up + l.expired_unsent,
            1,
            "at +{}",
            at - NOW
        );
        times.push(at);
        lost.extend(failed);
    }
    // The 0, 2, 12, 42, 162, 462, 762 schedule; the next would be 1062 > 900.
    assert_eq!(
        times.iter().map(|t| t - NOW).collect::<Vec<_>>(),
        [0, 2, 12, 42, 162, 462, 762]
    );
    assert_eq!(fake.seen.borrow().len(), 7);
    // The last attempt finds no time left: it is reported, once, with the
    // destination NAME and the last closed code, and nothing stays queued.
    assert_eq!(lost, vec![("g".to_string(), "http_500".to_string())]);
    assert!(r.queue.is_empty());
    // Every attempt carries the same id and body.
    let seen = fake.seen.borrow();
    assert!(seen.iter().all(|s| s.body == seen[0].body));
}

#[test]
fn a_turn_event_survives_a_receiver_that_comes_back() {
    let (mut r, n) = turn_rig(dests(&[("g", &[Event::TurnFinished])]));
    n(done("s-1", 14));
    let fake = Fake::new(vec![status(500), status(200)]);
    r.look(NOW, &fake, Some(KEY));
    let (l, _) = r.look(NOW + 2, &fake, Some(KEY));
    assert_eq!(l.sent, 1);
    assert!(r.queue.is_empty());
}

#[test]
fn a_400_is_final_for_a_turn_event_too() {
    let (mut r, n) = turn_rig(dests(&[("g", &[Event::TurnFinished])]));
    n(done("s-1", 14));
    let fake = Fake::new(vec![status(400)]);
    let (l, failed) = r.look(NOW, &fake, Some(KEY));
    assert_eq!(l.gave_up, 1);
    assert_eq!(failed, vec![("g".to_string(), "http_400".to_string())]);
    r.look(NOW + 500, &fake, Some(KEY));
    assert_eq!(fake.seen.borrow().len(), 1);
}

#[test]
fn an_approval_only_destination_is_never_sent_a_turn_event() {
    let (mut r, n) = turn_rig(dests(&[("g", BOTH)]));
    n(done("s-1", 14));
    assert!(r.queue.is_empty());
    let fake = Fake::new(vec![]);
    let (l, _) = r.look(NOW, &fake, Some(KEY));
    assert_eq!(l.sent, 0);
}

#[test]
fn the_sender_starts_for_a_turn_event_alone() {
    let one = dests(&[("g", &[Event::TurnFinished])]);
    assert!(one.any_lists(&[Event::TurnFinished]));
    let none = dests(&[("g", &[Event::CronRunFinished])]);
    assert!(!none.any_lists(&[Event::TurnFinished]));
    let spawned = spawn(none, Arc::new(ApprovalQueue::default()), Arc::default());
    assert!(
        spawned.turns.is_none(),
        "no destination, no notifier, no thread"
    );
}

// ---- a hung destination (#980) ----

/// A retry that comes back to a full queue is given up and reported, not lost
/// in silence.
#[test]
fn a_retry_refused_by_a_full_queue_is_given_up_and_reported() {
    struct FillsThenFails(Arc<VolatileQueue>);
    impl Poster for FillsThenFails {
        fn post(&self, _: &Request<'_>) -> Result<Reply, PostError> {
            // While the event is in flight a new one takes its slot.
            self.0.push(queued("g", 999)).unwrap();
            status(500)
        }
    }
    let d = dests(&[("g", &[Event::ApprovalRequested])]);
    let queue = VolatileQueue::new();
    for i in 0..crate::webhook_out::volatile::MAX_PER_DESTINATION {
        queue.push(queued("g", i)).unwrap();
    }
    let mut failed = Vec::new();
    let l = drain(
        &queue,
        &|_| true,
        &d,
        &|| NOW,
        &FillsThenFails(Arc::clone(&queue)),
        &FixedKey(Some(KEY)),
        &mut |d, c| failed.push((d.to_string(), c.to_string())),
    );
    assert_eq!((l.sent, l.retried, l.gave_up), (0, 0, 1));
    assert_eq!(failed, [("g".to_string(), "queue_full".to_string())]);
    assert_eq!(
        queue.len("g"),
        crate::webhook_out::volatile::MAX_PER_DESTINATION
    );
}

fn queued(dest: &str, n: usize) -> Queued {
    Queued {
        dest: dest.into(),
        id: format!("evt-{n}"),
        event: Event::ApprovalRequested,
        body: format!("{{\"n\":{n}}}"),
        deadline: NOW + 900,
        attempts: 0,
        next_at: 0,
        session: None,
    }
}

/// A loopback destination at `addr` with `timeout`.
fn loopback(addr: std::net::SocketAddr, timeout: Duration) -> Destination {
    Destination {
        url: format!("http://{addr}/hook"),
        policy: AddrPolicy::new(true, false),
        timeout,
        ..dest(&[Event::ApprovalRequested, Event::TurnFinished])
    }
}

/// Accepts and never answers; tells `accepted` about each connection and keeps
/// it open.
fn hung_listener(accepted: std::sync::mpsc::Sender<()>) -> std::net::SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut open = Vec::new();
        for s in l.incoming().flatten() {
            open.push(s);
            let _ = accepted.send(());
        }
    });
    addr
}

/// Answers every request 200 and reports the body it read.
fn healthy_listener(bodies: std::sync::mpsc::Sender<String>) -> std::net::SocketAddr {
    use std::io::{BufRead, BufReader, Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s);
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let mut s = r.into_inner();
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = bodies.send(String::from_utf8_lossy(&body).into_owned());
        }
    });
    addr
}

#[test]
fn a_hung_destination_delays_neither_another_destination_nor_the_expiry_timer() {
    let (acc_tx, acc_rx) = std::sync::mpsc::channel();
    let (body_tx, body_rx) = std::sync::mpsc::channel();
    let hung = hung_listener(acc_tx);
    let healthy = healthy_listener(body_tx);
    // The hung destination's timeout outlasts every bound asserted below.
    let d = Arc::new(Destinations(
        [
            (
                "a-hung".to_string(),
                loopback(hung, Duration::from_secs(20)),
            ),
            (
                "b-ok".to_string(),
                loopback(healthy, Duration::from_secs(5)),
            ),
        ]
        .into_iter()
        .collect(),
    ));
    let clock = Arc::new(std::sync::atomic::AtomicI64::new(NOW));
    let queue = VolatileQueue::new();
    for n in 0..3 {
        for dest in ["a-hung", "b-ok"] {
            queue.push(queued(dest, n)).unwrap();
        }
    }
    let expired = Arc::new(AtomicUsize::new(0));
    let e = Arc::clone(&expired);
    let sender = ApprovalSender::new(
        Arc::clone(&queue),
        Box::new(move || {
            e.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c = Arc::clone(&clock);
    let key: Vec<u8> = std::iter::repeat_n(b'k', 32).collect();
    struct RuntimeKey(Vec<u8>);
    impl Keys for RuntimeKey {
        fn key(&self, _: &Destination) -> Result<Vec<u8>, &'static str> {
            Ok(self.0.clone())
        }
    }
    let (workers, _threads) = start(
        Arc::clone(&d),
        sender,
        Wiring {
            poster: Arc::new(NetPoster),
            keys: Arc::new(RuntimeKey(key)),
            clock: Arc::new(move || c.load(Ordering::SeqCst)),
            failed: Arc::new(|_, _| {}),
            stop: Arc::clone(&stop),
        },
    );
    assert_eq!(workers, 2, "one worker per destination");

    // The hung destination has a request in flight...
    acc_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the hung destination was never contacted");
    let started = std::time::Instant::now();
    // ...and the healthy one still gets all three events, in order.
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(
            body_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the healthy destination was held back"),
        );
    }
    assert_eq!(got, ["{\"n\":0}", "{\"n\":1}", "{\"n\":2}"]);
    // The timer runs at start; 30 s later (on the test clock) it runs again
    // although the hung send has not timed out. Wait for the first tick before
    // moving the clock, or that tick would land at NOW + 30.
    let wait_for = |n: usize| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while expired.load(Ordering::SeqCst) < n && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        expired.load(Ordering::SeqCst)
    };
    assert_eq!(wait_for(1), 1, "the expiry timer never ran");
    clock.fetch_add(EXPIRE_EVERY_SECS, Ordering::SeqCst);
    assert_eq!(wait_for(2), 2, "the expiry timer stalled");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the bounds above ran past the hung timeout: {:?}",
        started.elapsed()
    );
    assert!(
        acc_rx.try_recv().is_err(),
        "one request in flight per destination"
    );
    assert_eq!(
        queue.len("a-hung"),
        2,
        "the hung destination's other events wait"
    );
    stop.store(true, Ordering::Relaxed);
}
