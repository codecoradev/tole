use super::*;
use std::collections::BTreeSet;
use std::time::Duration;
use tole_core::web::AddrPolicy;

const NOW: i64 = 1_795_000_000;

fn opts(now: i64) -> FoldOptions {
    FoldOptions {
        now,
        max_lease_ahead: 10 + 5 + SKEW_SECS,
    }
}

fn dest(events: &[Event]) -> Destination {
    Destination {
        url: "https://example.com/hook".into(),
        events: events.iter().copied().collect::<BTreeSet<_>>(),
        policy: AddrPolicy::default(),
        allow_agent_jobs: false,
        timeout: Duration::from_secs(10),
    }
}

fn body(dest: &str, id: &str, ty: &str, at: i64) -> String {
    format!(r#"{{"v":1,"id":"{id}","type":"{ty}","destination":"{dest}","at":{at},"data":{{}}}}"#)
}

fn file(lines: &[Line]) -> Vec<u8> {
    lines.iter().flat_map(|l| encode(l).unwrap()).collect()
}

fn folded(bytes: &[u8], now: i64) -> Fold {
    let mut f = Fold::default();
    let n = f.feed(bytes, 0, opts(now));
    assert_eq!(n, bytes.len(), "every byte was complete lines");
    f
}

fn enq(d: &str, id: &str, at: i64) -> Line {
    Line::enqueue(d, id, &body(d, id, "cron_run_finished", at), at)
}

#[test]
fn a_line_round_trips_as_one_json_line() {
    for l in [
        enq("g", "cron:a:1", NOW),
        Line::Lease {
            dest: "g".into(),
            id: "x".into(),
            at: 1,
            lease_until: 2,
        },
        Line::Attempt {
            dest: "g".into(),
            id: "x".into(),
            n: 3,
            at: 1,
            outcome: "http_500".into(),
            next_at: 9,
        },
        Line::Done {
            dest: "g".into(),
            id: "x".into(),
            at: 1,
        },
        Line::Dead {
            dest: "g".into(),
            id: "x".into(),
            at: 1,
            code: EXPIRED.into(),
        },
    ] {
        let b = encode(&l).unwrap();
        assert_eq!(b.iter().filter(|&&c| c == b'\n').count(), 1);
        assert_eq!(*b.last().unwrap(), b'\n');
        let back: Line = serde_json::from_slice(&b[..b.len() - 1]).unwrap();
        assert_eq!(back, l);
    }
}

#[test]
fn a_stored_body_is_byte_identical() {
    let text = body("g", "cron:a:1", "cron_run_finished", NOW);
    let l = Line::enqueue("g", "cron:a:1", &text, NOW);
    let b = encode(&l).unwrap();
    let Line::Enqueue { body, .. } = serde_json::from_slice(&b[..b.len() - 1]).unwrap() else {
        panic!()
    };
    assert_eq!(body, text);
}

#[test]
fn a_line_over_the_cap_is_not_encoded() {
    let l = Line::enqueue("g", "i", &"x".repeat(MAX_LINE), NOW);
    assert_eq!(encode(&l), Err(EncodeError::TooLong));
}

#[test]
fn a_writer_ends_a_torn_tail_before_appending() {
    assert_eq!(separator(None), b"");
    assert_eq!(separator(Some(b'\n')), b"");
    assert_eq!(separator(Some(b'}')), b"\n");
}

#[test]
fn the_fold_follows_one_event_to_done() {
    let mut bytes = file(&[enq("g", "a", NOW)]);
    let f = folded(&bytes, NOW);
    assert!(f.has("g", "a"));
    assert_eq!(f.pending_count("g"), 1);
    assert_eq!(f.get("g", "a").unwrap().offset, 0);

    bytes.extend(file(&[
        Line::Attempt {
            dest: "g".into(),
            id: "a".into(),
            n: 1,
            at: NOW,
            outcome: "http_500".into(),
            next_at: NOW + 2,
        },
        Line::Done {
            dest: "g".into(),
            id: "a".into(),
            at: NOW + 3,
        },
    ]));
    let f = folded(&bytes, NOW + 3);
    let e = f.get("g", "a").unwrap();
    assert_eq!(e.attempts, 1);
    assert_eq!(e.terminal, Some(Terminal::Done));
    assert_eq!(f.pending_count("g"), 0);
}

#[test]
fn lines_are_keyed_by_destination_and_id() {
    let bytes = file(&[
        enq("one", "same-id", NOW),
        enq("two", "same-id", NOW),
        Line::Done {
            dest: "one".into(),
            id: "same-id".into(),
            at: NOW,
        },
    ]);
    let f = folded(&bytes, NOW);
    assert_eq!(f.pending_count("one"), 0);
    assert_eq!(
        f.pending_count("two"),
        1,
        "the other destination is untouched"
    );
}

#[test]
fn a_torn_tail_is_not_consumed_until_it_is_complete() {
    let a = file(&[enq("g", "a", NOW)]);
    let b = file(&[enq("g", "b", NOW)]);
    let mut bytes = a.clone();
    bytes.extend(&b[..b.len() - 5]); // the second line is torn
    let mut f = Fold::default();
    let n = f.feed(&bytes, 0, opts(NOW));
    assert_eq!(n, a.len());
    assert!(f.has("g", "a") && !f.has("g", "b"));
    assert_eq!(f.bad_lines, 0, "a torn tail is not a bad line yet");

    // The writer completes it; resuming from the consumed offset sees it whole.
    let mut rest = bytes[n..].to_vec();
    rest.extend(&b[b.len() - 5..]);
    let n2 = f.feed(&rest, n, opts(NOW));
    assert_eq!(n2, rest.len());
    assert!(f.has("g", "b"));
    assert_eq!(f.get("g", "b").unwrap().offset, a.len());
}

#[test]
fn a_fragment_that_a_later_writer_terminated_is_one_bad_line() {
    let a = file(&[enq("g", "a", NOW)]);
    let torn = &file(&[enq("g", "b", NOW)])[..20];
    let mut bytes = a;
    bytes.extend(torn);
    bytes.extend(separator(Some(*torn.last().unwrap())));
    bytes.extend(file(&[enq("g", "c", NOW)]));
    let f = folded(&bytes, NOW);
    assert_eq!(f.bad_lines, 1);
    assert!(f.has("g", "a") && f.has("g", "c") && !f.has("g", "b"));
}

#[test]
fn garbage_and_overlong_lines_are_skipped_and_counted() {
    let mut bytes = b"not json\n\n{\"k\":\"nope\"}\n".to_vec();
    bytes.extend(vec![b'x'; MAX_LINE + 1]);
    bytes.push(b'\n');
    bytes.extend(file(&[enq("g", "a", NOW)]));
    let f = folded(&bytes, NOW);
    assert_eq!(f.bad_lines, 3, "the empty line is not counted");
    assert!(f.has("g", "a"));
}

#[test]
fn a_line_with_no_enqueue_is_ignored() {
    let bytes = file(&[
        Line::Done {
            dest: "g".into(),
            id: "ghost".into(),
            at: NOW,
        },
        Line::Lease {
            dest: "g".into(),
            id: "ghost".into(),
            at: NOW,
            lease_until: NOW + 5,
        },
        Line::Attempt {
            dest: "g".into(),
            id: "ghost".into(),
            n: 1,
            at: NOW,
            outcome: "x".into(),
            next_at: 0,
        },
        Line::Dead {
            dest: "g".into(),
            id: "ghost".into(),
            at: NOW,
            code: "x".into(),
        },
    ]);
    let f = folded(&bytes, NOW);
    assert_eq!(f.ignored, 4);
    assert!(!f.has("g", "ghost"));
}

#[test]
fn the_first_enqueue_wins_and_a_terminal_line_is_final() {
    let bytes = file(&[
        enq("g", "a", NOW),
        Line::enqueue("g", "a", "a different body", NOW + 99),
        Line::Done {
            dest: "g".into(),
            id: "a".into(),
            at: NOW,
        },
        Line::Dead {
            dest: "g".into(),
            id: "a".into(),
            at: NOW,
            code: "x".into(),
        },
    ]);
    let f = folded(&bytes, NOW);
    assert_eq!(f.get("g", "a").unwrap().at, NOW);
    assert_eq!(f.get("g", "a").unwrap().terminal, Some(Terminal::Done));
    assert_eq!(f.ignored, 2);
}

#[test]
fn a_far_future_lease_is_ignored() {
    let lease = |until| Line::Lease {
        dest: "g".into(),
        id: "a".into(),
        at: NOW,
        lease_until: until,
    };
    let f = folded(&file(&[enq("g", "a", NOW), lease(NOW + 15)]), NOW);
    assert_eq!(f.get("g", "a").unwrap().lease_until, NOW + 15);
    let f = folded(&file(&[enq("g", "a", NOW), lease(NOW + 10_000_000)]), NOW);
    assert_eq!(f.get("g", "a").unwrap().lease_until, 0);
    assert_eq!(f.ignored, 1);
    assert_eq!(f.due(NOW).len(), 1, "the forged lease does not park it");
}

#[test]
fn due_gives_one_event_per_destination_oldest_first() {
    let bytes = file(&[
        enq("one", "a", NOW),
        enq("one", "b", NOW),
        enq("two", "c", NOW),
    ]);
    let f = folded(&bytes, NOW);
    let due = f.due(NOW);
    assert_eq!(due.len(), 2);
    assert_eq!((due[0].0, due[0].1), ("one", "a"));
    assert_eq!((due[1].0, due[1].1), ("two", "c"));
}

#[test]
fn due_skips_leased_waiting_finished_and_expired_events() {
    let bytes = file(&[
        enq("g", "leased", NOW),
        Line::Lease {
            dest: "g".into(),
            id: "leased".into(),
            at: NOW,
            lease_until: NOW + 15,
        },
    ]);
    let f = folded(&bytes, NOW);
    assert!(f.due(NOW).is_empty(), "leased");
    assert_eq!(f.due(NOW + 16).len(), 1, "the lease ran out");

    let waiting = file(&[
        enq("g", "w", NOW),
        Line::Attempt {
            dest: "g".into(),
            id: "w".into(),
            n: 1,
            at: NOW,
            outcome: "timeout".into(),
            next_at: NOW + 30,
        },
    ]);
    let f = folded(&waiting, NOW);
    assert!(f.due(NOW + 10).is_empty(), "waiting for its retry time");
    assert_eq!(f.due(NOW + 30).len(), 1);

    let f = folded(&file(&[enq("g", "e", NOW)]), NOW);
    let late = NOW + CRON_DEADLINE_SECS;
    assert!(f.due(late).is_empty(), "past its deadline");
    assert_eq!(f.expired(late), vec![("g", "e")]);
    assert!(f.expired(NOW).is_empty());
}

#[test]
fn a_result_ends_the_lease_and_counts_the_attempt() {
    let bytes = file(&[
        enq("g", "a", NOW),
        Line::Lease {
            dest: "g".into(),
            id: "a".into(),
            at: NOW,
            lease_until: NOW + 15,
        },
        Line::Attempt {
            dest: "g".into(),
            id: "a".into(),
            n: 1,
            at: NOW + 1,
            outcome: "http_503".into(),
            next_at: NOW + 3,
        },
    ]);
    let f = folded(&bytes, NOW + 1);
    let e = f.get("g", "a").unwrap();
    assert_eq!((e.attempts, e.lease_until, e.next_at), (1, 0, NOW + 3));
}

#[test]
fn the_queue_is_bounded_per_destination() {
    let mut f = Fold::default();
    let mut bytes = Vec::new();
    for i in 0..MAX_PENDING_PER_DESTINATION {
        bytes.extend(file(&[Line::Enqueue {
            dest: "g".into(),
            id: format!("e{i}"),
            at: NOW,
            deadline: NOW + 1,
            body: String::new(),
        }]));
    }
    f.feed(&bytes, 0, opts(NOW));
    assert!(f.queue_full("g"));
    assert!(!f.queue_full("other"));
}

#[test]
fn a_well_formed_event_validates() {
    let d = dest(&[Event::CronRunFinished]);
    let b = body("g", "cron:a:1", "cron_run_finished", NOW);
    assert_eq!(validate("g", "cron:a:1", &b, Some(&d), NOW), Ok(()));
}

#[test]
fn validation_refuses_what_the_destination_does_not_allow() {
    let d = dest(&[Event::CronRunFinished]);
    let ok = body("g", "i", "cron_run_finished", NOW);
    // A destination that is not (or no longer) configured.
    assert_eq!(validate("g", "i", &ok, None, NOW), Err(BAD_LINE));
    // The body names another destination.
    let other = body("other", "i", "cron_run_finished", NOW);
    assert_eq!(validate("g", "i", &other, Some(&d), NOW), Err(BAD_LINE));
    // A type the destination does not list, or an unknown one.
    let approval = body("g", "i", "approval_requested", NOW);
    assert_eq!(validate("g", "i", &approval, Some(&d), NOW), Err(BAD_LINE));
    let unknown = body("g", "i", "rm_rf", NOW);
    assert_eq!(validate("g", "i", &unknown, Some(&d), NOW), Err(BAD_LINE));
    // The body carries another id than the line.
    assert_eq!(validate("g", "j", &ok, Some(&d), NOW), Err(BAD_LINE));
}

#[test]
fn validation_refuses_a_bad_body_or_id() {
    let d = dest(&[Event::CronRunFinished]);
    assert_eq!(validate("g", "i", "not json", Some(&d), NOW), Err(BAD_LINE));
    assert_eq!(validate("g", "i", "[]", Some(&d), NOW), Err(BAD_LINE));
    let v2 = body("g", "i", "cron_run_finished", NOW).replace(r#""v":1"#, r#""v":2"#);
    assert_eq!(validate("g", "i", &v2, Some(&d), NOW), Err(BAD_LINE));
    // An id outside the delivery charset, even when the body agrees.
    let bad = body("g", "a b", "cron_run_finished", NOW);
    assert_eq!(validate("g", "a b", &bad, Some(&d), NOW), Err(BAD_LINE));
    // A body over the cap.
    let big = format!(
        r#"{{"v":1,"id":"i","type":"cron_run_finished","destination":"g","at":{NOW},"data":"{}"}}"#,
        "x".repeat(MAX_BODY)
    );
    assert_eq!(validate("g", "i", &big, Some(&d), NOW), Err(BAD_LINE));
}

#[test]
fn validation_bounds_the_age_of_an_event() {
    let d = dest(&[Event::CronRunFinished]);
    let at = |t| body("g", "i", "cron_run_finished", t);
    assert_eq!(
        validate("g", "i", &at(NOW + SKEW_SECS), Some(&d), NOW),
        Ok(())
    );
    assert_eq!(
        validate("g", "i", &at(NOW + SKEW_SECS + 1), Some(&d), NOW),
        Err(BAD_LINE),
        "from the future"
    );
    let now = NOW + CRON_DEADLINE_SECS;
    assert_eq!(validate("g", "i", &at(NOW), Some(&d), now), Ok(()));
    assert_eq!(
        validate("g", "i", &at(NOW), Some(&d), now + 1),
        Err(EXPIRED)
    );
}

#[test]
fn a_built_event_passes_validation() {
    // The producer (event.rs) and the sender's check agree.
    use crate::webhook_out::event::{cron_run_finished, CronRun};
    use tole_core::ledger::RunStatus;
    let run = CronRun {
        job_id: "daily",
        job_name: None,
        fire_id: "daily@1/2.3",
        status: RunStatus::Ok,
        scheduled_for: NOW,
        finished_at: NOW,
        output: "hi",
        session_id: None,
    };
    let built = cron_run_finished("g", NOW, &run, &|s| s.to_string()).unwrap();
    let text = String::from_utf8(built.body).unwrap();
    let d = dest(&[Event::CronRunFinished]);
    assert_eq!(validate("g", &built.id, &text, Some(&d), NOW), Ok(()));
    // And it fits on one outbox line.
    assert!(encode(&Line::enqueue("g", &built.id, &text, NOW)).is_ok());
}

#[test]
fn a_crafted_timestamp_cannot_overflow_the_checks() {
    let d = dest(&[Event::CronRunFinished]);
    let at = |t: i64| body("g", "i", "cron_run_finished", t);
    // i64::MIN reads as absurdly old, i64::MAX as from the future: refused,
    // never a panic and never a wrap that passes the age check.
    assert_eq!(
        validate("g", "i", &at(i64::MIN), Some(&d), NOW),
        Err(EXPIRED)
    );
    assert_eq!(
        validate("g", "i", &at(i64::MAX), Some(&d), NOW),
        Err(BAD_LINE)
    );
    // The same holds when the caller's own clock is extreme.
    assert_eq!(
        validate("g", "i", &at(i64::MIN), Some(&d), i64::MAX),
        Err(EXPIRED)
    );
    assert_eq!(
        validate("g", "i", &at(i64::MAX), Some(&d), i64::MIN),
        Err(BAD_LINE)
    );
}

#[test]
fn a_crafted_lease_or_deadline_cannot_overflow_the_fold() {
    let bytes = file(&[
        Line::Enqueue {
            dest: "g".into(),
            id: "a".into(),
            at: i64::MIN,
            deadline: i64::MAX,
            body: String::new(),
        },
        Line::Lease {
            dest: "g".into(),
            id: "a".into(),
            at: i64::MIN,
            lease_until: i64::MAX,
        },
    ]);
    let mut f = Fold::default();
    f.feed(
        &bytes,
        0,
        FoldOptions {
            now: i64::MAX,
            max_lease_ahead: i64::MAX,
        },
    );
    assert!(f.has("g", "a"));
    let _ = f.due(i64::MAX);
    let _ = f.expired(i64::MIN);
    assert_eq!(
        Line::enqueue("g", "x", "", i64::MAX).clone(),
        Line::Enqueue {
            dest: "g".into(),
            id: "x".into(),
            at: i64::MAX,
            deadline: i64::MAX,
            body: String::new(),
        }
    );
}
