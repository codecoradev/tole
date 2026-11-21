//! Graceful shutdown of the long-running hosts, `tole serve` (REST and
//! `--transport mcp`) (#1014).
//!
//! SIGTERM and SIGINT only count themselves: the handler does one atomic add and
//! nothing else (no allocation, no lock, no I/O), so it is async-signal-safe.
//! Everything else happens on ordinary threads that poll [`requested`]: the
//! accept loop wakes up every few tens of milliseconds, the cron ticker and the
//! webhook senders sleep in short slices ([`sleep_unless`]), and the drain loop of
//! each face detaches running turns through the session host's existing detach
//! path, then waits at most the grace period ([`Countdown`]).
//!
//! A second signal while draining ends the wait at once ([`Ending::SecondSignal`]);
//! the process then exits non-zero without waiting for anything. Without any
//! signal nothing here changes behaviour: the handlers are installed, the counter
//! stays at zero.
//!
//! Unix only. On other platforms [`install`] does nothing and [`requested`] is
//! always false, so a Ctrl+C there ends the process as it did before.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// How long a drain waits before it gives up and exits anyway.
pub const DEFAULT_GRACE_SECS: u64 = 20;
/// The environment fallback of `--shutdown-grace` (whole seconds).
pub const GRACE_ENV: &str = "TOLE_SERVE_SHUTDOWN_GRACE";
/// The longest grace accepted: a typo of days must not look like a hang.
pub const MAX_GRACE_SECS: u64 = 3600;
/// How often a waiting loop looks at the flags: the longest a signal waits to be
/// noticed, and the longest a stopped helper thread keeps sleeping.
pub const POLL: Duration = Duration::from_millis(50);

/// Signals received so far. Written by the handler only.
static SIGNALS: AtomicU32 = AtomicU32::new(0);

#[cfg(unix)]
extern "C" fn on_signal(_sig: libc::c_int) {
    // One atomic read-modify-write: nothing else is safe to do here.
    SIGNALS.fetch_add(1, Ordering::SeqCst);
}

