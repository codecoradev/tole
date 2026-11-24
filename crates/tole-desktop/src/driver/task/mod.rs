//! The task driver: keeps the model's `TaskState` true to one session of a
//! serve, and carries out the owner's actions on it.
//!
//! It connects the adapter's pure functions (`adapter::task`) to the blocking
//! client and follows the contracts written there and in the model's
//! `task/event.rs`: the history is read page by page and only then is the
//! event stream opened from its cursor; the approvals queue is read after the
//! stream opens and after every reconnect; a prompt, a steer note, a Stop and
//! an answer to an approval are calls of their own; after an allow, a 409 or
//! a Stop the status is polled so the model is never left busy with nothing
//! watching. The rules are in `engine` (`Engine::step`), which has no thread
//! and no I/O and is what the tests drive.
//!
//! # Thread model
//!
//! One driver per task. All threads are started by [`TaskDriver::start`] and
//! stopped by [`TaskDriver::shutdown`]:
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `worker` (1) | reads one bounded inbox, runs `Engine::step`, hands the effects to the others, tells the UI | the inbox |
//! | `reader` (1) | follows the event stream from the history's cursor, reopens it with a backoff, parks and probes a session the serve refuses | the stream |
//! | `ticker` (1) | wakes the worker every 250 ms | the clock |
//! | `task-read` | `GET /entries` pages, then `GET /approvals?session=` | the call |
//! | `task-status` | `GET /status` | the call |
//! | `task-prompt` | `POST /prompt`, which answers when the turn ends | the call (minutes) |
//! | `task-stop` | `POST /cancel` | the call |
//! | `task-steer` | `POST /interrupt` | the call |
//! | `task-decide` | `POST /approvals/{id}/decision` | the call |
//!
//! Only the worker changes the state, one input at a time. The state sits
//! behind a mutex held for the apply and nothing else (never across a call or
//! a send), so [`Handle::snapshot`] waits at most that long; `on_change` runs on
//! the worker with no lock held. A call that blocks blocks its own lane, not
//! the worker. Each lane has one call in flight, and the engine does not ask
//! for another before the reply is applied. The inbox is bounded (`INBOX`):
//! the reader and the lanes wait when it is full (so an event of loss, a reply
//! or a stream item is never dropped), the ticker drops its tick, and the
//! UI's commands return [`Busy`].
//!
//! # Cadences
//!
//! All deadlines are on the monotonic clock; the wall clock is read only to
//! see a suspend (read the status and the queue again).
//!
//! - Tick: 250 ms. It only wakes the worker; nothing is polled that is not due.
//! - History: the next page at once; a failed page after 1 s doubling to 30 s,
//!   for ever.
//! - After an allow (`ResumePolling`): `GET /status` every 500 ms until the turn
//!   shows a sign of life, then every 3 s; after failed polls the wait grows
//!   (1 s, 2 s, 4 s) to 5 s. The turn is ended by 5 s without a sign of life,
//!   or by a not-busy snapshot after one.
//! - Any other busy turn nobody waits for (`BusyPolling`): every 500 ms, then
//!   every 5 s after repeated failures; after a Stop that was accepted, Stop
//!   is offered again after about 20 s of busy snapshots.
//! - Queue of approvals: after the stream opens and after each reconnect; a
//!   failed read after 2 s doubling to 60 s, for ever.
//! - Stream: reopened after 1 s doubling to 30 s; parked after three refusals
//!   and probed every 30 to 40 s. While no stream is up (slow to open,
//!   refused, parked) the status and the queue are read over REST, first 5 s
//!   after the history, then every 10 s doubling to 60 s, so Stop and the
//!   approval cards work without a stream (sending stays off until one is up).
//! - A prompt call still out 30 s after the model read idle (3 s after a
//!   suspend) is abandoned: see `Handle::send`.
//!
//! # Panics
//!
//! A panic inside an `Api` call is that call's failure (the engine releases the
//! lane, retries or shows the failure); the thread goes on. A panic anywhere
//! else in a thread is reported to the worker, which ends the driver: it stops
//! the other threads and clears the state, so the view reads stopped
//! ([`LinkState::Stopped`], nothing can be sent) and [`Handle::is_running`] is
//! false. A panic that leaves the state lock poisoned ends it the same way.
//!
//! # Stopping
//!
//! `shutdown` raises one stop signal (it wakes every sleep) and joins the
//! threads for up to a timeout. A thread inside a call is not interrupted (the
//! client has no cancel): it is reported as `detached`, ends when the call
//! returns and sends nothing after the signal. A prompt in flight does not
//! stop the turn on the serve: it only stops being waited for here. The reader
//! is always one of them while its stream is open (a blocking read cannot be
//! cancelled), and keeps its stream slot on the serve until its next frame (a
//! heartbeat, at most 15 s) or the 45 s idle timeout: a page opened again at
//! once may be refused (429) and probed until the slot is free. A lane thread
//! whose call was abandoned is counted as detached too.
//!
//! # Limits
//!
//! - While the history is read nothing can be sent ([`TaskView::loading`]).
//! - Images are not attached: the prompt is text only.
//! - A prompt call that never answers is abandoned, not cancelled: its thread
//!   stays in the call (a read timeout of two hours, `PROMPT_TIMEOUT`, ends it)
//!   and the serve may still run the turn.
//! - The model shows one card per cleaned approval id. The driver answers
//!   under the serve's own id (`engine::ids`), but two requests whose ids clean
//!   to the same text are one card that cannot be answered here
//!   ([`TaskView::undecidable`]): Stop cancels the turn and drops those cards (one that the serve still holds
//!   shows again after the next reconnect).

