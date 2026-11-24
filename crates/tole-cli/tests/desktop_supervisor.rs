//! #1029: the desktop's serve supervisor against the REAL `tole serve` binary.
//!
//! The supervisor lives in `tole-desktop` (a thin client that never depends on
//! this crate), so its end-to-end test sits here, where cargo builds the `tole`
//! binary. Every serve is started through a wrapper script that makes it
//! hermetic (its own HOME, a provider mock, no sibling tools on PATH) and
//! counts how often it was started, so "a second launch never starts a second
//! serve" is a number, not a hope.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349); the local
// socket is a Unix feature (the Windows named pipe is a follow-up).
#![cfg(all(feature = "shell-tools", unix))]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tole_client::{NewSession, PromptRequest};
use tole_desktop::supervisor::probe::{self, Probe, EXPECTED_PROTOCOL};
use tole_desktop::supervisor::process::Serve;
use tole_desktop::supervisor::token::{self, Token};
use tole_desktop::supervisor::{sys, Backoff, Config, ServeFailure, ServeState, Supervisor};

mod common;
mod serve_harness;
use common::{wait_until, write_executable, TestDir};

const WAIT: Duration = Duration::from_secs(30);

/// A hermetic world for one test: directories, the wrapper script, and a
/// guard that stops any serve the test leaves behind.
struct World {
    cfg: Config,
    starts: PathBuf,
    fail: PathBuf,
    wrapper: PathBuf,
    // Dropped after `cfg` users but before `dir`: the serve is gone before
    // its directory is removed.
    _reaper: Reaper,
    _dir: TestDir,
}

struct Reaper(PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Some(pid) = sys::socket_owner_pid(&self.0) {
            let _ = sys::kill(pid);
        }
    }
}

fn world(provider: &str) -> World {
    // A short path: a socket address holds about 100 bytes.
    let dir = TestDir::in_system_temp("ds");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let starts = dir.join("starts");
    let fail = dir.join("fail");
    let wrapper = dir.join("tole");
    let path = common::path_without_sibling_binaries();
    write_executable(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             echo started >> '{starts}'\n\
             if [ -e '{fail}' ]; then echo 'boom: scripted failure' >&2; exit 9; fi\n\
             export HOME='{home}' CODECORA_HOME='{home}' PATH='{path}'\n\
             export TOLE_BASE_URL='{provider}' TOLE_MODEL=mock-model TOLE_API_KEY=sk-test\n\
             unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME TOLE_SERVE_TOKEN\n\
             exec '{tole}' \"$@\"\n",
            starts = starts.display(),
            fail = fail.display(),
            home = home.display(),
            path = path.to_string_lossy(),
            tole = env!("CARGO_BIN_EXE_tole"),
        ),
    );
    let mut cfg = Config::new(dir.join("ws"), dir.join("run"));
    cfg.binary = Some(wrapper.clone());
    cfg.startup_timeout = WAIT;
    cfg.launch_wait = WAIT;
    cfg.stop_timeout = Duration::from_secs(10);
    cfg.poll_interval = Duration::from_millis(25);
    cfg.backoff = Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
        max_attempts: 3,
        stable_after: Duration::from_secs(3600),
    };
    World {
        _reaper: Reaper(cfg.socket()),
        cfg,
        starts,
        fail,
        wrapper,
        _dir: dir,
    }
}

impl World {
    fn started(&self) -> usize {
        std::fs::read_to_string(&self.starts).map_or(0, |s| s.lines().count())
    }

    fn pid(&self) -> u32 {
        sys::socket_owner_pid(&self.cfg.socket()).expect("a serve listens")
    }

    fn token(&self) -> String {
        token::read(&self.cfg.runtime_dir)
            .unwrap()
            .expect("a credential file")
            .expose()
            .to_owned()
    }
}

fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn running() -> ServeState {
    ServeState::Running { version: version() }
}

fn attached() -> ServeState {
    ServeState::Attached { version: version() }
}

fn wait_state(what: &str, sup: &Supervisor, done: impl Fn(&ServeState) -> bool) {
    wait_until(WAIT, what, || done(&sup.state()));
}

#[cfg(target_os = "linux")]
fn is_running(pid: u32) -> bool {
    // A zombie (a child nobody waited for) has stopped running.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| rest.starts_with('Z')))
        .is_some_and(|zombie| !zombie)
}

