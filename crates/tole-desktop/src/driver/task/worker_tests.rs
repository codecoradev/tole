//! The worker's own rules, without the rest of the driver.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::Duration;

use super::calls::{Lane, LANES};
use super::engine::Engine;
use super::fixtures::S;
use super::store::Store;
use super::worker::Worker;
use super::Config;
use crate::driver::clock::{ManualClock, Shutdown};
use crate::driver::fake::FakeApi;
use crate::driver::runtime::Threads;

#[test]
fn a_lane_is_replaced_while_the_driver_runs_and_not_once_it_is_stopping() {
    let mut threads = Threads::new();
    let stop = Shutdown::new();
    let store = Arc::new(Store::new(false));
    let (inbox_tx, inbox) = sync_channel(4);
    let (open, _opened) = sync_channel(1);
    let mut worker = Worker {
        engine: Engine::new(Arc::clone(&store), &Config::new(S)),
        inbox,
        lanes: LANES.iter().map(|_| sync_channel(1).0).collect(),
        open,
        clock: Arc::new(ManualClock::new(0)),
        stop: stop.clone(),
        store,
        alive: Arc::new(AtomicBool::new(true)),
        on_change: None,
        spawner: threads.spawner(),
        api: FakeApi::new(),
        session: S.to_owned(),
        inbox_tx,
    };
    assert!(worker.replace(Lane::Prompt));
    stop.trigger();
    assert!(worker.replace(Lane::Prompt), "stopping is not a failure");
    // The thread of the first replacement ends when its channel is dropped.
    drop(worker);
    let report = threads.join(Duration::from_secs(5));
    assert_eq!(
        (report.joined, report.detached),
        (1, 0),
        "only the first started a thread"
    );
}
