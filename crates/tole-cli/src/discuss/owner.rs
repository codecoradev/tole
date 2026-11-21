//! The owner's discussions run in the background (#1042): what `tole serve`
//! needs to start, continue and stop a discussion without blocking a request.
//!
//! Nothing here decides what a discussion may do. A start validates and
//! records through [`discuss::begin`] and a continue through
//! [`super::prepare_resume`], the same functions `tole discuss start` and
//! `resume` run, so the contract rules, the participant rules (profiles of the
//! owner's config, never a built-in role), the hard token budget, the depth
//! (a discussion started here is depth 0 with inline, read-only voices and no
//! tools; one the terminal started with `--tool` has no host here to start its
//! helper children, so over serve it can be listed, shown and stopped, and a
//! resume of it while open is refused: [`discuss::prepare_resume`] seats
//! helpers only when a step can still start one) and the background-file
//! checks are the CLI's. This module adds
//! only a thread per running discussion. There is no limit on how many run at
//! once (owner decision of 2026-11-20): each discussion's own budget is its
//! bound, and the one thing refused is the same discussion running twice.
//! A stop is the ledger's own escalation ([`DiscussionLog::stop`]): no text a
//! participant wrote can start, prevent or delay it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tole_core::discussion_log::{describe, DiscussionLog, Next};

use super::{Env, StartArgs};

/// One discussion driven by this process.
struct Live {
    /// Set when its thread has ended.
    done: Arc<AtomicBool>,
}

/// Whether a thread of this process still drives `id`.
fn is_live(live: &BTreeMap<String, Live>, id: &str) -> bool {
    live.get(id).is_some_and(|l| !l.done.load(Ordering::SeqCst))
}

/// A registration of the caller as the driver of one discussion; the
/// discussion counts as running until it is dropped, even if the caller
/// panics.
struct Claim(Arc<AtomicBool>);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The discussions this process runs for its owner.
#[derive(Default)]
pub struct Owner {
    live: Mutex<BTreeMap<String, Live>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Owner {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True while a thread of this process drives `id`.
    pub fn running(&self, id: &str) -> bool {
        is_live(&self.lock(), id)
    }

    /// Registers the caller as the driver of `id` for as long as the returned
    /// guard lives, or `None` when this process already drives it. The check
    /// and the registration are one step under the lock, so `resume`, a second
    /// `stop` and this caller never drive one discussion at the same time.
    fn claim(&self, id: &str) -> Option<Claim> {
        let mut live = self.lock();
        if is_live(&live, id) {
            return None;
        }
        let done = Arc::new(AtomicBool::new(false));
        live.insert(
            id.to_string(),
            Live {
                done: Arc::clone(&done),
            },
        );
        Some(Claim(done))
    }

    /// `running`, `closed`, or `interrupted` (open, and no thread of this
    /// process drives it: a crash, a failed call, or another process).
    pub fn state(&self, id: &str, closed: bool) -> &'static str {
        match (closed, self.running(id)) {
            (true, _) => "closed",
            (false, true) => "running",
            (false, false) => "interrupted",
        }
    }

    /// Starts a discussion from the owner's contract and returns at once; it
    /// runs on its own thread. Everything that can be refused is refused
    /// before the thread starts and before anything is spent.
    pub fn start(&self, args: &StartArgs, env: &Env<'_>) -> Result<Value, String> {
        // Held across the validation (a few file reads and one ledger line, no
        // model call), so two requests for one id are decided one after the
        // other: the ledger takes the contract once.
        let mut live = self.lock();
        let ready = super::begin(args, env)?;
        let out = json!({
            "id": ready.id, "status": "running", "plan": ready.plan, "advice": ready.advice,
        });
        spawn(&mut live, ready.id, ready.driver, env.dir)?;
        Ok(env.secrets.redact_value(&out))
    }

    /// Continues discussion `id` where its ledger stands (after a crash or a
    /// failed call), like `tole discuss resume`. A closed one or one that is
    /// already running is refused.
    pub fn resume(&self, id: &str, env: &Env<'_>) -> Result<Value, String> {
        let mut live = self.lock();
        if is_live(&live, id) {
            return Err(format!("discussion `{id}` is already running"));
        }
        if DiscussionLog::at(env.dir, id)?.state()?.closed.is_some() {
            return Err(format!("discussion `{id}` is closed: start a new one"));
        }
        let ready = super::prepare_resume(id, env)?;
        spawn(&mut live, ready.id, ready.driver, env.dir)?;
        Ok(json!({"id": id, "status": "running"}))
    }

