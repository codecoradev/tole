use super::*;
use crate::webhook_out::outbox::{FoldOptions, CRON_DEADLINE_SECS};
use crate::webhook_out::store::store_test_dir::Dir;
use crate::webhook_out::{Event, KeySource};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use tole_core::ledger::DeliveryStatus;

const NOW: i64 = 1_795_000_000;
const KEY: &[u8] = b"tole-test-key-0123456789abcdef";

/// The id a real cron event of fire `job@1/2.3` has.
fn real_id() -> String {
    crate::webhook_out::event::cron_event_id("job@1/2.3")
}

struct Seen {
    url: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

#[derive(Default)]
struct Fake {
    results: RefCell<Vec<Result<Reply, PostError>>>,
    seen: RefCell<Vec<Seen>>,
}

impl Fake {
    fn will(&self, r: Result<Reply, PostError>) {
        self.results.borrow_mut().push(r);
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
            panic!("an unexpected request");
        }
        q.remove(0)
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

fn dests(names: &[&str]) -> Destinations {
    Destinations(
        names
            .iter()
            .map(|n| (n.to_string(), dest(&[Event::CronRunFinished])))
            .collect(),
    )
}

fn body(dest: &str, id: &str, at: i64) -> String {
    format!(
        r#"{{"v":1,"id":"{id}","type":"cron_run_finished","destination":"{dest}","at":{at},"data":{{"fire_id":"job@1/2.3"}}}}"#
    )
}

struct Rig {
    dir: Dir,
    fake: Fake,
    clock: Cell<i64>,
    settled: RefCell<Vec<Settled>>,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        Rig {
            dir: Dir::new(tag),
            fake: Fake::default(),
            clock: Cell::new(NOW),
            settled: RefCell::new(Vec::new()),
        }
    }

    fn outbox(&self) -> Outbox {
        Outbox::new(&self.dir.0)
    }

    fn enqueue(&self, dest: &str, id: &str) {
        self.outbox()
            .enqueue(dest, id, &body(dest, id, NOW), NOW)
            .unwrap();
    }

    fn pass(&self, d: &Destinations, key: Option<&'static [u8]>) -> PassStats {
        drain_pass(
            &self.outbox(),
            d,
            &self.fake,
            &FixedKey(key),
            &|| self.clock.get(),
            &mut |s| self.settled.borrow_mut().push(s),
        )
        .unwrap()
    }

    fn fold(&self) -> crate::webhook_out::outbox::Fold {
        self.outbox()
            .fold(FoldOptions {
                now: self.clock.get(),
                max_lease_ahead: 1000,
            })
            .unwrap()
    }
}

fn header<'a>(h: &'a [(String, String)], k: &str) -> &'a str {
    h.iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
        .unwrap()
}

#[test]
fn a_2xx_is_one_signed_send_and_a_sent_row() {
    let r = Rig::new("ok");
    let id = real_id();
    r.enqueue("g", &id);
    r.fake.will(status(204));
    let s = r.pass(&dests(&["g"]), Some(KEY));
    assert_eq!((s.sent, s.retried, s.dead), (1, 0, 0));
    let seen = r.fake.seen.borrow();
    assert_eq!(seen.len(), 1);
    let q = &seen[0];
    assert_eq!(q.url, "https://hooks.example.com/tole?token=abc");
    assert_eq!(q.body, body("g", &id, NOW).as_bytes());
    // The signature is the native v2 one over exactly these bytes.
    let ts = header(&q.headers, "X-Tole-Timestamp");
    assert_eq!(ts, NOW.to_string());
    assert_eq!(
        header(&q.headers, "X-Tole-Signature"),
        tole_core::webhook::sign(KEY, ts, &id, Some("cron_run_finished"), &q.body)
    );
    assert_eq!(header(&q.headers, "X-Tole-Delivery"), id);
    assert_eq!(header(&q.headers, "X-Tole-Event"), "cron_run_finished");
    let st = r.settled.borrow();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].status, DeliveryStatus::Sent);
    assert_eq!(st[0].fire_id.as_deref(), Some("job@1/2.3"));
    assert_eq!(st[0].code, None);
    assert_eq!(r.fold().pending_count("g"), 0);
}

