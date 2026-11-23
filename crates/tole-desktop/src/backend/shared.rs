//! What the worker and the UI's side of the backend share, and the message
//! the worker's thread takes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, MutexGuard};

use super::link::Look;
use super::reap;
use super::tasks::TaskSet;
use crate::driver::approvals;
use crate::driver::runtime::JoinReport;
use crate::driver::shared::handle::OnChange;
use crate::driver::shared::thread::DriverInput;
use crate::driver::skills;
use crate::driver::task;

/// What reaches the worker's inbox. None of it carries state: the worker reads
/// the link, the open tasks and the drivers afresh at every message, so a
/// message that is dropped (a full inbox) loses nothing, and stopping is the
/// [`Shutdown`](crate::driver::Shutdown) signal, which is never dropped.
pub(super) enum Msg {
    /// The clock moved on.
    Tick,
    /// Look again: a driver changed, or the app asked for something.
    Wake,
    /// The ticker died by a panic: the backend ends.
    ThreadFailed,
}

impl DriverInput for Msg {
    fn tick() -> Self {
        Msg::Tick
    }

    fn wake() -> Self {
        Msg::Wake
    }

    fn thread_failed() -> Self {
        Msg::ThreadFailed
    }
}

/// The handles of the drivers of one attachment to the serve. Never changes
/// once published; the worker publishes a new one when the drivers do.
pub(super) struct Attached {
    /// The link's epoch when the drivers were started.
    pub epoch: u64,
    pub approvals: approvals::Handle,
    pub skills: skills::Handle,
    /// One per task driver that runs, in the order they were started.
    pub tasks: Vec<(String, task::Handle)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing behind these locks has an invariant a panic could break.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The state the worker publishes and the UI reads. Every lock is held for a
/// clone or an assignment, never across a call.
pub(super) struct Shared {
    attached: Mutex<Option<Arc<Attached>>>,
    /// The tasks the app has open: its wish, whatever runs.
    pub tasks: TaskSet,
    /// What the drivers shut down so far reported.
    totals: Mutex<JoinReport>,
    alive: AtomicBool,
}

impl Shared {
    pub(super) fn new() -> Self {
        Shared {
            attached: Mutex::new(None),
            tasks: TaskSet::default(),
            totals: Mutex::new(JoinReport::default()),
            alive: AtomicBool::new(true),
        }
    }

    pub(super) fn publish(&self, attached: Option<Arc<Attached>>) {
        *lock(&self.attached) = attached;
    }

    /// The drivers the UI may read: only those started under the serve that
    /// is there now. A look that says the serve is not ready, or is another
    /// one, finds none, so the view never shows what an old serve said, even
    /// before the worker has caught up with the change.
    pub(super) fn current(&self, look: &Look) -> Option<Arc<Attached>> {
        let attached = lock(&self.attached).clone()?;
        (look.state.is_ready() && attached.epoch == look.epoch).then_some(attached)
    }

    pub(super) fn add_report(&self, report: JoinReport) {
        let mut totals = lock(&self.totals);
        *totals = reap::sum(*totals, report);
    }

    pub(super) fn totals(&self) -> JoinReport {
        *lock(&self.totals)
    }

    pub(super) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub(super) fn set_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// Where the `on_change` of every driver arrives, as one flag: a storm of
/// driver events is one pending change, which the worker reports to the UI at
/// a bounded pace.
pub(super) struct Fanin {
    dirty: AtomicBool,
    inbox: SyncSender<Msg>,
}

impl Fanin {
    pub(super) fn new(inbox: SyncSender<Msg>) -> Arc<Self> {
        Arc::new(Fanin {
            dirty: AtomicBool::new(false),
            inbox,
        })
    }

    /// Something a view shows changed. Wakes the worker only when none was
    /// pending; a full inbox is no loss, because every message the worker
    /// takes ends by looking at the flag.
    pub(super) fn mark(&self) {
        if !self.dirty.swap(true, Ordering::SeqCst) {
            let _ = self.inbox.try_send(Msg::Wake);
        }
    }

    pub(super) fn is_pending(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    /// Whether a change was pending; clears it.
    pub(super) fn take(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }

    /// The `on_change` to give a driver.
    pub(super) fn callback(self: &Arc<Self>) -> OnChange {
        let fan = Arc::clone(self);
        Arc::new(move || fan.mark())
    }
}
