//! Ticker liveness (#579, P5: "tasks just don't run"): every pass writes
//! `<dir>/ticker.json` (pid, process start time, host, time). `tole cron
//! status` and `tole doctor` read it next to the job counts; a stale
//! ticker while jobs are active is a doctor failure.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tole_core::ledger::Lease;

use super::store;

/// A ticker older than this is stale (cron(8) fires every minute; the
/// serve ticker passes every minute).
pub const STALE_SECS: i64 = 180;

/// The last pass of a ticker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub pid: u32,
    pub start_time: u64,
    /// `serve` (in-process ticker) or `tick` (`tole cron tick`).
    pub host: String,
    pub at: i64,
}

fn path(dir: &Path) -> std::path::PathBuf {
    dir.join("ticker.json")
}

/// Records a pass (best effort: liveness reporting must never stop a tick).
pub fn beat(dir: &Path, now: i64, serve: bool) {
    let lease = store::own_lease();
    let hb = Heartbeat {
        pid: lease.pid,
        start_time: lease.start_time,
        host: if serve { "serve" } else { "tick" }.into(),
        at: now,
    };
    let tmp = dir.join(format!("ticker.json.{}", lease.pid));
    let ok = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, serde_json::to_vec(&hb).unwrap_or_default()))
        .and_then(|()| std::fs::rename(&tmp, path(dir)));
    if ok.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The last recorded pass, if any.
pub fn last_beat(dir: &Path) -> Option<Heartbeat> {
    serde_json::from_slice(&std::fs::read(path(dir)).ok()?).ok()
}

/// `tole cron status`.
#[derive(Debug, Clone)]
pub struct CronStatus {
    pub ticker: Option<Heartbeat>,
    /// Seconds since the last pass.
    pub age: Option<i64>,
    pub alive: bool,
    pub active: usize,
    pub paused: usize,
    pub running: usize,
    pub blocked: usize,
    pub missed: u64,
    pub tokens_today: u64,
}

/// Reads the store and the heartbeat at `now`.
pub fn status(dir: &Path, now: i64) -> anyhow::Result<CronStatus> {
    let l = store::open_read(dir)?;
    let s = l.state();
    let live: Vec<_> = s.jobs().filter(|j| !j.job.removed).collect();
    let ticker = last_beat(dir);
    let age = ticker.as_ref().map(|t| now - t.at);
    let alive = ticker.as_ref().is_some_and(|t| {
        now - t.at <= STALE_SECS
            && (t.host != "serve"
                || store::lease_alive(&Lease {
                    pid: t.pid,
                    start_time: t.start_time,
                }))
    });
    Ok(CronStatus {
        alive,
        age,
        ticker,
        active: live.iter().filter(|j| !j.job.paused).count(),
        paused: live.iter().filter(|j| j.job.paused).count(),
        running: s.pending().count(),
        blocked: s.blocked().count(),
        missed: s.missed().map(|m| m.count).sum(),
        tokens_today: super::tick::tokens_since(s, super::tick::day_start(now)),
    })
}

fn ago(secs: i64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7_200 => format!("{}m", s / 60),
        s => format!("{}h", s / 3_600),
    }
}

/// One line about the ticker.
pub fn ticker_line(st: &CronStatus) -> String {
    match (&st.ticker, st.age) {
        (Some(t), Some(age)) => format!(
            "ticker {} ({} pid {}, last tick {} ago)",
            if st.alive { "alive" } else { "stale" },
            t.host,
            t.pid,
            ago(age)
        ),
        _ => "no tick has run yet".into(),
    }
}

/// `tole doctor`'s cron section: `(lines, ok)`. Not ok when jobs are
/// active and no ticker is alive.
pub fn doctor(dir: &Path, now: i64) -> (Vec<String>, bool) {
    if !store::ledger_path(dir).exists() {
        return (vec!["  ✓ cron: no jobs".into()], true);
    }
    let st = match status(dir, now) {
        Ok(s) => s,
        Err(e) => {
            return (
                vec![format!("  ✗ cron: cannot read the job store: {e:#}")],
                false,
            )
        }
    };
    let mut lines = Vec::new();
    let ok = st.active == 0 || st.alive;
    if ok {
        lines.push(format!(
            "  ✓ cron: {} active job(s), {}",
            st.active,
            ticker_line(&st)
        ));
    } else {
        lines.push(format!(
            "  ✗ cron: {} active job(s) but {}",
            st.active,
            ticker_line(&st)
        ));
        lines.push(
            "    fix: run `tole cron tick` every minute (crontab: `* * * * * tole cron tick`) or `tole serve --cron`"
                .into(),
        );
    }
    if st.blocked > 0 || st.missed > 0 {
        lines.push(format!(
            "  ! cron: {} blocked and {} missed occurrence(s) (see `tole cron list`)",
            st.blocked, st.missed
        ));
    }
    (lines, ok)
}
