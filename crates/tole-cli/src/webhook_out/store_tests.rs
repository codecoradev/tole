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

/// The id a real cron event of fire `job@1/2.3` has.
fn real_id() -> String {
    crate::webhook_out::event::cron_event_id("job@1/2.3")
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
    let real = real_id();
    for (dest, id) in [("g", real.as_str()), ("g", "b"), ("h", "c")] {
        o.enqueue(dest, id, &body(dest, id), NOW).unwrap();
    }
    let l = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    assert!(l.got_lock && l.dead.is_empty());
    let ids: Vec<_> = l.leased.iter().map(|e| (&*e.dest, &*e.id)).collect();
    assert_eq!(ids, vec![("g", real.as_str()), ("h", "c")]);
    assert_eq!(l.leased[0].lease_until, NOW + 15);
    assert_eq!(l.leased[0].fire_id.as_deref(), Some("job@1/2.3"));
    assert_eq!(
        l.leased[1].fire_id, None,
        "id \"c\" is not that run's event id"
    );
    // Leased, so a second drainer finds the next one, not the same.
    let l2 = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    let ids: Vec<_> = l2.leased.iter().map(|e| (&*e.dest, &*e.id)).collect();
    assert_eq!(ids, vec![("g", "b")], "a leased event is skipped");
    // The lease ran out: it is due again.
    let l3 = o.lease_due(NOW + 16, &|_| Some(15), 10).unwrap();
    assert!(l3.leased.iter().any(|e| e.id == real));
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
    let id = real_id();
    o.enqueue("g", &id, &body("g", &id), NOW).unwrap();
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

#[test]
fn a_forged_fire_id_is_not_trusted_for_a_row() {
    let d = Dir::new("forgedfire");
    let o = Outbox::new(&d.0);
    // A line whose id is not the event id of the fire it names.
    o.enqueue("g", "not-that-run", &body("g", "not-that-run"), NOW)
        .unwrap();
    let l = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    assert_eq!(l.leased.len(), 1);
    assert_eq!(l.leased[0].fire_id, None);
    // Expiry too.
    let late = NOW + outbox::CRON_DEADLINE_SECS + 100;
    let l = o.lease_due(late, &|_| Some(15), 10).unwrap();
    assert_eq!(l.dead[0].fire_id, None);
}

#[test]
fn a_cut_fire_id_gives_no_row_instead_of_a_row_on_a_missing_run() {
    // event.rs cuts data.fire_id at 256 characters; the id hashes the whole.
    let long = format!("{}@1/2.3", "j".repeat(300));
    let id = crate::webhook_out::event::cron_event_id(&long);
    let cut: String = long.chars().take(256).collect();
    let b = format!(
        r#"{{"v":1,"id":"{id}","type":"cron_run_finished","destination":"g","at":{NOW},"data":{{"fire_id":"{cut}"}}}}"#
    );
    let d = Dir::new("cut");
    let o = Outbox::new(&d.0);
    o.enqueue("g", &id, &b, NOW).unwrap();
    let l = o.lease_due(NOW, &|_| Some(15), 10).unwrap();
    assert_eq!(l.leased[0].fire_id, None);
}

#[test]
#[allow(unsafe_code)] // SAFETY: test-only libc call on a path or descriptor the test owns.
fn a_planted_huge_or_special_outbox_is_refused_not_read() {
    // A FIFO must not block the open, and a symlink must not be followed.
    #[cfg(unix)]
    {
        let d = Dir::new("fifo");
        let path = d.0.join("outbox.jsonl");
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let o = Outbox::new(&d.0);
        assert!(o.read_all().is_err(), "a FIFO is not a regular file");
        assert!(o.lease_due(NOW, &|_| Some(15), 10).is_err());

        let d = Dir::new("symread");
        let target = d.0.join("real");
        std::fs::write(&target, b"x\n").unwrap();
        std::os::unix::fs::symlink(&target, d.0.join("outbox.jsonl")).unwrap();
        assert!(Outbox::new(&d.0).read_all().is_err());
    }
    // A sparse file far over the cap is refused without allocating it.
    let d = Dir::new("huge");
    let f = File::create(d.0.join("outbox.jsonl")).unwrap();
    f.set_len(MAX_FILE_BYTES * 3).unwrap();
    drop(f);
    let o = Outbox::new(&d.0);
    assert!(o.read_all().is_err());
    assert_eq!(
        o.enqueue("g", "a", &body("g", "a"), NOW),
        Err(EnqueueError::QueueFull),
        "a producer refuses at the cap"
    );
}

#[test]
fn debug_shows_no_body() {
    let l = Leased {
        dest: "g".into(),
        id: "a".into(),
        fire_id: None,
        body: "SECRET-RUN-OUTPUT".into(),
        lease_until: 1,
        attempts: 0,
        deadline: 2,
    };
    assert!(!format!("{l:?}").contains("SECRET-RUN-OUTPUT"));
}
