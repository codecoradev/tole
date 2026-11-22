//! The reader threads: started and retired by the worker while the driver
//! runs, all ended when it stops.
//!
//! Each reader has a signal of its own, because a retired reader must end
//! while the others go on. The driver's stop reaches them through
//! [`Readers::close`], called by the thread that stops the driver as well as
//! by the worker when it ends, so a stuck worker cannot leave a reader asleep
//! in a backoff. A reader ends with `Input::ReaderEnded`, sent after its last
//! message: the engine counts the stream slot as held until it arrives.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, MutexGuard};

use super::engine::{Input, ReaderId};
use super::threads::reader;
use crate::driver::api::Api;
use crate::driver::clock::{Clock, Shutdown};
use crate::driver::runtime::Spawner;
use crate::driver::shared::thread::run_guarded;

#[derive(Default)]
struct Live {
    /// The driver is stopping: no reader may start.
    closed: bool,
    /// The signal of every reader that has not been retired.
    signals: HashMap<ReaderId, Shutdown>,
}

pub(super) struct Readers {
    api: Arc<dyn Api>,
    clock: Arc<dyn Clock>,
    inbox: SyncSender<Input>,
    spawner: Spawner,
    live: Mutex<Live>,
}

impl Readers {
    pub(super) fn new(
        api: Arc<dyn Api>,
        clock: Arc<dyn Clock>,
        inbox: SyncSender<Input>,
        spawner: Spawner,
    ) -> Self {
        Readers {
            api,
            clock,
            inbox,
            spawner,
            live: Mutex::default(),
        }
    }

    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start the reader `id` of `session`. `Err` when the driver is stopping or
    /// the system cannot make a thread.
    pub(super) fn start(&self, id: ReaderId, session: String) -> io::Result<()> {
        let signal = Shutdown::new();
        {
            let mut live = self.live();
            if live.closed {
                return Err(io::Error::other("the driver is stopping"));
            }
            live.signals.insert(id, signal.clone());
        }
        let (api, clock, inbox) = (
            Arc::clone(&self.api),
            Arc::clone(&self.clock),
            self.inbox.clone(),
        );
        let started = self.spawner.spawn("approvals-stream", move || {
            run_guarded(&inbox, || {
                reader(api, session, inbox.clone(), clock, signal)
            });
            // Never dropped: a lost one would hold a stream slot for ever. The
            // inbox is closed once the worker is gone.
            let _ = inbox.send(Input::ReaderEnded(id));
        });
        if started.is_err() {
            self.live().signals.remove(&id);
        }
        started
    }

    /// Tell reader `id` to end. A reader inside a read ends at its next frame
    /// and sends nothing before it does.
    pub(super) fn retire(&self, id: ReaderId) {
        let signal = self.live().signals.remove(&id);
        if let Some(signal) = signal {
            signal.trigger();
        }
    }

    /// Stop every reader and refuse new ones.
    pub(super) fn close(&self) {
        let signals: Vec<Shutdown> = {
            let mut live = self.live();
            live.closed = true;
            live.signals.drain().map(|(_, s)| s).collect()
        };
        for signal in signals {
            signal.trigger();
        }
    }
}
