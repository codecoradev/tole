//! A scripted serve for the app's tests: no socket, no process.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tole_client::{
    Approval, ApprovalList, CancelReply, CronJobState, CronJobs, CronRuns, Decision, DecisionReply,
    EntriesPage, Error, InterruptReply, InterruptRequest, PromptReply, PromptRequest,
    SessionStatus, SkillsState, ToolsState,
};
use tole_desktop_model::ServeState;

use super::launch::{CloseOutcome, Launcher, Serve};
use crate::backend::ServeLink;
use crate::driver::api::{Api, EventSource};

/// How long a test waits for something that must happen. Only a broken app
/// reaches it.
pub(super) const PATIENCE: Duration = Duration::from_secs(20);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Poll `cond` until it holds, or fail the test after [`PATIENCE`].
pub(super) fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + PATIENCE;
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A door a test holds shut: whoever waits at it is blocked until `open`.
#[derive(Default)]
pub(super) struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    arrived: AtomicUsize,
}

impl Gate {
    pub(super) fn new() -> Arc<Gate> {
        Arc::default()
    }

    pub(super) fn open(&self) {
        *lock(&self.open) = true;
        self.changed.notify_all();
    }

    /// Block until opened (bounded by [`PATIENCE`], so a bug fails a test).
    pub(super) fn pass(&self) {
        self.arrived.fetch_add(1, Ordering::SeqCst);
        let guard = lock(&self.open);
        let _ = self
            .changed
            .wait_timeout_while(guard, PATIENCE, |open| !*open)
            .unwrap_or_else(|p| p.into_inner());
    }

    /// How many have reached the gate.
    pub(super) fn arrivals(&self) -> usize {
        self.arrived.load(Ordering::SeqCst)
    }
}

/// What the scripted serve answers on `GET /approvals`, `/skills`, `/tools`.
#[derive(Clone, Copy)]
pub(super) struct Counts {
    pub approvals: usize,
    pub drafts: usize,
    pub untrusted_tools: usize,
}

struct Scripted(Counts);

fn refused() -> Error {
    Error::Io(io::Error::from(io::ErrorKind::ConnectionRefused))
}

struct Silent;

impl EventSource for Silent {
    fn next_item(&mut self) -> Result<Option<tole_client::StreamItem>, Error> {
        Ok(None)
    }
}

impl Api for Scripted {
    fn approvals(&self, _: Option<&str>) -> Result<ApprovalList, Error> {
        let rows: Vec<Value> = (0..self.0.approvals)
            .map(|i| {
                json!({
                    "id": format!("a{i}"), "sessionId": "s1", "tool": "write_file",
                    "description": "path: a.txt", "status": "pending",
                    "createdAtMs": 1_700_000_000_000u64, "expiresInSeconds": 900,
                    "summary": {"kind": "create_file", "target": "a.txt",
                                "effect": "adds", "insideWorkspace": true}
                })
            })
            .collect();
        Ok(serde_json::from_value(json!({ "approvals": rows })).expect("a list"))
    }

    fn skills(&self) -> Result<SkillsState, Error> {
        let drafts: Vec<Value> = (0..self.0.drafts)
            .map(|i| {
                json!({
                    "id": format!("d{i}"), "name": format!("skill {i}"), "description": "How to",
                    "created_at": "2026-11-17", "hash": "sha256:cd", "tainted": false,
                    "warnings": [], "nearest": null
                })
            })
            .collect();
        Ok(serde_json::from_value(
            json!({"skills": [], "drafts": drafts, "archived": [], "notes": []}),
        )
        .expect("a skills answer"))
    }

    fn tools(&self) -> Result<ToolsState, Error> {
        let servers: Vec<Value> = (0..self.0.untrusted_tools)
            .map(|i| {
                json!({
                    "trust": "not_trusted", "name": format!("t{i}"), "state": "unapproved",
                    "tier": "user", "source": null, "version": null, "approved_at": null,
                    "secrets": [], "sandbox": "none", "manifest_hash": null, "lock": "-"
                })
            })
            .collect();
        Ok(
            serde_json::from_value(json!({"servers": servers, "notes": [], "probed": false}))
                .expect("a tools answer"),
        )
    }

