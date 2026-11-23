//! Stopping drivers: all at once, and counting what they report.

use std::sync::{Arc, Mutex};
use std::thread;

use crate::driver::runtime::JoinReport;

/// One driver's shutdown, ready to run on any thread.
pub(super) type Job = Box<dyn FnOnce() -> JoinReport + Send>;

/// Run every shutdown at the same time and add up the reports. A driver's
/// shutdown waits up to its own timeout for a reader that is blocked in a read
/// (and then detaches it), so run one after the other the waits would add up
/// to one timeout per driver, and the serve's streams would be held that
/// much longer. Each job returns within its own timeout, so this does too.
/// A thread that cannot be started means the job runs on this one.
pub(super) fn all(jobs: Vec<Job>) -> JoinReport {
    thread::scope(|scope| {
        let mut total = JoinReport::default();
        let mut running = Vec::new();
        for job in jobs {
            let slot = Arc::new(Mutex::new(Some(job)));
            let handed = Arc::clone(&slot);
            let started = thread::Builder::new()
                .name("backend-reap".into())
                .spawn_scoped(scope, move || take(&handed).map(|job| job()));
            match started {
                Ok(handle) => running.push(handle),
                // No thread for it: stop it here, now.
                Err(_) => total = sum(total, take(&slot).map(|job| job()).unwrap_or_default()),
            }
        }
        for handle in running {
            let report = match handle.join() {
                Ok(report) => report.unwrap_or_default(),
                Err(_) => JoinReport {
                    panicked: 1,
                    ..JoinReport::default()
                },
            };
            total = sum(total, report);
        }
        total
    })
}

fn take(slot: &Mutex<Option<Job>>) -> Option<Job> {
    slot.lock().ok().and_then(|mut job| job.take())
}

/// The two reports as one.
pub(super) fn sum(a: JoinReport, b: JoinReport) -> JoinReport {
    JoinReport {
        joined: a.joined.saturating_add(b.joined),
        panicked: a.panicked.saturating_add(b.panicked),
        detached: a.detached.saturating_add(b.detached),
    }
}
