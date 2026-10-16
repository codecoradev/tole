//! Host wiring for sibling change notices (#380).
//!
//! A child agent (spawned by `agent_start`) finds its identity and the
//! shared touch log in its environment. When present, its file tools are
//! wrapped so every successful access is recorded, and a watcher thread
//! folds the shared log and queues a notice into the run's soft-interrupt
//! queue when a sibling changes a file this child read or wrote. Outside a
//! child (no env) everything here is a no-op.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tole_core::cancel::CancelToken;
use tole_core::sibling_watch::{SiblingWatcher, Tracked, TouchRecorder, ENV_AGENT_ID, ENV_TOUCH_LOG};
use tole_core::tool::Tool;

/// How often the watcher folds the shared log. Notices are only ever
/// delivered at the turn loop's next safe point, so this bounds latency,
/// not correctness.
pub const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// A child's identity and the shared log it reports to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    pub agent: String,
    pub log: PathBuf,
}

/// Read the child identity from the environment. `None` when this is not
/// a tracked child, or when the id is not a well-formed agent id (never
/// trusted blindly: it ends up in the shared log).
pub fn tracking_from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<Tracking> {
    let agent = env(ENV_AGENT_ID)?.trim().to_string();
    let log = env(ENV_TOUCH_LOG)?.trim().to_string();
    if !tole_core::agents::valid_agent_id(&agent) || log.is_empty() {
        return None;
    }
    Some(Tracking {
        agent,
        log: PathBuf::from(log),
    })
}

/// Wrap a file tool so its successful accesses are recorded; identity when
/// not tracking.
pub fn track(tool: Box<dyn Tool>, tracking: Option<&Tracking>, root: &Path) -> Box<dyn Tool> {
    match tracking {
        Some(t) => Box::new(Tracked::new(
            tool,
            TouchRecorder::new(t.agent.clone(), t.log.clone(), root),
        )),
        None => tool,
    }
}

/// Background watcher; stops when dropped.
pub struct Watcher {
    stop: Arc<AtomicBool>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Start folding the shared log, queueing a notice into `cancel`'s
/// soft-interrupt queue for every sibling change. A full queue drops the
/// notice (it is informational); a read error is reported once.
pub fn start_watcher(tracking: &Tracking, cancel: &CancelToken, every: Duration) -> Watcher {
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, token) = (Arc::clone(&stop), cancel.clone());
    let mut watcher = SiblingWatcher::new(tracking.agent.clone(), tracking.log.clone());
    std::thread::spawn(move || {
        let mut reported = false;
        while !flag.load(Ordering::SeqCst) {
            match watcher.poll() {
                Ok(notices) => {
                    for n in notices {
                        let _ = token.interrupts().push_notice(&n.text());
                    }
                }
                Err(e) if !reported => {
                    reported = true;
                    eprintln!("tole: sibling tracking: cannot read the touch log: {e}");
                }
                Err(_) => {}
            }
            std::thread::sleep(every);
        }
    });
    Watcher { stop }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::sibling_watch::Op;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn tracking_needs_a_well_formed_id_and_a_log() {
        let ok = env_of(&[(ENV_AGENT_ID, "a-1f-2-0"), (ENV_TOUCH_LOG, "/x/touch.log")]);
        assert_eq!(
            tracking_from_env(&ok),
            Some(Tracking {
                agent: "a-1f-2-0".into(),
                log: "/x/touch.log".into()
            })
        );
        assert_eq!(tracking_from_env(&env_of(&[])), None);
        assert_eq!(
            tracking_from_env(&env_of(&[(ENV_AGENT_ID, "a-1")])),
            None,
            "no log"
        );
        for bad in ["", "A-1", "a b", "a\n[x]", "../x"] {
            let e = env_of(&[(ENV_AGENT_ID, bad), (ENV_TOUCH_LOG, "/x")]);
            assert_eq!(tracking_from_env(&e), None, "{bad:?}");
        }
    }

    #[test]
    fn the_watcher_turns_a_sibling_write_into_a_queued_notice() {
        let dir = std::env::temp_dir().join(format!("tole-sibwire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = dir.join("tole-agents").join("touch.log");
        let mine = Tracking {
            agent: "a-1".into(),
            log: log.clone(),
        };
        let cancel = CancelToken::default();
        let _w = start_watcher(&mine, &cancel, Duration::from_millis(20));

        let me = TouchRecorder::new("a-1", &log, "/ws");
        let sib = TouchRecorder::new("a-2", &log, "/ws");
        me.record(Op::Read, "src/lib.rs").unwrap();
        sib.record(Op::Write, "src/lib.rs").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            got = cancel.interrupts().drain();
        }
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].source, tole_core::interrupt::Source::Notice);
        assert!(got[0].text.contains("child agent a-2"), "{}", got[0].text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn track_is_the_identity_outside_a_child() {
        struct T;
        impl Tool for T {
            fn name(&self) -> &str {
                "read_file"
            }
            fn risk(&self) -> tole_core::tool::Risk {
                tole_core::tool::Risk::ReadOnly
            }
            fn execute(&self, i: serde_json::Value) -> Result<serde_json::Value, String> {
                Ok(i)
            }
        }
        let t = track(Box::new(T), None, Path::new("/ws"));
        assert_eq!(t.name(), "read_file");
        assert_eq!(t.execute(serde_json::json!({"path": "x"})).unwrap()["path"], "x");
    }
}