    /// Stops discussion `id`: the stop is recorded as the ledger's escalation
    /// and the discussion closes with the usual report. A running one closes
    /// on its thread at its next step; one nobody runs here is closed now (no
    /// model call is needed). Stopping a closed discussion changes nothing.
    pub fn stop(&self, id: &str, env: &Env<'_>) -> Result<Value, String> {
        let log = DiscussionLog::at(env.dir, id)?;
        let st = log.state()?;
        if st.contract.is_none() {
            return Err(format!("no discussion `{id}`"));
        }
        let gate = log.stop(now_secs())?;
        let mut note = None;
        if st.closed.is_none() {
            // Closing here drives the discussion, so it is registered as
            // running while it does: a resume that arrives meanwhile is
            // refused, not run next to it. When a thread already drives it,
            // that thread closes it at its next step.
            if let Some(_driving) = self.claim(id) {
                // A profile that is gone cannot be seated again; the stop is
                // recorded all the same and a later resume closes it.
                note = super::prepare_resume(id, env)
                    .and_then(|r| r.driver.run())
                    .err()
                    .map(|e| env.secrets.redact(&e).into_owned());
            }
        }
        let closed = log.state()?.closed.is_some();
        let mut out = json!({
            "id": id,
            "outcome": describe(gate),
            "state": self.state(id, closed),
        });
        if let Some(n) = note {
            out["note"] = json!(n);
        }
        Ok(out)
    }
}

/// Runs `work` and marks the discussion as no longer driven when it ends, by
/// returning or by panicking. Without the guard a panic in the driver would
/// leave the discussion reported as running until the process restarts: a
/// resume would answer 409, its state would be wrong and a stop could not
/// claim it.
fn run_to_end(done: Arc<AtomicBool>, work: impl FnOnce()) {
    let _ended = Claim(done);
    work();
}

/// Runs `driver` to its end on a thread and records it as live.
fn spawn(
    live: &mut BTreeMap<String, Live>,
    id: String,
    driver: super::Driver,
    dir: &std::path::Path,
) -> Result<(), String> {
    let done = Arc::new(AtomicBool::new(false));
    let log = DiscussionLog::at(dir, &id)?;
    let flag = Arc::clone(&done);
    std::thread::Builder::new()
        .name(format!("discussion-{id}"))
        .spawn(move || {
            run_to_end(flag, || {
                let mut out = driver.run();
                // A stop recorded while a call was in flight makes that step
                // fail; the ledger then only needs closing.
                for _ in 0..3 {
                    let finishing =
                        matches!(log.state().map(|s| s.next()), Ok(Next::Finish { .. }));
                    if out.is_err() && finishing {
                        out = driver.run();
                    } else {
                        break;
                    }
                }
                // An error leaves the ledger resumable; the owner reads it as
                // `interrupted` and continues it.
            });
        })
        .map_err(|e| format!("cannot start the discussion's thread: {e}"))?;
    live.insert(id, Live { done });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A discussion that a `stop` is closing counts as running, so a `resume`
    /// or a second `stop` that arrives meanwhile is refused instead of driving
    /// the same ledger next to it.
    #[test]
    fn a_stop_that_closes_a_discussion_holds_it_as_running() {
        let owner = Owner::default();
        assert!(!owner.running("d-1"));
        let first = owner.claim("d-1").expect("nobody drives it yet");
        assert!(owner.running("d-1"), "the closing stop is the driver");
        assert!(owner.claim("d-1").is_none(), "a second driver is refused");
        assert_eq!(owner.state("d-1", false), "running");
        // Another discussion is not affected.
        assert!(owner.claim("d-2").is_some());
        drop(first);
        assert!(!owner.running("d-1"));
        assert_eq!(owner.state("d-1", false), "interrupted");
        assert!(owner.claim("d-1").is_some(), "free again once it ended");
    }

    /// The thread of a running discussion marks it as ended when its work
    /// returns and when it panics, so the owner never reads a dead thread as a
    /// running discussion.
    #[test]
    fn a_discussion_thread_that_panics_is_not_left_running() {
        for panics in [false, true] {
            let done = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&done);
            let handle = std::thread::spawn(move || {
                run_to_end(flag, || {
                    if panics {
                        panic!("the driver failed");
                    }
                });
            });
            assert_eq!(handle.join().is_err(), panics);
            assert!(done.load(Ordering::SeqCst), "panics={panics}");
        }
    }

    /// A panic while closing must not leave the discussion running forever.
    #[test]
    fn a_claim_is_released_when_the_closing_panics() {
        let owner = Owner::default();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _driving = owner.claim("d-3").unwrap();
            panic!("the close failed");
        }));
        assert!(r.is_err());
        assert!(!owner.running("d-3"));
    }

    /// Two threads asking at once: exactly one gets the discussion.
    #[test]
    fn two_claims_at_once_give_the_discussion_to_one() {
        let owner = Arc::new(Owner::default());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let wins: usize = (0..8)
            .map(|_| {
                let (owner, barrier) = (Arc::clone(&owner), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    let claim = owner.claim("d-4");
                    let won = claim.is_some();
                    // Hold it until every thread has asked.
                    barrier.wait();
                    drop(claim);
                    usize::from(won)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .sum();
        assert_eq!(wins, 1);
    }
}
