//! Time for the drivers: the clock they read and sleep on, and the signal that
//! stops them.
//!
//! A driver never calls `SystemTime` or `thread::sleep` itself. It asks its
//! [`Clock`] for `now_ms` (the `u64` the model's actions carry) and sleeps with
//! [`Clock::sleep`], which a [`Shutdown`] interrupts. Tests give the driver a
//! [`ManualClock`]: time moves only when the test calls
//! [`ManualClock::advance`], so a backoff of 30 s is one call, not 30 s.
//!
//! Two readings of time, never mixed: [`Clock::now_ms`] is the wall clock, the
//! `now_ms` the model's actions carry (the model's deadlines are wall-clock by
//! contract); it can step back or forward when the system clock is corrected.
//! [`Clock::monotonic_ms`] only moves forward at the pace of real time, and is
//! what every driver deadline (retry, refresh, age of the last fetch, ticks)
//! is measured on, so a clock correction cannot stall or rush them.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A mutex whose data has no invariant a panic could break (a flag, a counter).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The stop signal of a driver: raised once, seen by every thread, and it wakes
/// every sleeper at once. Cheap to clone; all clones are one signal.
#[derive(Clone, Default)]
pub struct Shutdown(Arc<Inner>);

#[derive(Default)]
struct Inner {
    /// `(stopped, epoch)`. The epoch counts wake-ups, so a waiter that read it
    /// before checking its condition never misses one that came after.
    state: Mutex<(bool, u64)>,
    wake: Condvar,
}

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// Raise the signal and wake every sleeper. It stays raised.
    pub fn trigger(&self) {
        let mut s = lock(&self.0.state);
        s.0 = true;
        s.1 = s.1.wrapping_add(1);
        self.0.wake.notify_all();
    }

    pub fn is_set(&self) -> bool {
        lock(&self.0.state).0
    }

    /// Wake the sleepers without stopping (a clock that moved).
    fn poke(&self) {
        let mut s = lock(&self.0.state);
        s.1 = s.1.wrapping_add(1);
        self.0.wake.notify_all();
    }

    fn epoch(&self) -> u64 {
        lock(&self.0.state).1
    }

    /// Block until stopped, until the epoch is no longer `seen`, or (with a
    /// timeout) until it ran out.
    fn wait_change(&self, seen: u64, timeout: Option<Duration>) {
        let s = lock(&self.0.state);
        let still = |s: &mut (bool, u64)| !s.0 && s.1 == seen;
        // The guard is dropped on return; a poisoned wait is as good as any.
        match timeout {
            Some(t) => drop(self.0.wake.wait_timeout_while(s, t, still)),
            None => drop(self.0.wake.wait_while(s, still)),
        }
    }
}

/// How a [`Clock::sleep`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slept {
    /// The whole duration passed.
    Elapsed,
    /// The [`Shutdown`] was raised first.
    Stopped,
}

/// The time source of a driver.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch: the `now_ms` of the model's actions.
    fn now_ms(&self) -> u64;

    /// Milliseconds on a clock that never steps back (from an arbitrary
    /// start): for deadlines and durations, never for the model.
    fn monotonic_ms(&self) -> u64;

    /// Wait for `d` on this clock, or until `stop` is raised.
    fn sleep(&self, d: Duration, stop: &Shutdown) -> Slept;
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // A clock before 1970 reads as 0; a huge value saturates.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, millis)
    }

    fn monotonic_ms(&self) -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        millis(START.get_or_init(Instant::now).elapsed())
    }

    fn sleep(&self, d: Duration, stop: &Shutdown) -> Slept {
        let start = Instant::now();
        loop {
            let seen = stop.epoch();
            if stop.is_set() {
                return Slept::Stopped;
            }
            let left = d.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Slept::Elapsed;
            }
            stop.wait_change(seen, Some(left));
        }
    }
}

/// A clock that moves only when told to, for tests. Sleepers wake when
/// [`advance`](Self::advance) carries the time to their deadline, or when their
/// [`Shutdown`] is raised; no real time passes.
#[derive(Default)]
pub struct ManualClock {
    /// `(wall, monotonic)`.
    now: Mutex<(u64, u64)>,
    /// The signals of the sleepers, poked on every `advance`.
    sleepers: Mutex<Vec<Shutdown>>,
    /// The sleeps, so a test can wait for a thread to be asleep before it
    /// moves the time (a sleep that begins after the move would wait for the
    /// next one) and `advance` can wait for the sleepers that fall due.
    sleeps: Mutex<Sleeps>,
    sleeps_changed: Condvar,
}

#[derive(Default)]
struct Sleeps {
    /// Sleeps begun, ever.
    begun: u64,
    /// The deadline (monotonic) of every sleep in progress.
    deadlines: Vec<u64>,
}

