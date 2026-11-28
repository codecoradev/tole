//! `GET /ready` (#1017): is this serve usable, not only alive. Three checks, each true when it
//! does not apply: the state directories take a write (cached, so a flood costs one probe per
//! directory per `ttl`), the cron ticker has beaten recently when `--cron` was asked for, the
//! webhook threads still run when a destination is configured.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The routes that answer without a credential: liveness, and readiness when none is sent (a
/// credential that is sent is checked like on every route, and earns the `checks`).
pub fn is_probe(path: &str, credential: bool) -> bool {
    path == "/health" || (path == "/ready" && !credential)
}

/// A state probe that takes longer than this counts as a failure (a hung mount).
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// Must take a write: the data dir, the cron dir when used, `--sessions-dir`. A missing one
    /// is probed at its nearest existing ancestor and never created.
    pub state_dirs: Vec<PathBuf>,
    /// `--cron`: the cron dir whose ticker heartbeat (`tole cron status`) must be ours and fresh.
    pub cron_dir: Option<PathBuf>,
    pub helpers: Helpers,
    /// How long a state check is reused (zero: never).
    pub ttl: Duration,
    /// The write probe of one directory; `None` is [`writable`]. A seam for tests.
    pub probe: Option<fn(&Path) -> bool>,
    pub memo: Memo,
}

/// The last state check, and whether a probe thread is still out (a hung mount).
#[derive(Default)]
pub struct Memo {
    last: Mutex<Option<(Instant, bool)>>,
    out: Arc<AtomicBool>,
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
            ttl: Duration::from_secs(5),
            ..Default::default()
        }
    }

    /// The answer at `now` (Unix seconds): `200` when every check holds, else `503`. `detail`
    /// (the caller sent a valid credential) adds one boolean per check; nobody gets a path or an
    /// error text.
    pub fn answer(&self, now: i64, detail: bool) -> (u16, Value) {
        let state = self.state_ok();
        let cron = self.cron_dir.as_deref().is_none_or(|d| ticking(d, now));
        let webhook = self
            .helpers
            .all()
            .iter()
            .all(|t| !t.0 || !t.1.is_finished());
        let ok = state && cron && webhook;
        let mut body = json!({"status": if ok { "ok" } else { "not ready" }});
        if detail {
            body["checks"] = json!({"state": state, "cron": cron, "webhook": webhook});
        }
        (if ok { 200 } else { 503 }, body)
    }

    /// One caller probes, the others wait for it and share its answer; a probe runs on its own
    /// thread so a hung mount costs a request at most [`PROBE_TIMEOUT`], and a new probe is not
    /// started while the stuck one is out (it counts as a failure).
    fn state_ok(&self) -> bool {
        let mut last = self
            .memo
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((at, ok)) = *last {
            if at.elapsed() < self.ttl {
                return ok;
            }
        }
        let ok = !self.memo.out.swap(true, Relaxed) && {
            let (dirs, probe, out) = (self.state_dirs.clone(), self.probe, self.memo.out.clone());
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let probe = probe.unwrap_or(writable);
                let ok = dirs.iter().all(|d| probe(d));
                out.store(false, Relaxed);
                let _ = tx.send(ok);
            });
            rx.recv_timeout(PROBE_TIMEOUT).unwrap_or(false)
        };
        *last = Some((Instant::now(), ok));
        ok
    }
}

/// A file can be created and removed in `dir`, or in its nearest existing ancestor when it is
/// missing (nothing is created). A name taken by a leftover of a killed probe (a restart as
/// pid 1 repeats the pid) is skipped for the next one.
fn writable(dir: &Path) -> bool {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = dir.ancestors().find(|d| d.exists()).unwrap_or(dir);
    for _ in 0..8 {
        let probe = dir.join(format!(
            ".ready-probe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => return std::fs::remove_file(&probe).is_ok(),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return false,
        }
    }
    false
}

/// This process's ticker beat within the stale window `tole cron status` uses.
fn ticking(dir: &Path, now: i64) -> bool {
    use crate::cron::status::{last_beat, STALE_SECS};
    let beat = last_beat(dir);
    beat.is_some_and(|b| {
        b.host == "serve" && b.pid == std::process::id() && now - b.at <= STALE_SECS
    })
}