/// Install the SIGTERM and SIGINT handlers. Call once, before the listener is
/// reported ready, so a signal that arrives right after the "listening" line is
/// already counted.
#[cfg(unix)]
pub fn install() -> std::io::Result<()> {
    // SAFETY: `sigaction` is given a zeroed struct with a valid handler and an
    // emptied mask; the handler is `extern "C"` and async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        // A blocking read or write on another thread resumes instead of failing
        // with EINTR; the polling loops notice the flag on their own.
        action.sa_flags = libc::SA_RESTART;
        for sig in [libc::SIGTERM, libc::SIGINT] {
            if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn install() -> std::io::Result<()> {
    Ok(())
}

/// True once a shutdown signal has arrived.
pub fn requested() -> bool {
    SIGNALS.load(Ordering::SeqCst) > 0
}

/// True once a second signal arrived: the operator does not want to wait.
pub fn forced() -> bool {
    SIGNALS.load(Ordering::SeqCst) > 1
}

/// Sleep for `total` in short slices and stop early when `stop` is set. Defined
/// next to the webhook senders, a lower level that uses it too (the layering test
/// forbids them importing this module).
pub use crate::webhook_out::sleep_unless;

/// The grace period: `--shutdown-grace` wins, then [`GRACE_ENV`] (a blank value
/// counts as unset), then [`DEFAULT_GRACE_SECS`]. `0` does not wait at all.
pub fn grace(flag: Option<u64>, env: Option<String>) -> Result<Duration, String> {
    let secs = match (
        flag,
        env.as_deref().map(str::trim).filter(|v| !v.is_empty()),
    ) {
        (Some(n), _) => n,
        (None, Some(v)) => v.parse::<u64>().map_err(|_| {
            format!("{GRACE_ENV} must be a whole number of seconds (0 to {MAX_GRACE_SECS})")
        })?,
        (None, None) => DEFAULT_GRACE_SECS,
    };
    if secs > MAX_GRACE_SECS {
        return Err(format!(
            "the shutdown grace is at most {MAX_GRACE_SECS} seconds (got {secs})"
        ));
    }
    Ok(Duration::from_secs(secs))
}

/// How a drain ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ending {
    /// Nothing was left running: exit code 0.
    Clean,
    /// The grace period ran out with work still running.
    GraceExpired,
    /// A second signal asked to stop waiting.
    SecondSignal,
}

impl Ending {
    /// `Ok` for a clean drain; otherwise an error naming what was left, which the
    /// binary turns into exit code 1. `left` is only called for an unclean end.
    pub fn into_result(
        self,
        who: &str,
        grace: Duration,
        left: impl FnOnce() -> String,
    ) -> anyhow::Result<()> {
        match self {
            Ending::Clean => Ok(()),
            Ending::GraceExpired => Err(anyhow::anyhow!(
                "{who}: the {}s shutdown grace ran out with {} still running; exiting anyway \
                 (sessions stay resumable)",
                grace.as_secs(),
                left()
            )),
            Ending::SecondSignal => Err(anyhow::anyhow!(
                "{who}: a second signal arrived; exiting without waiting for {} \
                 (sessions stay resumable)",
                left()
            )),
        }
    }
}

/// The clock of one drain.
pub struct Countdown {
    started: Instant,
    grace: Duration,
}

impl Countdown {
    pub fn start(grace: Duration) -> Self {
        Countdown {
            started: Instant::now(),
            grace,
        }
    }

    /// `None` while the drain should go on. A drain that is finished wins over a
    /// second signal, and a second signal over the clock.
    pub fn check(&self, drained: bool) -> Option<Ending> {
        if drained {
            Some(Ending::Clean)
        } else if forced() {
            Some(Ending::SecondSignal)
        } else if self.started.elapsed() >= self.grace {
            Some(Ending::GraceExpired)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn the_grace_comes_from_the_flag_then_the_environment_then_the_default() {
        let secs = |f, e: Option<&str>| grace(f, e.map(String::from)).map(|d| d.as_secs());
        assert_eq!(secs(None, None), Ok(DEFAULT_GRACE_SECS));
        assert_eq!(secs(None, Some("7")), Ok(7));
        assert_eq!(secs(None, Some(" 7 ")), Ok(7));
        assert_eq!(secs(None, Some("  ")), Ok(DEFAULT_GRACE_SECS));
        assert_eq!(secs(Some(3), Some("7")), Ok(3), "the flag wins");
        assert_eq!(secs(Some(0), None), Ok(0), "zero means do not wait");
    }

    #[test]
    fn a_bad_grace_is_refused_by_name() {
        let e = grace(None, Some("soon".into())).unwrap_err();
        assert!(e.contains(GRACE_ENV), "{e}");
        let e = grace(Some(MAX_GRACE_SECS + 1), None).unwrap_err();
        assert!(e.contains("at most"), "{e}");
        assert!(grace(None, Some("-1".into())).is_err());
    }

    #[test]
    fn a_drain_ends_clean_when_done_and_on_the_clock_otherwise() {
        let c = Countdown::start(Duration::from_secs(3600));
        assert_eq!(c.check(false), None);
        assert_eq!(c.check(true), Some(Ending::Clean));
        let c = Countdown::start(Duration::ZERO);
        assert_eq!(c.check(false), Some(Ending::GraceExpired));
        assert_eq!(c.check(true), Some(Ending::Clean), "done beats the clock");
    }

    #[test]
    fn only_an_unclean_end_is_an_error_and_it_names_what_was_left() {
        let g = Duration::from_secs(20);
        assert!(Ending::Clean
            .into_result("tole serve", g, || unreachable!())
            .is_ok());
        let e = Ending::GraceExpired
            .into_result("tole serve", g, || "2 turn(s)".into())
            .unwrap_err()
            .to_string();
        assert!(e.contains("20s") && e.contains("2 turn(s)"), "{e}");
        let e = Ending::SecondSignal
            .into_result("tole serve", g, || "1 turn(s)".into())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("second signal") && e.contains("1 turn(s)"),
            "{e}"
        );
    }

    #[test]
    fn a_sleep_returns_as_soon_as_the_stop_flag_is_set() {
        let stop = AtomicBool::new(true);
        let started = Instant::now();
        sleep_unless(&stop, Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(5));
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        sleep_unless(&stop, Duration::from_millis(120));
        assert!(started.elapsed() >= Duration::from_millis(100));
    }
}