type Log = Arc<Mutex<Vec<(Instant, ServeState)>>>;

fn launch_logged(cfg: Config) -> (Supervisor, Log) {
    let log: Log = Arc::default();
    let sink = Arc::clone(&log);
    let sup = Supervisor::launch_observed(cfg, move |s| {
        sink.lock().unwrap().push((Instant::now(), s.clone()));
    });
    (sup, log)
}

fn states(log: &Log) -> Vec<ServeState> {
    log.lock().unwrap().iter().map(|(_, s)| s.clone()).collect()
}

#[test]
fn launch_starts_a_serve_with_a_private_credential_and_quit_completely_stops_it() {
    let w = world("http://127.0.0.1:9/v1");
    let (sup, log) = launch_logged(w.cfg.clone());
    assert_eq!(sup.state(), running());
    assert_eq!(states(&log), [ServeState::Starting, running()]);
    assert_eq!(w.started(), 1);

    // The protocol the desktop speaks is the one the binary speaks.
    let client = sup.client().expect("a client once running");
    let health = client.health().unwrap();
    assert_eq!(health.protocol, Some(EXPECTED_PROTOCOL));
    assert_eq!(
        EXPECTED_PROTOCOL,
        tole_cli::protocol::SERVE_PROTOCOL_VERSION
    );
    assert_eq!(health.version, Some(version()));
    // The credential works on an authenticated route.
    let id = client
        .create_session(&NewSession::default())
        .unwrap()
        .session_id;
    assert!(client
        .sessions()
        .unwrap()
        .sessions
        .iter()
        .any(|s| s.id == id));

    // The boundary is the filesystem: directory, socket and credential file.
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&w.cfg.runtime_dir), 0o700);
    assert_eq!(mode(&w.cfg.socket()), 0o600);
    assert_eq!(mode(&token::token_path(&w.cfg.runtime_dir)), 0o600);

    let pid = w.pid();
    {
        // The credential is not in the arguments, on macOS and Linux alike
        // (`ps` shows them on both; `-ww` keeps a long line whole).
        let secret = w.token();
        let ps = std::process::Command::new("ps")
            .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .expect("ps runs");
        assert!(ps.status.success(), "ps did not find the serve");
        let shown = String::from_utf8_lossy(&ps.stdout);
        assert!(
            !shown.contains(&secret),
            "the credential leaked into the arguments"
        );
        assert!(shown.contains("--token-fd"), "{shown}");
    }
    #[cfg(target_os = "linux")]
    {
        // The credential is in neither the arguments nor the environment.
        let secret = w.token();
        let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        let has = |hay: &[u8]| hay.windows(secret.len()).any(|w| w == secret.as_bytes());
        assert!(!has(&args), "the credential leaked into the arguments");
        // `tole` marks itself non-dumpable (process_guard), so another user,
        // or this user without privilege, cannot read its environment at all:
        // that refusal is the protection, and CI runs unprivileged. Where the
        // read is allowed (a privileged test run) the credential must not be
        // in it. The unit test of `Serve::spawn` checks the environment of
        // the child with a stand-in binary that is dumpable.
        match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(env) => assert!(!has(&env), "the credential leaked into the environment"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}"),
        }
        assert!(String::from_utf8_lossy(&args).contains("--token-fd"));
        assert!(is_running(pid));
    }

    sup.quit_completely().unwrap();
    assert_eq!(probe::probe(&w.cfg.socket()), Probe::Absent);
    assert!(!w.cfg.socket().exists(), "the socket is removed");
    assert!(token::read(&w.cfg.runtime_dir).unwrap().is_none());
    assert!(sys::serve_lock_is_free(&w.cfg.serve_lock()).unwrap());
    #[cfg(target_os = "linux")]
    assert!(!is_running(pid), "the serve process is gone");
}

