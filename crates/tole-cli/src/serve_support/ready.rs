//! `GET /ready` (#1017): is this serve usable, not only alive. Three checks, each true when it
//! does not apply: the state directories take a write, the cron ticker has beaten recently
//! when `--cron` was asked for, the webhook threads still run when a destination is configured.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

/// The routes that need no credential: liveness and readiness.
pub fn is_probe(path: &str) -> bool {
    matches!(path, "/health" | "/ready")
}

/// Every helper thread of a serve, tagged when it carries webhooks (the drain reads them too).
#[derive(Default)]
pub struct Helpers(Mutex<Vec<(bool, JoinHandle<()>)>>);

impl Helpers {
    pub fn add(&self, webhook: bool, handle: JoinHandle<()>) {
        self.all().push((webhook, handle));
    }

    pub fn running(&self) -> usize {
        self.all().iter().filter(|t| !t.1.is_finished()).count()
    }

    fn all(&self) -> MutexGuard<'_, Vec<(bool, JoinHandle<()>)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What `GET /ready` looks at, fixed at start-up.
#[derive(Default)]
pub struct Readiness {
    /// Must take a write: the data dir, the cron dir when used, `--sessions-dir`.
    pub state_dirs: Vec<PathBuf>,
    /// `--cron`: the cron dir whose ticker heartbeat (`tole cron status`) must be ours and fresh.
    pub cron_dir: Option<PathBuf>,
    pub helpers: Helpers,
}

impl Readiness {
    pub fn new(sessions_dir: Option<&Path>, cron: bool, webhooks: bool) -> Self {
        let cron_dir = crate::cron::default_dir().ok();
        let mut state_dirs: Vec<PathBuf> = tole_core::paths::tole_data_dir().into_iter().collect();
        state_dirs.extend(cron_dir.clone().filter(|_| cron || webhooks));
        state_dirs.extend(sessions_dir.map(Path::to_path_buf));
        let cron_dir = cron_dir.filter(|_| cron);
        Readiness {
            state_dirs,
            cron_dir,
            helpers: Helpers::default(),
        }
    }

    /// The answer at `now` (Unix seconds): `200` when every check holds, else `503`. One boolean
    /// per check, never a path or an error text.
    pub fn answer(&self, now: i64) -> (u16, Value) {
        let state = self.state_dirs.iter().all(|d| writable(d));
        let cron = self.cron_dir.as_deref().is_none_or(|d| ticking(d, now));
        let webhook = self
            .helpers
            .all()
            .iter()
            .all(|t| !t.0 || !t.1.is_finished());
        let ok = state && cron && webhook;
        let checks = json!({"state": state, "cron": cron, "webhook": webhook});
        let status = if ok { "ok" } else { "not ready" };
        (
            if ok { 200 } else { 503 },
            json!({"status": status, "checks": checks}),
        )
    }
}

/// A file can be created and removed in `dir` (made if missing). The name is unique per call:
/// two probes at once must not remove each other's file.
fn writable(dir: &Path) -> bool {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Relaxed);
    let probe = dir.join(format!(".ready-probe-{}-{n}", std::process::id()));
    let made = std::fs::create_dir_all(dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
        })
        .is_ok();
    made && std::fs::remove_file(&probe).is_ok()
}

/// This process's ticker beat within the stale window `tole cron status` uses.
fn ticking(dir: &Path, now: i64) -> bool {
    use crate::cron::status::{last_beat, STALE_SECS};
    let beat = last_beat(dir);
    beat.is_some_and(|b| {
        b.host == "serve" && b.pid == std::process::id() && now - b.at <= STALE_SECS
    })
}