#[test]
fn a_500_is_retried_with_the_same_id_and_body_and_a_new_timestamp() {
    let r = Rig::new("retry");
    r.enqueue("g", "cron:a:1");
    let d = dests(&["g"]);
    r.fake.will(status(500));
    let s = r.pass(&d, Some(KEY));
    assert_eq!((s.sent, s.retried), (0, 1));
    assert!(r.settled.borrow().is_empty(), "not terminal yet");
    let e = r.fold().get("g", "cron:a:1").unwrap().clone();
    assert_eq!((e.attempts, e.next_at), (1, NOW + 2));

    // Not yet due.
    r.clock.set(NOW + 1);
    assert_eq!(r.pass(&d, Some(KEY)), PassStats::default());

    r.clock.set(NOW + 2);
    r.fake.will(status(200));
    let s = r.pass(&d, Some(KEY));
    assert_eq!(s.sent, 1);
    let seen = r.fake.seen.borrow();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, seen[1].body, "byte-identical body");
    assert_eq!(
        header(&seen[0].headers, "X-Tole-Delivery"),
        header(&seen[1].headers, "X-Tole-Delivery")
    );
    assert_ne!(
        header(&seen[0].headers, "X-Tole-Timestamp"),
        header(&seen[1].headers, "X-Tole-Timestamp")
    );
    assert_ne!(
        header(&seen[0].headers, "X-Tole-Signature"),
        header(&seen[1].headers, "X-Tole-Signature")
    );
    assert_eq!(r.settled.borrow().len(), 1);
}

#[test]
fn another_4xx_is_final_and_a_failed_row() {
    let r = Rig::new("400");
    r.enqueue("g", "a");
    r.fake.will(status(400));
    let s = r.pass(&dests(&["g"]), Some(KEY));
    assert_eq!((s.sent, s.dead), (0, 1));
    let st = r.settled.borrow();
    assert_eq!(st[0].status, DeliveryStatus::Failed);
    assert_eq!(st[0].code.as_deref(), Some("http_400"));
    assert_eq!(r.fold().pending_count("g"), 0);
    // Never retried.
    drop(st);
    r.clock.set(NOW + 10_000);
    assert_eq!(r.pass(&dests(&["g"]), Some(KEY)), PassStats::default());
    assert_eq!(r.fake.seen.borrow().len(), 1);
}

#[test]
fn a_policy_refusal_and_a_redirect_are_final() {
    for (res, code) in [
        (Err(PostError::RefusedAddress), "refused_address"),
        (Err(PostError::Redirect(302)), "redirect_refused"),
        (Err(PostError::SchemeRefused), "bad_destination"),
    ] {
        let r = Rig::new("final");
        r.enqueue("g", "a");
        r.fake.will(res);
        r.pass(&dests(&["g"]), Some(KEY));
        assert_eq!(r.settled.borrow()[0].code.as_deref(), Some(code));
        assert_eq!(r.fold().pending_count("g"), 0);
    }
}

#[test]
fn a_transport_failure_is_retried_not_settled() {
    for res in [
        Err(PostError::Dns),
        Err(PostError::Timeout),
        Err(PostError::Transport("connect".into())),
        status(503),
        status(429),
        status(408),
    ] {
        let r = Rig::new("transient");
        r.enqueue("g", "a");
        r.fake.will(res);
        let s = r.pass(&dests(&["g"]), Some(KEY));
        assert_eq!((s.retried, s.dead), (1, 0));
        assert!(r.settled.borrow().is_empty());
        assert_eq!(r.fold().pending_count("g"), 1);
    }
}

#[test]
fn a_missing_key_sends_nothing_and_retries() {
    let r = Rig::new("nokey");
    r.enqueue("g", "a");
    let s = r.pass(&dests(&["g"]), None);
    assert_eq!((s.retried, s.sent), (1, 0));
    assert!(r.fake.seen.borrow().is_empty(), "nothing went out");
    assert_eq!(r.fold().get("g", "a").unwrap().attempts, 1);
}

#[test]
fn only_one_event_per_destination_is_sent_per_pass() {
    let r = Rig::new("one");
    r.enqueue("g", "a");
    r.enqueue("g", "b");
    r.enqueue("h", "c");
    let d = dests(&["g", "h"]);
    r.fake.will(status(200));
    r.fake.will(status(200));
    assert_eq!(r.pass(&d, Some(KEY)).sent, 2);
    r.fake.will(status(200));
    assert_eq!(r.pass(&d, Some(KEY)).sent, 1);
    assert_eq!(r.fold().pending_count("g"), 0);
}