#[test]
fn a_second_launch_attaches_and_never_starts_a_second_serve() {
    let w = world("http://127.0.0.1:9/v1");
    let first = Supervisor::launch(w.cfg.clone());
    assert_eq!(first.state(), running());
    let pid = w.pid();

    let second = Supervisor::launch(w.cfg.clone());
    assert_eq!(second.state(), attached());
    assert_eq!(w.started(), 1, "the binary was started once");
    assert_eq!(w.pid(), pid, "one serve process");
    for sup in [&first, &second] {
        let client = sup.client().unwrap();
        client.create_session(&NewSession::default()).unwrap();
    }

    // Closing the windows leaves the serve running.
    assert!(first.close_window().is_ok());
    assert!(second.close_window().is_ok());
    assert!(matches!(probe::probe(&w.cfg.socket()), Probe::Serve { .. }));

    // A later launch (another process, in effect) still attaches.
    let third = Supervisor::launch(w.cfg.clone());
    assert_eq!(third.state(), attached());
    assert_eq!(w.started(), 1);
    // "Quit completely" stops a serve that this launch did not start.
    third.quit_completely().unwrap();
    assert!(sys::serve_lock_is_free(&w.cfg.serve_lock()).unwrap());
    assert_eq!(probe::probe(&w.cfg.socket()), Probe::Absent);
}

#[test]
fn two_launches_at_once_start_one_serve() {
    let w = world("http://127.0.0.1:9/v1");
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let cfg = w.cfg.clone();
            std::thread::spawn(move || {
                let sup = Supervisor::launch(cfg);
                (sup.state(), sup)
            })
        })
        .collect();
    let launched: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let mut seen: Vec<_> = launched.iter().map(|(s, _)| format!("{s:?}")).collect();
    seen.sort();
    let mut want = vec![format!("{:?}", running()), format!("{:?}", attached())];
    want.sort();
    assert_eq!(seen, want, "one started it, the other attached");
    assert_eq!(w.started(), 1);
}