mod calls;
#[cfg(test)]
mod e2e_fault_tests;
#[cfg(test)]
mod e2e_tests;
mod engine;
#[cfg(test)]
mod engine_abandon_tests;
#[cfg(test)]
mod engine_cadence_tests;
#[cfg(test)]
mod engine_failed_tests;
#[cfg(test)]
mod engine_guard_tests;
#[cfg(test)]
mod engine_history_tests;
#[cfg(test)]
mod engine_ids_tests;
#[cfg(test)]
mod engine_live_tests;
#[cfg(test)]
mod engine_send_tests;
#[cfg(test)]
mod engine_stop_tests;
#[cfg(test)]
mod engine_watch_tests;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod hostile_tests;
mod preview;
#[cfg(test)]
mod rig;
#[cfg(test)]
mod sim;
mod store;
mod threads;
mod view;
mod worker;
#[cfg(test)]
mod worker_tests;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::approvals::Decision;

use self::calls::{Call, LANES};
use self::engine::{Engine, Input, TICK_EVERY};
pub use self::preview::Preview;
use self::store::Store;
use self::threads::{reader, spawn_lane};
pub use self::view::{LinkState, TaskView};
use self::worker::Worker;
use super::api::Api;
use super::clock::{Clock, Shutdown};
use super::runtime::{JoinReport, Threads};
use super::shared::handle;
pub use super::shared::handle::{Busy, OnChange};
use super::shared::thread::{run_guarded, spawn_ticker, stop_threads, DROP_JOIN};

/// The room in the worker's inbox.
const INBOX: usize = 256;

/// Event streams the serve allows, in all, for every client of it together.
pub const SERVE_STREAM_SLOTS: usize = 8;
/// Task drivers an app may have open at once: each holds one stream slot for as
/// long as it lives, the approvals driver holds up to
/// `approvals::MAX_STREAMS` (4) more, and the CLI or another window needs room.
/// A fourth task driver would take a slot the others need: it is refused (429),
/// parks after three tries and then only reads over REST (a few seconds late,
/// no live text), so the app must cap itself, and say so in the UI, rather
/// than rely on that. One session followed by an open task and by the
/// approvals driver takes two slots.
pub const MAX_OPEN_TASKS: usize = 3;

pub struct Config {
    /// The id of the session this driver follows.
    pub session: String,
    /// The session was opened in plan mode (`SessionOpened::plan_mode`).
    pub look_only: bool,
    /// The session is new and has no history: the stream opens at once.
    pub fresh: bool,
    pub on_change: Option<OnChange>,
}

impl Config {
    /// A session to follow, with its history still to read.
    pub fn new(session: impl Into<String>) -> Self {
        Config {
            session: session.into(),
            look_only: false,
            fresh: false,
            on_change: None,
        }
    }
}

/// The UI's side of the driver: cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Handle {
    store: Arc<Store>,
    inbox: SyncSender<Input>,
    alive: Arc<AtomicBool>,
}

impl Handle {
    /// The task as of now. After the driver stopped it reads
    /// [`LinkState::Stopped`] with nothing sendable.
    pub fn snapshot(&self) -> TaskView {
        self.store.view(self.is_running())
    }

    /// The UI's list shows the rows around `scroll_top` (pixels), `height`
    /// tall, with `overscan` more on each side: what the next snapshots'
    /// `rows` hold. Not a command: it changes no model state and cannot be
    /// refused (a value that is not a finite number is ignored).
    pub fn set_viewport(&self, scroll_top: f32, height: f32, overscan: f32) {
        if [scroll_top, height, overscan].iter().all(|v| v.is_finite()) {
            self.store.lock().viewport = store::Viewport {
                scroll_top,
                height,
                overscan,
            };
        }
    }

    /// Send `text` as a new prompt. `Ok` only means the command was queued:
    /// only when the task is idle, the history is read and the previous prompt
    /// call returned does anything happen. Otherwise the command is counted in
    /// [`TaskView::ignored`] (the view says why: [`TaskView::status`] `send`,
    /// `loading`), so a UI keeps the text it typed until the transcript shows
    /// it. A failed send puts the words back in [`TaskView::draft`]. A prompt
    /// call that never answers is abandoned once the model has read idle for
    /// 30 s (3 s after a suspend): the lane is replaced and sending is open
    /// again; the answer of the abandoned call, if it ever comes, is dropped.
    pub fn send(&self, text: &str) -> Result<(), Busy> {
        self.command(Input::Send(text.to_owned()))
    }