#[test]
fn a_leased_event_is_not_sent_by_a_second_drainer() {
    let r = Rig::new("lease");
    r.enqueue("g", "a");
    // Another process leased it and has not answered yet.
    r.outbox().lease_due(NOW, &|_| Some(15), 10).unwrap();
    // This pass must not send: the poster has nothing queued and would panic.
    assert_eq!(r.pass(&dests(&["g"]), Some(KEY)), PassStats::default());
    // The lease ran out (a crash between lease and result): sent again.
    r.clock.set(NOW + 16);
    r.fake.will(status(200));
    assert_eq!(r.pass(&dests(&["g"]), Some(KEY)).sent, 1);
}

#[test]
fn a_pass_does_nothing_while_the_lock_is_held() {
    let r = Rig::new("lock");
    r.enqueue("g", "a");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(r.dir.0.join("outbox.lock"))
        .unwrap();
    held.lock().unwrap();
    let s = r.pass(&dests(&["g"]), Some(KEY));
    assert!(s.busy);
    assert!(r.fake.seen.borrow().is_empty());
}

#[test]
fn an_event_past_its_deadline_is_never_sent() {
    let r = Rig::new("expired");
    r.enqueue("g", &real_id());
    r.clock.set(NOW + CRON_DEADLINE_SECS);
    let s = r.pass(&dests(&["g"]), Some(KEY));
    assert_eq!(s.dead, 1);
    assert!(r.fake.seen.borrow().is_empty());
    let st = r.settled.borrow();
    assert_eq!(st[0].code.as_deref(), Some("expired"));
    assert_eq!(st[0].fire_id.as_deref(), Some("job@1/2.3"));
}

#[test]
fn a_forged_line_is_dead_and_never_signed() {
    // Each of these is a line another process of the account could append.
    let cases: Vec<String> = vec![
        // The body names another destination than the line.
        body("other", "a", NOW),
        // An event type the destination does not list.
        body("g", "a", NOW).replace("cron_run_finished", "approval_requested"),
        // A body with another id than the line.
        body("g", "different", NOW),
        // From the future.
        body("g", "a", NOW + 100_000),
        // Not JSON.
        "not json".to_string(),
    ];
    for (n, bad_body) in cases.into_iter().enumerate() {
        let r = Rig::new(&format!("forged{n}"));
        let line = Line::Enqueue {
            dest: "g".into(),
            id: "a".into(),
            at: NOW,
            deadline: NOW + CRON_DEADLINE_SECS,
            body: bad_body,
        };
        r.outbox().append(&[line]).unwrap();
        let s = r.pass(&dests(&["g"]), Some(KEY));
        assert_eq!((s.dead, s.sent), (1, 0), "case {n}");
        assert!(r.fake.seen.borrow().is_empty(), "case {n} was signed");
        assert_eq!(r.settled.borrow()[0].code.as_deref(), Some("bad_line"));
    }
}

#[test]
fn an_oversized_body_is_never_sent() {
    let r = Rig::new("big");
    let big = format!(
        r#"{{"v":1,"id":"a","type":"cron_run_finished","destination":"g","at":{NOW},"data":"{}"}}"#,
        "x".repeat(20_000)
    );
    r.outbox()
        .append(&[Line::Enqueue {
            dest: "g".into(),
            id: "a".into(),
            at: NOW,
            deadline: NOW + CRON_DEADLINE_SECS,
            body: big,
        }])
        .unwrap();
    r.pass(&dests(&["g"]), Some(KEY));
    assert!(r.fake.seen.borrow().is_empty());
    assert_eq!(r.settled.borrow()[0].code.as_deref(), Some("bad_line"));
}

#[test]
fn a_destination_that_is_gone_is_not_sent_to() {
    let r = Rig::new("gone");
    r.enqueue("gone", "a");
    let s = r.pass(&dests(&["g"]), Some(KEY));
    assert_eq!(s, PassStats::default());
    assert!(r.fake.seen.borrow().is_empty());
    assert_eq!(r.fold().pending_count("gone"), 1, "kept, not sent");
}

