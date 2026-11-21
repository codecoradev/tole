//! The lifecycle of the supervisor against an in-process stand-in for the
//! serve (`fake`) and shell scripts for a binary that fails. The real `tole
//! serve` is exercised by `crates/tole-cli/tests/desktop_supervisor.rs`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tole_desktop_model::{ServeFailure, ServeState};

use super::fake::{FakeServe, TestDir};
use super::probe::EXPECTED_PROTOCOL;
use super::token::{self, Token};
use super::{Backoff, Config, StopError, Supervisor};

const TOKEN: &str = "launch-token-aaaaaaaaaaaaaaaa";

/// A test configuration: a runtime directory and workspace inside `dir`, short
/// waits, and a `tole` that must not run unless the test says it may.
fn config(dir: &TestDir) -> (Config, PathBuf) {
    let workspace = dir.path("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut cfg = Config::new(&workspace, dir.path("run"));
    cfg.startup_timeout = Duration::from_secs(2);
    cfg.launch_wait = Duration::from_secs(2);
    cfg.stop_timeout = Duration::from_millis(500);
    cfg.poll_interval = Duration::from_millis(20);
    cfg.backoff = Backoff {
        initial: Duration::from_millis(30),
        max: Duration::from_millis(100),
        max_attempts: 3,
        stable_after: Duration::from_secs(3600),
    };
    cfg.path_override = Some("".into());
    let marker = dir.path("tole-ran");
    cfg.binary = Some(script(
        dir,
        "tole",
        &format!("touch {}\necho boom >&2\nexit 7", marker.display()),
    ));
    (cfg, marker)
}

/// An executable shell script `name` in `dir/bin`.
fn script(dir: &TestDir, name: &str, body: &str) -> PathBuf {
    let bin = dir.path("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// What a first launch leaves for a second one: the credential file.
fn save_token(runtime: &Path, text: &str) {
    token::write(runtime, &Token::from_text(text).unwrap()).unwrap();
}

fn wait_for(what: &str, sup: &Supervisor, done: impl Fn(&ServeState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if done(&sup.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "timed out waiting for {what}; the state is {:?}",
        sup.state()
    );
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
fn a_running_serve_is_attached_to_and_no_second_one_is_started() {
    let dir = TestDir::new("attach");
    let (cfg, marker) = config(&dir);
    let _serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "9.9.9");
    save_token(&cfg.runtime_dir, TOKEN);

    let (sup, log) = launch_logged(cfg);
    assert_eq!(
        sup.state(),
        ServeState::Attached {
            version: "9.9.9".into()
        }
    );
    assert_eq!(
        states(&log),
        [
            ServeState::Starting,
            ServeState::Attached {
                version: "9.9.9".into()
            }
        ]
    );
    let client = sup.client().expect("a client once attached");
    assert_eq!(client.health().unwrap().version.as_deref(), Some("9.9.9"));
    assert!(
        client.sessions().unwrap().sessions.is_empty(),
        "the credential is accepted"
    );
    assert!(!marker.exists(), "the tole binary was not started");
}

#[test]
fn a_second_launch_attaches_to_the_same_serve() {
    let dir = TestDir::new("second");
    let (cfg, marker) = config(&dir);
    let _serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "9.9.9");
    save_token(&cfg.runtime_dir, TOKEN);

    let first = Supervisor::launch(cfg.clone());
    let second = Supervisor::launch(cfg);
    for sup in [&first, &second] {
        assert!(
            matches!(sup.state(), ServeState::Attached { .. }),
            "{:?}",
            sup.state()
        );
        assert!(sup.client().unwrap().sessions().is_ok());
    }
    assert!(!marker.exists());
}

#[test]
fn a_serve_with_another_protocol_is_refused_and_never_talked_to() {
    for found in [Some(EXPECTED_PROTOCOL + 1), Some(0), None] {
        let dir = TestDir::new("proto");
        let (cfg, marker) = config(&dir);
        let _serve = FakeServe::start(&cfg.runtime_dir, TOKEN, found, "0.1.0");
        save_token(&cfg.runtime_dir, TOKEN);
        let sup = Supervisor::launch(cfg);
        assert_eq!(
            sup.state(),
            ServeState::Failed(ServeFailure::VersionMismatch {
                found,
                expected: EXPECTED_PROTOCOL
            })
        );
        assert!(sup.client().is_none(), "no client for a serve we refuse");
        assert!(
            !marker.exists(),
            "it is not replaced behind the owner's back"
        );
    }
}

#[test]
fn a_serve_that_does_not_take_our_credential_is_not_used() {
    let dir = TestDir::new("cred");
    let (cfg, marker) = config(&dir);
    let _serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "1.0.0");
    let unavailable = ServeState::Failed(ServeFailure::CredentialUnavailable);

    // No credential file: started by something else.
    assert_eq!(Supervisor::launch(cfg.clone()).state(), unavailable);
    // A credential the serve rejects.
    save_token(&cfg.runtime_dir, "some-other-token");
    assert_eq!(Supervisor::launch(cfg.clone()).state(), unavailable);
    // The right one, in a file other users can read: not trusted.
    save_token(&cfg.runtime_dir, TOKEN);
    let file = token::token_path(&cfg.runtime_dir);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(Supervisor::launch(cfg).state(), unavailable);
    assert!(!marker.exists());
}

#[test]
fn a_socket_directory_that_is_not_owner_only_is_refused_and_left_alone() {
    let dir = TestDir::new("wide");
    let (cfg, marker) = config(&dir);
    for mode in [0o755, 0o770, 0o701] {
        std::fs::create_dir_all(&cfg.runtime_dir).unwrap();
        std::fs::set_permissions(&cfg.runtime_dir, std::fs::Permissions::from_mode(mode)).unwrap();
        let sup = Supervisor::launch(cfg.clone());
        match sup.state() {
            ServeState::Failed(ServeFailure::DirectoryNotPrivate { reason, .. }) => {
                assert!(
                    reason.contains("0700") && reason.contains(&format!("{mode:04o}")),
                    "{reason}"
                );
            }
            other => panic!("{mode:o}: {other:?}"),
        }
        let now = std::fs::metadata(&cfg.runtime_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            now, mode,
            "the directory is not chmod-ed behind the owner's back"
        );
        assert!(!cfg.socket().exists() && !marker.exists());
    }
}

#[test]
fn a_directory_widened_while_a_serve_runs_stops_the_desktop_from_sending_anything() {
    let dir = TestDir::new("widened");
    let (cfg, marker) = config(&dir);
    let _serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "1.0.0");
    save_token(&cfg.runtime_dir, TOKEN);
    std::fs::set_permissions(&cfg.runtime_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sup = Supervisor::launch(cfg);
    assert!(matches!(
        sup.state(),
        ServeState::Failed(ServeFailure::DirectoryNotPrivate { .. })
    ));
    assert!(sup.client().is_none() && !marker.exists());
}

#[test]
fn no_tole_binary_is_a_clear_state_not_a_hang() {
    let dir = TestDir::new("nobin");
    let (mut cfg, _) = config(&dir);
    let failed = ServeState::Failed(ServeFailure::BinaryNotFound);
    cfg.binary = Some(dir.path("missing/tole"));
    assert_eq!(Supervisor::launch(cfg.clone()).state(), failed);
    cfg.binary = Some(PathBuf::from("tole")); // relative: never used
    assert_eq!(Supervisor::launch(cfg.clone()).state(), failed);
    cfg.binary = None; // nothing next to the test binary, an empty PATH
    assert_eq!(Supervisor::launch(cfg).state(), failed);
}

#[test]
fn a_start_that_fails_says_why_and_is_not_retried_on_launch() {
    let _turn = super::fake::spawn_guard();
    let dir = TestDir::new("fail");
    let (cfg, marker) = config(&dir);
    let (sup, log) = launch_logged(cfg);
    match sup.state() {
        ServeState::Failed(ServeFailure::StartFailed { detail }) => {
            assert!(
                detail.contains("exited with code 7") && detail.contains("boom"),
                "{detail}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(marker.exists(), "the binary was started once");
    assert_eq!(
        states(&log).len(),
        2,
        "Starting, then Failed: {:?}",
        states(&log)
    );
    assert!(sup.client().is_none());
}

#[test]
fn a_serve_that_dies_is_restarted_with_growing_waits_and_then_given_up_on() {
    let _turn = super::fake::spawn_guard();
    let dir = TestDir::new("crash");
    let (cfg, _) = config(&dir);
    let mut serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "1.0.0");
    save_token(&cfg.runtime_dir, TOKEN);
    let (sup, log) = launch_logged(cfg);
    assert!(sup.state().is_ready());

    serve.stop(); // it dies; every start after this fails
    wait_for("the supervisor to give up", &sup, |s| {
        matches!(s, ServeState::Failed(ServeFailure::GaveUp { .. }))
    });

    let seen = log.lock().unwrap().clone();
    let restarts: Vec<(Instant, u32, Duration)> = seen
        .iter()
        .filter_map(|(at, s)| match s {
            ServeState::Restarting { attempt, retry_in } => Some((*at, *attempt, *retry_in)),
            _ => None,
        })
        .collect();
    let waits: Vec<_> = restarts
        .iter()
        .map(|(_, n, d)| (*n, d.as_millis()))
        .collect();
    assert_eq!(
        waits,
        [(1, 30), (2, 60), (3, 100)],
        "doubling, capped at the maximum"
    );
    // Each next state came after the wait it announced, not before.
    for pair in restarts.windows(2) {
        let (earlier, later) = (&pair[0], &pair[1]);
        assert!(later.0.duration_since(earlier.0) >= earlier.2, "{pair:?}");
    }
    match seen.last().map(|(_, s)| s) {
        Some(ServeState::Failed(ServeFailure::GaveUp {
            attempts: 3,
            detail,
        })) => {
            assert!(detail.contains("boom"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
    assert!(sup.client().is_none());
}

#[test]
fn a_serve_that_comes_back_on_its_own_is_attached_to_without_starting_another() {
    let dir = TestDir::new("back");
    let (mut cfg, marker) = config(&dir);
    cfg.backoff.initial = Duration::from_millis(300);
    cfg.backoff.max = Duration::from_millis(300);
    let mut serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "1.0.0");
    save_token(&cfg.runtime_dir, TOKEN);
    let runtime = cfg.runtime_dir.clone();
    let sup = Supervisor::launch(cfg);

    serve.stop();
    drop(serve);
    wait_for("the restart wait", &sup, |s| {
        matches!(s, ServeState::Restarting { .. })
    });
    // Another desktop (or the owner) starts one before the wait is over.
    let _again = FakeServe::start_over(&runtime, "second-launch-token-bbbbbbbbbb", "2.0.0");
    save_token(&runtime, "second-launch-token-bbbbbbbbbb");
    wait_for("the new serve", &sup, |s| {
        matches!(s, ServeState::Attached { .. })
    });
    assert_eq!(
        sup.state(),
        ServeState::Attached {
            version: "2.0.0".into()
        }
    );
    assert!(!marker.exists(), "no second serve was started");
}

#[test]
fn closing_the_window_leaves_the_serve_running_by_default() {
    let dir = TestDir::new("close");
    let (cfg, _) = config(&dir);
    let _serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "1.0.0");
    save_token(&cfg.runtime_dir, TOKEN);
    let socket = cfg.socket();
    let sup = Supervisor::launch(cfg.clone());
    assert!(sup.close_window().is_ok());
    assert!(matches!(
        super::probe::probe(&socket),
        super::probe::Probe::Serve { .. }
    ));
    // Dropping a supervisor (the window going away) does the same.
    drop(Supervisor::launch(cfg));
    assert!(matches!(
        super::probe::probe(&socket),
        super::probe::Probe::Serve { .. }
    ));
}

#[test]
fn quit_completely_with_nothing_running_cleans_up_and_reports_stopped() {
    let dir = TestDir::new("quit");
    let (cfg, _) = config(&dir);
    // What a killed serve leaves: a socket file nobody listens on, a credential.
    drop(FakeServe::start(
        &cfg.runtime_dir,
        TOKEN,
        Some(EXPECTED_PROTOCOL),
        "1.0.0",
    ));
    save_token(&cfg.runtime_dir, TOKEN);
    assert!(cfg.socket().exists());

    let log: Log = Arc::default();
    let sink = Arc::clone(&log);
    let mut cfg2 = cfg.clone();
    cfg2.binary = Some(dir.path("missing"));
    let sup = Supervisor::launch_observed(cfg2, move |s| {
        sink.lock().unwrap().push((Instant::now(), s.clone()));
    });
    assert_eq!(
        sup.state(),
        ServeState::Failed(ServeFailure::BinaryNotFound)
    );
    sup.quit_completely().unwrap();
    assert_eq!(states(&log).last(), Some(&ServeState::Stopped));
    assert!(!cfg.socket().exists(), "the stale socket is removed");
    assert!(
        token::read(&cfg.runtime_dir).unwrap().is_none(),
        "the credential is removed"
    );
}

#[test]
fn the_desktop_never_signals_itself_when_the_serve_is_in_its_own_process() {
    let dir = TestDir::new("self");
    let (cfg, _) = config(&dir);
    let _serve = FakeServe::start(&cfg.runtime_dir, TOKEN, Some(EXPECTED_PROTOCOL), "1.0.0");
    save_token(&cfg.runtime_dir, TOKEN);
    let sup = Supervisor::launch(cfg);
    let result = sup.quit_completely();
    // (Reaching the next line at all is the test: the process was not killed.)
    assert!(matches!(result, Err(StopError::Signal(_))), "{result:?}");
}