    /// Send `text` as a note to the running turn. Only while the turn runs
    /// (and no other note is on its way).
    pub fn steer(&self, text: &str) -> Result<(), Busy> {
        self.command(Input::Steer(text.to_owned()))
    }

    /// Stop the running turn. Only when the view offers it
    /// ([`TaskView::status`] `can_stop`); otherwise counted in
    /// [`TaskView::ignored`].
    pub fn stop(&self) -> Result<(), Busy> {
        self.command(Input::Stop)
    }

    /// Answer the approval `id` of this task. Sent at most once: an id the
    /// task does not show, one already being answered, or one that can only be
    /// answered elsewhere ([`TaskView::undecidable`]) is refused (counted in
    /// [`TaskView::ignored`]) and nothing is sent. The outcome shows in the view.
    pub fn decide(&self, id: &str, decision: Decision) -> Result<(), Busy> {
        self.command(Input::Decide(id.to_owned(), decision))
    }

    /// The measured heights of rows the UI built (`(row, pixels)`).
    pub fn set_heights(&self, heights: Vec<(usize, f32)>) -> Result<(), Busy> {
        self.command(Input::Heights(heights))
    }

    /// Open or close the "Working" group at row `index`.
    pub fn toggle_group(&self, index: usize) -> Result<(), Busy> {
        self.command(Input::Toggle(index))
    }

    /// Whether the worker is running.
    pub fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn command(&self, input: Input) -> Result<(), Busy> {
        // `alive` goes false before the inbox is closed: refuse from then on.
        if !self.is_running() {
            return Err(Busy);
        }
        handle::queue(&self.inbox, input)
    }
}

/// The running driver: owns its threads.
pub struct TaskDriver {
    handle: Handle,
    stop: Shutdown,
    threads: Threads,
}

impl TaskDriver {
    /// Start the threads. The first thing the worker does is read the history;
    /// until it is in, the view is `loading` and nothing can be sent. `Err`
    /// only when the system cannot start a thread (nothing is left running).
    pub fn start(api: Arc<dyn Api>, clock: Arc<dyn Clock>, cfg: Config) -> io::Result<Self> {
        let store = Arc::new(Store::new(cfg.look_only));
        let stop = Shutdown::new();
        let alive = Arc::new(AtomicBool::new(true));
        let (inbox, inbox_rx) = sync_channel::<Input>(INBOX);
        let (open_tx, open_rx) = sync_channel::<u64>(1);
        let (mut lane_tx, mut lane_rx) = (Vec::new(), Vec::new());
        for _ in LANES {
            let (tx, rx) = sync_channel::<Call>(1);
            lane_tx.push(tx);
            lane_rx.push(rx);
        }
        let handle = Handle {
            store: Arc::clone(&store),
            inbox: inbox.clone(),
            alive: Arc::clone(&alive),
        };
        let mut driver = TaskDriver {
            handle,
            stop: stop.clone(),
            threads: Threads::new(),
        };
        let session = cfg.session.clone();
        let spawner = driver.threads.spawner();
        let started = (|| {
            let t = &mut driver.threads;
            let worker = Worker {
                engine: Engine::new(Arc::clone(&store), &cfg),
                inbox: inbox_rx,
                lanes: lane_tx,
                open: open_tx,
                clock: Arc::clone(&clock),
                stop: stop.clone(),
                store: Arc::clone(&store),
                alive,
                on_change: cfg.on_change,
                spawner: spawner.clone(),
                api: Arc::clone(&api),
                session: session.clone(),
                inbox_tx: inbox.clone(),
            };
            t.spawn("task-worker", move || worker.run())?;
            let (i, c, s) = (inbox.clone(), Arc::clone(&clock), stop.clone());
            spawn_ticker(t, "task-tick", i, c, s, TICK_EVERY)?;
            for (kind, jobs) in LANES.into_iter().zip(lane_rx.drain(..)) {
                let ends = (inbox.clone(), stop.clone());
                spawn_lane(
                    &spawner,
                    Arc::clone(&api),
                    session.clone(),
                    kind,
                    jobs,
                    ends,
                )?;
            }
            let (a, n, i, c, s) = (
                Arc::clone(&api),
                session.clone(),
                inbox.clone(),
                Arc::clone(&clock),
                stop.clone(),
            );
            t.spawn("task-stream", move || {
                run_guarded(&i.clone(), || reader(a, n, open_rx, i, c, s))
            })?;
            io::Result::Ok(())
        })();
        match started {
            Ok(()) => Ok(driver),
            Err(e) => {
                driver.stop(DROP_JOIN);
                Err(e)
            }
        }
    }

    /// The UI's side. Clones stay usable after the driver stops: they read
    /// stopped and refuse commands.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Stop every thread and wait up to `timeout` for them.
    pub fn shutdown(mut self, timeout: Duration) -> JoinReport {
        self.stop(timeout)
    }

    fn stop(&mut self, timeout: Duration) -> JoinReport {
        stop_threads(&self.stop, &self.handle.inbox, &mut self.threads, timeout)
    }
}

impl Drop for TaskDriver {
    fn drop(&mut self) {
        self.stop(DROP_JOIN);
    }
}