    fn decide_row(&self, _: &Approval, _: Decision) -> Result<DecisionReply, Error> {
        Err(refused())
    }
    fn cron_jobs(&self) -> Result<CronJobs, Error> {
        Err(refused())
    }
    fn cron_runs(&self, _: &str, _: usize) -> Result<CronRuns, Error> {
        Err(refused())
    }
    fn pause_cron_job(&self, _: &str) -> Result<CronJobState, Error> {
        Err(refused())
    }
    fn resume_cron_job(&self, _: &str) -> Result<CronJobState, Error> {
        Err(refused())
    }
    fn follow_events(&self, _: &str, _: u64) -> Box<dyn EventSource> {
        Box::new(Silent)
    }
    fn entries(&self, _: &str, _: u64, _: Option<usize>) -> Result<EntriesPage, Error> {
        Err(refused())
    }
    fn status(&self, _: &str) -> Result<SessionStatus, Error> {
        Err(refused())
    }
    fn prompt(&self, _: &str, _: &PromptRequest) -> Result<PromptReply, Error> {
        Err(refused())
    }
    fn cancel(&self, _: &str) -> Result<CancelReply, Error> {
        Err(refused())
    }
    fn interrupt(&self, _: &str, _: &InterruptRequest) -> Result<InterruptReply, Error> {
        Err(refused())
    }
    fn decide(&self, _: &str, _: Decision) -> Result<DecisionReply, Error> {
        Err(refused())
    }
}

/// A serve the test drives by hand.
pub(super) struct FakeServe {
    state: Mutex<ServeState>,
    epoch: AtomicU64,
    counts: Mutex<Option<Counts>>,
    pub retries: AtomicUsize,
    pub restarts: AtomicUsize,
    pub closes: AtomicUsize,
    /// `retry` and `restart_serve` wait here, when set.
    pub command_gate: Mutex<Option<Arc<Gate>>>,
    /// `close_window` waits here, when set.
    pub close_gate: Mutex<Option<Arc<Gate>>>,
    pub dropped: Arc<AtomicBool>,
    /// What `close_window` reports.
    pub close_outcome: Mutex<CloseOutcome>,
}

pub(super) fn running() -> ServeState {
    ServeState::Running {
        version: "1.0".into(),
    }
}

impl FakeServe {
    pub(super) fn new(state: ServeState, counts: Option<Counts>) -> Arc<FakeServe> {
        Arc::new(FakeServe {
            state: Mutex::new(state),
            epoch: AtomicU64::new(1),
            counts: Mutex::new(counts),
            retries: AtomicUsize::new(0),
            restarts: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            command_gate: Mutex::new(None),
            close_gate: Mutex::new(None),
            dropped: Arc::new(AtomicBool::new(false)),
            close_outcome: Mutex::new(CloseOutcome::Closed),
        })
    }

    /// The serve is now `state`; every change is a new epoch, as the real
    /// supervisor's observer makes it.
    pub(super) fn set(&self, state: ServeState) {
        *lock(&self.state) = state;
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn hold(gate: &Mutex<Option<Arc<Gate>>>) {
        let gate = lock(gate).clone();
        if let Some(gate) = gate {
            gate.pass();
        }
    }
}

impl Drop for FakeServe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl ServeLink for FakeServe {
    fn state(&self) -> ServeState {
        lock(&self.state).clone()
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn api(&self) -> Option<Arc<dyn Api>> {
        let counts = (*lock(&self.counts))?;
        Some(Arc::new(Scripted(counts)))
    }
}

impl Serve for FakeServe {
    fn retry(&self) {
        self.retries.fetch_add(1, Ordering::SeqCst);
        FakeServe::hold(&self.command_gate);
    }

    fn restart_serve(&self) {
        self.restarts.fetch_add(1, Ordering::SeqCst);
        FakeServe::hold(&self.command_gate);
    }

    fn close_window(self: Arc<Self>) -> CloseOutcome {
        self.closes.fetch_add(1, Ordering::SeqCst);
        FakeServe::hold(&self.close_gate);
        *lock(&self.close_outcome)
    }
}

/// A launcher that hands out `serve` after waiting at `gate`, if there is one.
pub(super) fn launcher_of(serve: Arc<FakeServe>, gate: Option<Arc<Gate>>) -> Launcher {
    Arc::new(move || {
        if let Some(gate) = &gate {
            gate.pass();
        }
        let serve: Arc<dyn Serve> = serve.clone();
        serve
    })
}