#[test]
fn nothing_secret_reaches_the_outbox_or_a_settled_row() {
    let r = Rig::new("leak");
    r.enqueue("g", "a");
    r.fake.will(status(500));
    r.pass(&dests(&["g"]), Some(KEY));
    r.clock.set(NOW + 2);
    r.fake.will(status(200));
    r.pass(&dests(&["g"]), Some(KEY));
    let file = String::from_utf8(r.outbox().read_all().unwrap()).unwrap();
    let key = std::str::from_utf8(KEY).unwrap();
    assert!(!file.contains(key), "the key is in the outbox");
    assert!(!file.contains("token=abc"), "the URL is in the outbox");
    for s in r.settled.borrow().iter() {
        assert!(!format!("{s:?}").contains(key));
    }
}

#[test]
fn retry_delays_follow_the_design() {
    let d = |n| retry_delay(n, "cron:a:1", None);
    assert_eq!([d(1), d(2), d(3), d(4)], [2, 10, 30, 120]);
    for n in 5..40 {
        let v = d(n);
        assert!((240..=360).contains(&v), "n={n}: {v}");
        assert_eq!(v, d(n), "deterministic");
    }
    let spread: BTreeSet<i64> = (5..60).map(d).collect();
    assert!(spread.len() > 5, "jitter varies: {spread:?}");
    // Another id jitters differently.
    assert!((5..30).any(|n| d(n) != retry_delay(n, "cron:b:2", None)));
    // A hint raises the wait, is capped, and never lowers it.
    assert_eq!(retry_delay(1, "x", Some(60)), 60);
    assert_eq!(retry_delay(1, "x", Some(0)), 2);
    assert_eq!(retry_delay(1, "x", Some(100_000)), MAX_RETRY_AFTER_SECS);
}

#[test]
fn classification_is_a_closed_table() {
    let resp = |s, ra: Option<&str>| {
        Ok(Reply {
            status: s,
            retry_after: ra.map(str::to_string),
        })
    };
    assert_eq!(classify(&resp(200, None)), Verdict::Done);
    assert_eq!(classify(&resp(299, None)), Verdict::Done);
    assert_eq!(classify(&resp(404, None)), Verdict::Dead("http_404".into()));
    assert_eq!(classify(&resp(199, None)), Verdict::Dead("http_199".into()));
    assert_eq!(classify(&resp(301, None)), Verdict::Dead("http_301".into()));
    for s in [408, 429, 500, 502, 599] {
        assert!(
            matches!(classify(&resp(s, None)), Verdict::Retry { .. }),
            "{s}"
        );
    }
    // Retry-After: seconds only, on 429 and 503 only.
    assert_eq!(
        classify(&resp(429, Some("30"))),
        Verdict::Retry {
            code: "http_429".into(),
            retry_after: Some(30)
        }
    );
    assert_eq!(
        classify(&resp(503, Some("99999"))),
        Verdict::Retry {
            code: "http_503".into(),
            retry_after: Some(MAX_RETRY_AFTER_SECS)
        }
    );
    for ra in [
        Some("Wed, 21 Oct 2026 07:28:00 GMT"),
        Some("-5"),
        Some("x"),
        None,
    ] {
        assert_eq!(
            classify(&resp(429, ra)),
            Verdict::Retry {
                code: "http_429".into(),
                retry_after: None
            },
            "{ra:?}"
        );
    }
    assert_eq!(
        classify(&resp(500, Some("30"))),
        Verdict::Retry {
            code: "http_500".into(),
            retry_after: None
        },
        "only 429 and 503 honour it"
    );
}

/// A key resolver that must never be asked.
struct PanicKeys;

impl Keys for PanicKeys {
    fn key(&self, _: &Destination) -> Result<Vec<u8>, &'static str> {
        panic!("the key was read for a line that is not valid");
    }
}

