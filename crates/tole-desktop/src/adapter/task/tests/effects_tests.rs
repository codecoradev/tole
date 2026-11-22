//! The pure decisions of the effects: polling after an allow.

use crate::adapter::task::{ResumePolling, ResumeStep, MAX_POLL_FAILURES, RESUME_TIMEOUT};
use serde_json::json;
use std::time::Duration;
use tole_client::{Error, SessionStatus};
use tole_desktop_model::task::TaskEvent;

fn status(busy: bool) -> Result<SessionStatus, Error> {
    let row = json!({"id": "s1", "entries": null, "busy": busy, "usage_report": {},
        "model": null, "effort": null, "contextWindow": null, "contextUsed": null,
        "name": null, "snapshotAt": null, "stale": false});
    Ok(serde_json::from_value(row).expect("a status"))
}

fn lost() -> Result<SessionStatus, Error> {
    Err(Error::Io(std::io::ErrorKind::ConnectionReset.into()))
}

const SOON: Duration = Duration::from_millis(500);

fn ready(busy: bool) -> Vec<TaskEvent> {
    vec![TaskEvent::Ready { busy }]
}

#[test]
fn a_busy_snapshot_confirms_and_the_first_idle_one_after_it_ends_the_polling() {
    let mut p = ResumePolling::new();
    assert_eq!(
        p.status(&status(true), SOON),
        ResumeStep {
            events: ready(true),
            finished: false
        }
    );
    assert_eq!(
        p.status(&status(true), SOON * 2),
        ResumeStep {
            events: ready(true),
            finished: false
        }
    );
    assert_eq!(
        p.status(&status(false), SOON * 3),
        ResumeStep {
            events: ready(false),
            finished: true
        }
    );
}

#[test]
fn an_idle_snapshot_before_any_sign_of_life_goes_on_until_the_timeout() {
    let mut p = ResumePolling::new();
    let early = p.status(&status(false), SOON);
    assert_eq!(
        early,
        ResumeStep {
            events: ready(false),
            finished: false
        }
    );
    let late = p.status(&status(false), RESUME_TIMEOUT);
    assert_eq!(
        late,
        ResumeStep {
            events: vec![TaskEvent::ResumeTimedOut],
            finished: true
        }
    );
}

#[test]
fn the_streams_activity_counts_like_a_busy_snapshot_and_a_tick_times_out_without_it() {
    let mut p = ResumePolling::new();
    assert!(!p.tick(RESUME_TIMEOUT - Duration::from_millis(1)).finished);
    let out = p.tick(RESUME_TIMEOUT);
    assert_eq!(
        out,
        ResumeStep {
            events: vec![TaskEvent::ResumeTimedOut],
            finished: true
        }
    );
    let mut seen = ResumePolling::new();
    seen.activity();
    assert!(seen.tick(RESUME_TIMEOUT * 10).events.is_empty());
    assert_eq!(
        seen.status(&status(false), SOON),
        ResumeStep {
            events: ready(false),
            finished: true
        }
    );
}

#[test]
fn lost_polling_ends_an_unconfirmed_turn_after_failures_in_a_row_and_one_good_answer_resets_the_count(
) {
    let mut p = ResumePolling::new();
    for _ in 1..MAX_POLL_FAILURES {
        assert_eq!(
            p.status(&lost(), SOON),
            ResumeStep {
                events: vec![],
                finished: false
            }
        );
    }
    // A good answer in between starts the count again.
    assert!(!p.status(&status(false), SOON).finished);
    assert!(!p.status(&lost(), SOON).finished);
    let end = p.status(&lost(), SOON);
    assert_eq!(
        end,
        ResumeStep {
            events: vec![TaskEvent::ResumeTimedOut],
            finished: true
        }
    );
}

#[test]
fn failures_past_the_timeout_end_it_at_once_and_the_count_cannot_overflow() {
    let mut p = ResumePolling::new();
    assert!(p.status(&lost(), RESUME_TIMEOUT).finished);
    let mut q = ResumePolling::new();
    q.failures = u32::MAX;
    assert!(q.status(&lost(), SOON).finished);
}
