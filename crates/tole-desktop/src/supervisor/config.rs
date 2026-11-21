//! What the supervisor is told: where, with which binary and how patiently.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// File name of the socket in the runtime directory.
pub const SOCKET_NAME: &str = "serve.sock";
/// File name of the desktop's own launch lock in the runtime directory.
pub const LAUNCH_LOCK_NAME: &str = "desktop.lock";

/// What closing the window does to the serve. Owner decision D3: the serve
/// keeps running by default, so automations and turns in progress continue;
/// "quit completely" stops it. The owner can change this in Settings (the
/// default of that setting is still to be confirmed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseBehavior {
    #[default]
    LeaveRunning,
    StopServe,
}

/// How a serve that stopped by itself is started again: wait `initial`, then
/// twice as long each time up to `max`, give up after `max_attempts` starts in
/// a row. A serve that ran for `stable_after` before it stopped counts as a
/// fresh start: its failures are not a crash loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub max_attempts: u32,
    pub stable_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            max_attempts: 6,
            stable_after: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The wait before restart number `attempt` (1 is the first).
    pub fn delay(&self, attempt: u32) -> Duration {
        let doublings = attempt.saturating_sub(1).min(31);
        self.initial
            .checked_mul(1u32 << doublings)
            .map_or(self.max, |d| d.min(self.max))
    }

    /// Whether `attempt` is past the last one allowed.
    pub fn gives_up(&self, attempt: u32) -> bool {
        attempt > self.max_attempts
    }

    /// The attempt count after a serve that had `attempts` failed starts in a
    /// row ran for `ran_for` and stopped: reset by a steady run.
    pub fn after_run(&self, attempts: u32, ran_for: Duration) -> u32 {
        if ran_for >= self.stable_after {
            0
        } else {
            attempts
        }
    }
}

/// Everything the supervisor needs to attach to or start a serve.
#[derive(Debug, Clone)]
pub struct Config {
    /// The folder the serve works in (`--workspace`). One serve has one root;
    /// one serve per folder or one rooted at a parent folder is still an open
    /// owner decision (D8), and either works here: give each serve its own
    /// `runtime_dir`.
    pub workspace: PathBuf,
    /// The private directory: the socket, its lock, the launch lock and the
    /// credential file. Created `0700`; refused when it is not the owner's alone.
    pub runtime_dir: PathBuf,
    /// A `tole` binary chosen by the owner, in Settings.
    pub binary: Option<PathBuf>,
    /// Start the serve with `--cron`, so automations run in it. Off by default:
    /// exactly one serve must run the scheduler (D8).
    pub cron: bool,
    pub close_behavior: CloseBehavior,
    pub backoff: Backoff,
    /// How long a started serve has to answer `/health`.
    pub startup_timeout: Duration,
    /// How long to wait for another launch of the desktop that is deciding.
    pub launch_wait: Duration,
    /// How long a serve has to stop after `SIGTERM` before `SIGKILL`.
    pub stop_timeout: Duration,
    /// How often the monitor looks at the serve.
    pub poll_interval: Duration,
    /// `PATH` and the desktop's directory are read from the process unless
    /// these are set (tests, and an embedder with its own rules).
    pub path_override: Option<OsString>,
}

impl Config {
    pub fn new(workspace: impl Into<PathBuf>, runtime_dir: impl Into<PathBuf>) -> Self {
        Config {
            workspace: workspace.into(),
            runtime_dir: runtime_dir.into(),
            binary: None,
            cron: false,
            close_behavior: CloseBehavior::default(),
            backoff: Backoff::default(),
            startup_timeout: Duration::from_secs(15),
            launch_wait: Duration::from_secs(30),
            stop_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_secs(2),
            path_override: None,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join(SOCKET_NAME)
    }

    /// The lock the serve holds for as long as it lives (`<socket>.lock`).
    pub fn serve_lock(&self) -> PathBuf {
        self.runtime_dir.join(format!("{SOCKET_NAME}.lock"))
    }

    pub fn launch_lock(&self) -> PathBuf {
        self.runtime_dir.join(LAUNCH_LOCK_NAME)
    }
}

/// The runtime directory when the owner sets none: a directory of this user
/// under `$XDG_RUNTIME_DIR` (a per-user directory the system keeps private)
/// when that is an absolute path, else under the system temporary directory
/// with the user id in its name. A directory that already exists there and is
/// not the owner's alone is refused, never used.
pub fn default_runtime_dir(xdg_runtime_dir: Option<&Path>, temp_dir: &Path, uid: u32) -> PathBuf {
    match xdg_runtime_dir.filter(|p| p.is_absolute()) {
        Some(xdg) => xdg.join("tole-desktop"),
        None => temp_dir.join(format!("tole-desktop-{uid}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn the_delay_doubles_from_the_first_and_stops_at_the_maximum() {
        let b = Backoff {
            initial: secs(1),
            max: secs(10),
            max_attempts: 8,
            stable_after: secs(60),
        };
        let delays: Vec<_> = (1..=7).map(|n| b.delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 10, 10, 10]);
        // No overflow however high the count goes.
        assert_eq!(b.delay(u32::MAX), secs(10));
        assert_eq!(b.delay(0), secs(1));
    }

    #[test]
    fn the_number_of_starts_is_bounded() {
        let b = Backoff::default();
        assert!(!b.gives_up(b.max_attempts));
        assert!(b.gives_up(b.max_attempts + 1));
    }

    #[test]
    fn a_steady_run_resets_the_count_and_a_short_one_does_not() {
        let b = Backoff::default();
        assert_eq!(b.after_run(4, b.stable_after), 0);
        assert_eq!(b.after_run(4, b.stable_after + secs(1)), 0);
        assert_eq!(b.after_run(4, b.stable_after - Duration::from_millis(1)), 4);
        assert_eq!(b.after_run(0, secs(0)), 0);
    }

    #[test]
    fn closing_the_window_leaves_the_serve_running_by_default() {
        assert_eq!(CloseBehavior::default(), CloseBehavior::LeaveRunning);
        let c = Config::new("/w", "/run/x");
        assert_eq!(c.close_behavior, CloseBehavior::LeaveRunning);
        assert!(!c.cron);
        assert_eq!(c.socket(), PathBuf::from("/run/x/serve.sock"));
        assert_eq!(c.serve_lock(), PathBuf::from("/run/x/serve.sock.lock"));
    }

    #[test]
    fn the_runtime_dir_prefers_an_absolute_xdg_dir_and_never_a_relative_one() {
        let tmp = Path::new("/tmp");
        assert_eq!(
            default_runtime_dir(Some(Path::new("/run/user/1000")), tmp, 1000),
            PathBuf::from("/run/user/1000/tole-desktop")
        );
        for xdg in [None, Some(Path::new("relative/dir")), Some(Path::new(""))] {
            assert_eq!(
                default_runtime_dir(xdg, tmp, 501),
                PathBuf::from("/tmp/tole-desktop-501")
            );
        }
    }
}
