//! The owner's discussions run in the background (#1042): what `tole serve`
//! needs to start, continue and stop a discussion without blocking a request.
//!
//! Nothing here decides what a discussion may do. A start validates and
//! records through [`discuss::begin`] and a continue through
//! [`discuss::prepare_resume`], the same functions `tole discuss start` and
//! `resume` run, so the contract rules, the participant rules (profiles of the
//! owner's config, never a built-in role), the hard token budget, the depth
//! (an owner-started discussion is depth 0 with inline, read-only voices and
//! no tools) and the background-file checks are the CLI's. This module adds
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
        self.lock()
            .get(id)
            .is_some_and(|l| !l.done.load(Ordering::SeqCst))
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
        if live.get(id).is_some_and(|l| !l.done.load(Ordering::SeqCst)) {
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
        if st.closed.is_none() && !self.running(id) {
            // A profile that is gone cannot be seated again; the stop is
            // recorded all the same and a later resume closes it.
            note = super::prepare_resume(id, env)
                .and_then(|r| r.driver.run())
                .err()
                .map(|e| env.secrets.redact(&e).into_owned());
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
            let mut out = driver.run();
            // A stop recorded while a call was in flight makes that step
            // fail; the ledger then only needs closing.
            for _ in 0..3 {
                let finishing = matches!(log.state().map(|s| s.next()), Ok(Next::Finish { .. }));
                if out.is_err() && finishing {
                    out = driver.run();
                } else {
                    break;
                }
            }
            // An error leaves the ledger resumable; the owner reads it as
            // `interrupted` and continues it.
            flag.store(true, Ordering::SeqCst);
        })
        .map_err(|e| format!("cannot start the discussion's thread: {e}"))?;
    live.insert(id, Live { done });
    Ok(())
}