#[test]
fn the_key_is_not_read_for_a_forged_line() {
    for (n, bad) in [
        "not json".to_string(),
        body("other", "a", NOW),
        body("g", "a", NOW).replace(r#""v":1"#, r#""v":2"#),
        body("g", "a", NOW - CRON_DEADLINE_SECS - 1),
    ]
    .into_iter()
    .enumerate()
    {
        let r = Rig::new(&format!("nokeyread{n}"));
        r.outbox()
            .append(&[Line::Enqueue {
                dest: "g".into(),
                id: "a".into(),
                at: NOW,
                deadline: NOW + CRON_DEADLINE_SECS,
                body: bad,
            }])
            .unwrap();
        let s = drain_pass(
            &r.outbox(),
            &dests(&["g"]),
            &r.fake,
            &PanicKeys,
            &|| r.clock.get(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(s.dead, 1, "case {n}");
    }
}

#[test]
fn a_forged_attempt_count_cannot_overflow_or_reset_the_backoff() {
    let r = Rig::new("n-max");
    r.enqueue("g", "a");
    r.outbox()
        .append(&[Line::Attempt {
            dest: "g".into(),
            id: "a".into(),
            n: u32::MAX,
            at: NOW,
            outcome: "x".into(),
            next_at: 0,
        }])
        .unwrap();
    r.fake.will(status(500));
    // Does not panic; the count stays at the maximum instead of wrapping.
    let s = r.pass(&dests(&["g"]), Some(KEY));
    assert_eq!(s.retried, 1);
    assert_eq!(r.fold().get("g", "a").unwrap().attempts, u32::MAX);
}

#[test]
fn a_result_and_its_terminal_line_are_adjacent() {
    // One write: no other drainer can see the event unleased and due between
    // the attempt and the done line.
    let r = Rig::new("adjacent");
    r.enqueue("g", "a");
    r.fake.will(status(200));
    r.pass(&dests(&["g"]), Some(KEY));
    let text = String::from_utf8(r.outbox().read_all().unwrap()).unwrap();
    let kinds: Vec<&str> = text
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            match v["k"].as_str().unwrap() {
                "enqueue" => "enqueue",
                "lease" => "lease",
                "attempt" => "attempt",
                "done" => "done",
                _ => "other",
            }
        })
        .collect();
    assert_eq!(kinds, ["enqueue", "lease", "attempt", "done"]);
}

/// A poster that plays the second drainer: while the send is in flight it
/// tries to lease the same event.
struct RacingPoster<'a> {
    outbox: Outbox,
    leased_meanwhile: &'a Cell<usize>,
}

impl Poster for RacingPoster<'_> {
    fn post(&self, _: &Request<'_>) -> Result<Reply, PostError> {
        let l = self.outbox.lease_due(NOW, &|_| Some(15), 10).unwrap();
        self.leased_meanwhile.set(l.leased.len());
        status(200)
    }
}

#[test]
fn a_second_drainer_cannot_lease_an_event_while_it_is_being_sent() {
    let r = Rig::new("race");
    r.enqueue("g", "a");
    let seen = Cell::new(99);
    let p = RacingPoster {
        outbox: r.outbox(),
        leased_meanwhile: &seen,
    };
    let s = drain_pass(
        &r.outbox(),
        &dests(&["g"]),
        &p,
        &FixedKey(Some(KEY)),
        &|| NOW,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(s.sent, 1);
    assert_eq!(seen.get(), 0, "the lease holds during the send");
    // And once done, it is not due again.
    assert!(r
        .outbox()
        .lease_due(NOW + 1, &|_| Some(15), 10)
        .unwrap()
        .leased
        .is_empty());
}

#[test]
fn a_send_that_could_outlive_its_lease_is_not_started() {
    let r = Rig::new("lease-margin");
    r.enqueue("g", "a");
    // Leased at NOW with 15 s (10 s timeout + 5); by NOW+6 a 10 s send would
    // end past the lease.
    let t = Cell::new(NOW);
    let calls = Cell::new(0);
    let clock = || {
        calls.set(calls.get() + 1);
        // The first read leases; the next reads are "later".
        if calls.get() == 1 {
            NOW
        } else {
            t.get() + 6
        }
    };
    let s = drain_pass(
        &r.outbox(),
        &dests(&["g"]),
        &r.fake,
        &FixedKey(Some(KEY)),
        &clock,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(s, PassStats::default());
    assert!(r.fake.seen.borrow().is_empty());
}

#[test]
fn debug_of_a_reply_shows_no_receiver_text() {
    let reply = Reply {
        status: 429,
        retry_after: Some("RECEIVER-TEXT".into()),
    };
    assert!(!format!("{reply:?}").contains("RECEIVER-TEXT"));
}
