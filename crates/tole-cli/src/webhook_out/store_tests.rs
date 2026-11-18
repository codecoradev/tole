use super::*;
use crate::webhook_out::outbox::{Fold, FoldOptions};

const NOW: i64 = 1_795_000_000;

/// A temp cron dir that removes itself (the suites leave no `tole-*` entry).
pub struct Dir(pub PathBuf);

impl Dir {
    pub fn new(tag: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("tole-outbox-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn opts(now: i64) -> FoldOptions {
    FoldOptions {
        now,
        max_lease_ahead: 1000,
    }
}

fn body(dest: &str, id: &str) -> String {
    format!(
        r#"{{"v":1,"id":"{id}","type":"cron_run_finished","destination":"{dest}","at":{NOW},"data":{{"fire_id":"job@1/2.3"}}}}"#
    )
}

#[test]
fn enqueue_queues_once_and_skips_a_duplicate() {
    let d = Dir::new("dup");
    let o = Outbox::new(&d.0);
    assert_eq!(
        o.enqueue("g", "a", &body("g", "a"), NOW),
        Ok(Enqueued::Queued)
    );
    assert_eq!(
        o.enqueue("g", "a", &body("g", "a"), NOW + 5),
        Ok(Enqueued::Duplicate)
    );
    // The same id for another destination is another event.
    assert_eq!(
        o.enqueue("h", "a", &body("h", "a"), NOW),
        Ok(Enqueued::Queued)
    );
    let f = o.fold(opts(NOW)).unwrap();
    assert_eq!((f.pending_count("g"), f.pending_count("h")), (1, 1));
}

#[test]
fn a_finished_event_is_still_a_duplicate() {
    let d = Dir::new("done");
    let o = Outbox::new(&d.0);
    o.enqueue("g", "a", &body("g", "a"), NOW).unwrap();
    o.append(&[Line::Done {
        dest: "g".into(),
        id: "a".into(),
        at: NOW,
    }])
    .unwrap();
    assert_eq!(
        o.enqueue("g", "a", &body("g", "a"), NOW),
        Ok(Enqueued::Duplicate),
        "a settled run is not announced twice"
    );
}

#[test]
fn a_torn_tail_is_ended_before_the_next_line() {
    let d = Dir::new("torn");
    let o = Outbox::new(&d.0);
    o.enqueue("g", "a", &body("g", "a"), NOW).unwrap();
    // A crash mid-append left a fragment.
    let mut f = OpenOptions::new().append(true).open(&o.file).unwrap();
    f.write_all(br#"{"k":"enqueue","dest":"g","id":"b""#)
        .unwrap();
    drop(f);
    o.enqueue("g", "c", &body("g", "c"), NOW).unwrap();
    let fold = o.fold(opts(NOW)).unwrap();
    assert!(fold.has("g", "a") && fold.has("g", "c") && !fold.has("g", "b"));
    assert_eq!(fold.bad_lines, 1, "the fragment is one bad line");
}

#[cfg(unix)]
#[test]
fn the_file_is_private_and_a_symlink_is_not_followed() {
    use std::os::unix::fs::PermissionsExt;
    let d = Dir::new("mode");
    let o = Outbox::new(&d.0);
    o.enqueue("g", "a", &body("g", "a"), NOW).unwrap();
    let mode = std::fs::metadata(&o.file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let d2 = Dir::new("link");
    let target = d2.0.join("elsewhere");
    std::fs::write(&target, b"keep").unwrap();
    std::os::unix::fs::symlink(&target, d2.0.join("outbox.jsonl")).unwrap();
    let o2 = Outbox::new(&d2.0);
    assert_eq!(
        o2.enqueue("g", "a", &body("g", "a"), NOW),
        Err(EnqueueError::Failed)
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
}

#[test]
fn leasing_takes_one_due_event_per_destination_and_marks_it() {
    let d = Dir::new("lease");
    let o = Outbox::new(&d.0);
    for (dest, id) in [("g", "a"), ("g", "b"), ("h", "c")] {
        o.enqueue(dest, id, &body(dest, id), NOW).unwrap();
    }
    let l = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    assert!(l.got_lock && l.dead.is_empty());
    let ids: Vec<_> = l.leased.iter().map(|e| (&*e.dest, &*e.id)).collect();
    assert_eq!(ids, vec![("g", "a"), ("h", "c")]);
    assert_eq!(l.leased[0].lease_until, NOW + 15);
    assert_eq!(l.leased[0].fire_id.as_deref(), Some("job@1/2.3"));
    // Leased, so a second drainer finds the next one, not the same.
    let l2 = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    let ids: Vec<_> = l2.leased.iter().map(|e| (&*e.dest, &*e.id)).collect();
    assert_eq!(ids, vec![("g", "b")], "a leased event is skipped");
    // The lease ran out: it is due again.
    let l3 = o.lease_due(NOW + 16, &|_| Some(15), 10).unwrap();
    assert!(l3.leased.iter().any(|e| e.id == "a"));
}

#[test]
fn a_destination_that_is_not_configured_is_left_alone() {
    let d = Dir::new("noconf");
    let o = Outbox::new(&d.0);
    o.enqueue("gone", "a", &body("gone", "a"), NOW).unwrap();
    let l = o.lease_due(NOW, &|_| None, 10).unwrap();
    assert!(l.leased.is_empty() && l.dead.is_empty());
    let f = o.fold(opts(NOW)).unwrap();
    assert_eq!(f.pending_count("gone"), 1, "still pending, never sent");
}

#[test]
fn an_event_past_its_deadline_is_marked_dead_with_its_run() {
    let d = Dir::new("expired");
    let o = Outbox::new(&d.0);
    o.enqueue("g", "a", &body("g", "a"), NOW).unwrap();
    let late = NOW + outbox::CRON_DEADLINE_SECS;
    let l = o.lease_due(late, &|_| Some(15), 10).unwrap();
    assert!(l.leased.is_empty());
    assert_eq!(l.dead.len(), 1);
    assert_eq!(l.dead[0].code, "expired");
    assert_eq!(l.dead[0].fire_id.as_deref(), Some("job@1/2.3"));
    let f = o.fold(opts(late)).unwrap();
    assert_eq!(f.pending_count("g"), 0);
}

#[test]
fn a_second_drainer_does_nothing_while_the_lock_is_held() {
    let d = Dir::new("busy");
    let o = Outbox::new(&d.0);
    o.enqueue("g", "a", &body("g", "a"), NOW).unwrap();
    let held = o.open_lock().unwrap();
    held.lock().unwrap();
    let l = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    assert!(!l.got_lock && l.leased.is_empty());
    drop(held);
    assert!(o.lease_due(NOW, &|_| Some(15), 10).unwrap().got_lock);
}

#[test]
fn a_missing_file_is_an_empty_outbox() {
    let d = Dir::new("empty");
    let o = Outbox::new(&d.0);
    assert_eq!(o.size(), 0);
    assert_eq!(o.fold(opts(NOW)).unwrap(), Fold::default());
    let l = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    assert!(l.got_lock && l.leased.is_empty());
}