#[test]
fn the_serve_lock_refuses_a_second_serve_on_the_same_socket() {
    let w = world("http://127.0.0.1:9/v1");
    let sup = Supervisor::launch(w.cfg.clone());
    assert_eq!(sup.state(), running());

    // What a launch that raced would do, without the desktop's own launch lock.
    let token = Token::generate().unwrap();
    let mut second =
        Serve::spawn(&w.wrapper, &w.cfg.socket(), &w.cfg.workspace, false, &token).unwrap();
    let status = loop {
        if let Some(status) = second.try_wait().unwrap() {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success(), "a second serve must not run");
    std::thread::sleep(Duration::from_millis(100));
    let said = second.last_lines();
    assert!(said.contains("already"), "{said}");
    assert_eq!(w.started(), 2, "it was tried, and refused");
    // The first is undisturbed and still ours.
    assert_eq!(sup.state(), running());
    assert!(sup.client().unwrap().health().is_ok());
}

#[test]
fn killing_the_serve_restarts_it_and_the_session_resumes_from_the_ledger() {
    // A prompt that mentions "slow" is held by the provider for a minute.
    let w = world(&serve_harness::spawn_mock(60_000));
    let (sup, log) = launch_logged(w.cfg.clone());
    assert_eq!(sup.state(), running());
    let old_token = w.token();
    let old_pid = w.pid();

    let client = sup.client().unwrap();
    let id = client
        .create_session(&NewSession::default())
        .unwrap()
        .session_id;
    let reply = client.prompt(&id, &PromptRequest::text("hello")).unwrap();
    assert_eq!(reply.text.as_deref(), Some("ENTRIES-ANSWER"));
    let before = client.entries(&id, 0, None).unwrap();
    assert!(before.entries.len() >= 2, "{before:?}");

    // A turn is running when the serve is killed.
    let busy = {
        let (client, id) = (client.clone(), id.clone());
        std::thread::spawn(move || client.prompt(&id, &PromptRequest::text("slow turn")))
    };
    wait_until(WAIT, "the turn to reach the ledger", || {
        client
            .entries(&id, 0, None)
            .is_ok_and(|p| p.entries.len() > before.entries.len())
    });
    sys::kill(old_pid).unwrap();
    assert!(busy.join().unwrap().is_err(), "the turn was cut off");

    // The state shows the stop, then the serve is back (the log has every state,
    // so a restart that is quicker than a poll is not missed).
    wait_until(WAIT, "the stop to be noticed", || {
        states(&log)
            .iter()
            .any(|s| matches!(s, ServeState::Restarting { .. }))
    });
    wait_state("the restart", &sup, |s| *s == running());
    let seen = states(&log);
    let restarting = seen
        .iter()
        .position(|s| matches!(s, ServeState::Restarting { attempt: 1, .. }))
        .expect("the state shows the restart");
    assert!(
        restarting > 1 && seen.last() == Some(&running()),
        "{seen:?}"
    );
    assert_ne!(w.pid(), old_pid, "a new process");
    assert_ne!(w.token(), old_token, "a new credential for every start");
    assert_eq!(w.started(), 2);

    // The ledger survived: the same session opens again, from its file.
    let client = sup.client().unwrap();
    let resumed = client.resume_session(&id).unwrap();
    assert_eq!(resumed.session_id, id);
    assert_eq!(resumed.resumed, Some(true));
    let after = client.entries(&id, 0, None).unwrap();
    let seqs = |p: &tole_client::EntriesPage| p.entries.iter().map(|e| e.seq).collect::<Vec<_>>();
    assert!(
        seqs(&after).starts_with(&seqs(&before)),
        "{:?} then {:?}",
        seqs(&before),
        seqs(&after)
    );
}

#[test]
fn a_serve_that_keeps_dying_is_given_up_on_and_retry_starts_it_again() {
    let w = world("http://127.0.0.1:9/v1");
    let (sup, log) = launch_logged(w.cfg.clone());
    assert_eq!(sup.state(), running());

    std::fs::write(&w.fail, "").unwrap(); // every start from now on fails
    sys::kill(w.pid()).unwrap();
    wait_state("giving up", &sup, |s| {
        matches!(s, ServeState::Failed(ServeFailure::GaveUp { .. }))
    });
    let seen = log.lock().unwrap().clone();
    let waits: Vec<_> = seen
        .iter()
        .filter_map(|(_, s)| match s {
            ServeState::Restarting { attempt, retry_in } => Some((*attempt, retry_in.as_millis())),
            _ => None,
        })
        .collect();
    assert_eq!(waits, [(1, 50), (2, 100), (3, 200)]);
    match seen.last().map(|(_, s)| s) {
        Some(ServeState::Failed(ServeFailure::GaveUp {
            attempts: 3,
            detail,
        })) => {
            assert!(detail.contains("boom"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(w.started(), 1 + 3, "the first start and three restarts");
    assert!(sup.client().is_none());

    // The owner fixes the cause and presses "Try again": the count starts over.
    std::fs::remove_file(&w.fail).unwrap();
    sup.retry();
    assert_eq!(sup.state(), running());
    assert!(sup.client().unwrap().health().is_ok());
}

#[test]
fn a_serve_started_by_something_else_is_replaced_on_request() {
    let w = world("http://127.0.0.1:9/v1");
    // Started the way a terminal user would: no credential file for it.
    let mut foreign = Serve::spawn(
        &w.wrapper,
        &w.cfg.socket(),
        &w.cfg.workspace,
        false,
        &Token::generate().unwrap(),
    )
    .unwrap();
    wait_until(WAIT, "the foreign serve to listen", || {
        matches!(probe::probe(&w.cfg.socket()), Probe::Serve { .. })
    });

    let sup = Supervisor::launch(w.cfg.clone());
    assert_eq!(
        sup.state(),
        ServeState::Failed(ServeFailure::CredentialUnavailable),
        "the desktop cannot sign in to it, and says so"
    );
    assert!(sup.client().is_none());
    assert_eq!(w.started(), 1, "it did not start a second one");

    sup.restart_serve();
    assert_eq!(sup.state(), running());
    assert_eq!(w.started(), 2);
    // A serve asked to stop with SIGTERM drains and exits cleanly (#1014), so
    // the proof is that it exited, not how.
    let status = common::poll_until(WAIT, || foreign.try_wait().ok().flatten());
    assert!(status.is_some(), "the old serve was stopped");
    assert!(sup.client().unwrap().sessions().is_ok());
}

#[test]
fn the_cron_flag_is_passed_only_when_asked() {
    for cron in [false, true] {
        let mut w = world("http://127.0.0.1:9/v1");
        w.cfg.cron = cron;
        let sup = Supervisor::launch(w.cfg.clone());
        assert_eq!(sup.state(), running());
        #[cfg(target_os = "linux")]
        {
            let args = std::fs::read(format!("/proc/{}/cmdline", w.pid())).unwrap();
            let has_cron = String::from_utf8_lossy(&args).contains("--cron");
            assert_eq!(has_cron, cron);
        }
        sup.quit_completely().unwrap();
    }
}