/// Counts a sleep as in progress for as long as it lives.
struct Asleep<'a>(&'a ManualClock, u64);

impl Drop for Asleep<'_> {
    fn drop(&mut self) {
        let mut sleeps = lock(&self.0.sleeps);
        if let Some(i) = sleeps.deadlines.iter().position(|d| *d == self.1) {
            sleeps.deadlines.swap_remove(i);
        }
        self.0.sleeps_changed.notify_all();
    }
}

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        Self {
            now: Mutex::new((start_ms, 0)),
            ..Self::default()
        }
    }

    /// How many [`Clock::sleep`] calls have begun, ever.
    pub fn sleeps_begun(&self) -> u64 {
        lock(&self.sleeps).begun
    }

    /// Block until at least `n` sleeps are in progress at once (`false` after
    /// `timeout`): a test that moves the time waits for the threads it expects
    /// to be asleep, so none of them starts its sleep after the move.
    pub fn wait_sleepers(&self, n: usize, timeout: Duration) -> bool {
        let sleeps = lock(&self.sleeps);
        match self
            .sleeps_changed
            .wait_timeout_while(sleeps, timeout, |s| s.deadlines.len() < n)
        {
            Ok((_, waited)) => !waited.timed_out(),
            Err(_) => false,
        }
    }

    /// Block until at least `n` sleeps have begun; `false` if that did not
    /// happen within `timeout` (real time: a bound for a test that would hang).
    pub fn wait_sleeps_begun(&self, n: u64, timeout: Duration) -> bool {
        let sleeps = lock(&self.sleeps);
        match self
            .sleeps_changed
            .wait_timeout_while(sleeps, timeout, |s| s.begun < n)
        {
            Ok((_, waited)) => !waited.timed_out(),
            Err(_) => false,
        }
    }

    /// Move both readings forward (saturating) and wake the sleepers to look.
    /// Returns once every sleep that fell due has ended, so a test that ticks
    /// fast cannot run the clock past a thread that is slow to wake (that
    /// thread would otherwise start its next sleep from a later time).
    pub fn advance(&self, d: Duration) {
        let mono = {
            let mut now = lock(&self.now);
            now.0 = now.0.saturating_add(millis(d));
            now.1 = now.1.saturating_add(millis(d));
            now.1
        };
        self.wake_sleepers();
        self.wait_due_sleepers_end(mono);
    }

    /// Move only the monotonic reading: time passes for every deadline of a
    /// driver while the wall clock, which the model's own deadlines (a
    /// request's 15 minutes, a lost fetch's 2 minutes) read, stands still.
    pub fn advance_monotonic(&self, d: Duration) {
        let mono = {
            let mut now = lock(&self.now);
            now.1 = now.1.saturating_add(millis(d));
            now.1
        };
        self.wake_sleepers();
        self.wait_due_sleepers_end(mono);
    }

    /// Set the wall clock only, forward or back: a system clock that was
    /// corrected. The monotonic reading does not move.
    pub fn set_wall_ms(&self, wall_ms: u64) {
        lock(&self.now).0 = wall_ms;
    }

    /// Wait (in real time, bounded) until no sleep with a deadline at or before
    /// `mono` is still in progress: they have been woken and are leaving.
    fn wait_due_sleepers_end(&self, mono: u64) {
        let sleeps = lock(&self.sleeps);
        drop(
            self.sleeps_changed
                .wait_timeout_while(sleeps, Duration::from_secs(10), |s| {
                    s.deadlines.iter().any(|d| *d <= mono)
                }),
        );
    }

    fn wake_sleepers(&self) {
        let woken = std::mem::take(&mut *lock(&self.sleepers));
        for s in woken {
            s.poke();
        }
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        lock(&self.now).0
    }

    fn monotonic_ms(&self) -> u64 {
        lock(&self.now).1
    }

    fn sleep(&self, d: Duration, stop: &Shutdown) -> Slept {
        let deadline = self.monotonic_ms().saturating_add(millis(d));
        {
            let mut sleeps = lock(&self.sleeps);
            sleeps.begun += 1;
            sleeps.deadlines.push(deadline);
            self.sleeps_changed.notify_all();
        }
        let _asleep = Asleep(self, deadline);
        loop {
            let seen = stop.epoch();
            // Registered before the time is read: an `advance` after the read
            // finds this signal in the list and pokes it.
            lock(&self.sleepers).push(stop.clone());
            if stop.is_set() {
                return Slept::Stopped;
            }
            if self.monotonic_ms() >= deadline {
                return Slept::Elapsed;
            }
            stop.wait_change(seen, None);
        }
    }
}
