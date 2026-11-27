//! #1017: `GET /ready`, the readiness answer next to `/health`'s liveness. Real serve
//! processes for what a process can show (healthy, a state path that is not a directory,
//! `--cron` with a ticker that cannot record, a webhook destination), and the check itself
//! with real directories, files and threads for what a process cannot be made to do (a ticker
//! or a sender thread that died, a stale beat, a foreign beat).

// The ticker is the cron module's and the harness spawns the real `tole serve` binary
// (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tole_cli::serve_support::ready::{is_probe, Readiness};

mod common;
mod serve_harness;

// A path that does not exist as a directory is made by a real file in its place, so the test
// does not depend on chmod, which a root user ignores.
fn file_in_place_of_dir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("tole-ready-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::write(&p, "not a directory").unwrap();
    p
}

/// Tests that draw probe names from the one counter of the process run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn checks(answer: &(u16, Value)) -> [bool; 3] {
    let c = &answer.1["checks"];
    ["state", "cron", "webhook"].map(|k| c[k].as_bool().unwrap())
}

#[test]
fn probe_routes_are_exactly_health_and_ready() {
    assert!(is_probe("/health", false) && is_probe("/ready", false));
    // A credential that is sent is checked like on every route, so `/ready` is no probe then.
    assert!(is_probe("/health", true) && !is_probe("/ready", true));
    for p in ["/info", "/ready/", "/readyz", "/ready?x", "/sessions", ""] {
        assert!(!is_probe(p, false), "{p}");
    }
}

#[test]
fn state_dir_that_cannot_take_a_write_is_not_ready() {
    let _one = serial();
    let dir = std::env::temp_dir().join(format!("tole-ready-ok-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ready = Readiness {
        state_dirs: vec![dir.clone(), dir.join("missing/yet")],
        ..Default::default()
    };
    let a = ready.answer(0, true);
    assert_eq!((a.0, checks(&a)), (200, [true; 3]));
    assert_eq!(a.1["status"], "ok");
    // The probe file is gone again, and a missing directory is probed at its parent, not made.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();

    // One bad directory among good ones is enough, and nothing names it.
    let file = file_in_place_of_dir("state");
    let ready = Readiness {
        state_dirs: vec![std::env::temp_dir(), file.join("sessions")],
        ..Default::default()
    };
    let a = ready.answer(0, true);
    assert_eq!((a.0, checks(&a)), (503, [false, true, true]));
    assert_eq!(a.1["status"], "not ready");
    assert!(
        !a.1.to_string().contains("tole-ready"),
        "no path in the body"
    );
    std::fs::remove_file(&file).unwrap();
}

#[test]
fn a_leftover_of_a_killed_probe_is_skipped_not_a_failure() {
    // A serve restarted as pid 1 repeats the pid, and the counter starts over: its first names
    // are taken. All names but every eighth are taken here, so the first try of at least every
    // second call collides and the retry has to find the free one.
    let _one = serial();
    let dir = std::env::temp_dir().join(format!("tole-ready-left-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for k in (0..3000).filter(|k| k % 8 != 4) {
        std::fs::write(
            dir.join(format!(".ready-probe-{}-{k}", std::process::id())),
            "",
        )
        .unwrap();
    }
    let ready = Readiness {
        state_dirs: vec![dir.clone()],
        ..Default::default()
    };
    for _ in 0..4 {
        assert_eq!(ready.answer(0, false).0, 200);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn concurrent_probes_do_not_remove_each_others_file() {
    let _one = serial();
    let dir = std::env::temp_dir().join(format!("tole-ready-race-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ready = std::sync::Arc::new(Readiness {
        state_dirs: vec![dir.clone()],
        ..Default::default()
    });
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let ready = std::sync::Arc::clone(&ready);
            std::thread::spawn(move || (0..100).all(|_| ready.answer(0, true).0 == 200))
        })
        .collect();
    for t in threads {
        assert!(t.join().unwrap(), "a probe failed against a healthy dir");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_unauthenticated_answer_has_no_checks_and_an_authenticated_one_has() {
    let _one = serial();
    let file = file_in_place_of_dir("detail");
    let ready = Readiness {
        state_dirs: vec![file.join("s")],
        ..Default::default()
    };
    assert_eq!(
        ready.answer(0, false),
        (503, json!({"status": "not ready"}))
    );
    let (code, body) = ready.answer(0, true);
    assert_eq!(code, 503);
    assert_eq!(
        body["checks"],
        json!({"state": false, "cron": true, "webhook": true})
    );
    assert_eq!(
        Readiness::default().answer(0, false),
        (200, json!({"status": "ok"}))
    );
    std::fs::remove_file(&file).unwrap();
}

static CALLS: AtomicUsize = AtomicUsize::new(0);
fn counted(_: &std::path::Path) -> bool {
    CALLS.fetch_add(1, Ordering::SeqCst);
    true
}

#[test]
fn a_flood_costs_one_probe_per_directory_per_ttl() {
    let ready = Arc::new(Readiness {
        state_dirs: vec!["/a".into(), "/b".into()],
        ttl: Duration::from_millis(400),
        probe: Some(counted),
        ..Default::default()
    });
    let flood = |n: usize| {
        let threads: Vec<_> = (0..n)
            .map(|_| {
                let ready = Arc::clone(&ready);
                std::thread::spawn(move || ready.answer(0, false).0)
            })
            .collect();
        threads.into_iter().all(|t| t.join().unwrap() == 200)
    };
    assert!(flood(64));
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        2,
        "64 requests, one round of probes"
    );
    assert!(flood(64));
    assert_eq!(CALLS.load(Ordering::SeqCst), 2, "still the same round");
    std::thread::sleep(Duration::from_millis(450));
    assert!(flood(64));
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        4,
        "the next round after the ttl"
    );
}

static HUNG: AtomicUsize = AtomicUsize::new(0);
fn hung(_: &std::path::Path) -> bool {
    HUNG.fetch_add(1, Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(4));
    true
}

#[test]
fn a_probe_that_hangs_is_not_ready_after_the_timeout_and_is_not_started_again() {
    let ready = Readiness {
        state_dirs: vec!["/a".into()],
        probe: Some(hung),
        ..Default::default()
    };
    let started = Instant::now();
    assert_eq!(ready.answer(0, false).0, 503);
    assert!(
        started.elapsed() < Duration::from_millis(3500),
        "{:?}",
        started.elapsed()
    );
    // With no cache at all, the stuck probe is still out: no second thread, an answer at once.
    let again = Instant::now();
    assert_eq!(ready.answer(0, false).0, 503);
    assert!(again.elapsed() < Duration::from_millis(500));
    assert_eq!(HUNG.load(Ordering::SeqCst), 1);
}

#[test]
fn cron_is_ready_only_with_this_process_ticker_beat_inside_the_stale_window() {
    use tole_cli::cron::status::{beat, Heartbeat, STALE_SECS};
    let dir = std::env::temp_dir().join(format!("tole-ready-cron-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let now = 1_800_000_000;
    let ready = |cron: bool| Readiness {
        cron_dir: cron.then(|| dir.clone()),
        ..Default::default()
    };
    let cron_ok = |r: &Readiness| checks(&r.answer(now, true))[1];

    assert!(
        cron_ok(&ready(false)),
        "--cron not asked for: nothing to check"
    );
    assert!(!cron_ok(&ready(true)), "asked for, no beat yet");
    beat(&dir, now, true);
    assert!(cron_ok(&ready(true)), "a fresh beat of this process");
    assert!(
        ready(true).answer(now + STALE_SECS, true).0 == 200,
        "the edge is inside"
    );
    assert_eq!(
        ready(true).answer(now + STALE_SECS + 1, true).0,
        503,
        "a stale beat"
    );
    // The ticker is dead and a person reads the same file: it says so as well.
    assert!(!tole_cli::cron::status::status(&dir, now + STALE_SECS + 1)
        .map(|s| s.alive)
        .unwrap_or(true));

    // A beat of `tole cron tick` (not serve) or of another process is not this serve's ticker.
    beat(&dir, now, false);
    assert!(!cron_ok(&ready(true)), "a standalone tick");
    let other = Heartbeat {
        pid: std::process::id() + 1,
        start_time: 0,
        host: "serve".into(),
        at: now,
    };
    std::fs::write(dir.join("ticker.json"), serde_json::to_vec(&other).unwrap()).unwrap();
    assert!(!cron_ok(&ready(true)), "another serve's beat");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_webhook_thread_that_ended_is_not_ready_and_other_threads_do_not_count() {
    let ready = Readiness::default();
    let (stop, parked) = std::sync::mpsc::channel::<()>();
    let sender = std::thread::spawn(move || {
        let _ = parked.recv();
    });
    ready.helpers.add(true, sender);
    assert!(checks(&ready.answer(0, true))[2], "a running sender");
    assert_eq!(ready.helpers.running(), 1);

    // A ticker thread that ended is the ticker check's business, not the webhook one.
    let ticker = std::thread::spawn(|| {});
    ready.helpers.add(false, ticker);
    while ready.helpers.running() > 1 {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(checks(&ready.answer(0, true))[2]);

    // The sender ends (stop is dropped, `recv` fails): not ready, and the check says which.
    drop(stop);
    let until = Instant::now() + Duration::from_secs(5);
    while ready.helpers.running() > 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    let a = ready.answer(0, true);
    assert_eq!((a.0, checks(&a)), (503, [true, true, false]));
}

// ------------------------------------------------------------------ a real serve

mod real {
    use super::*;
    use crate::common::Gate;
    use crate::serve_harness::{home_of, spawn_mock_gated, unique_dir, Serve};

    /// A serve that never calls the provider, over a fresh workspace.
    fn serve(env: &[(&str, &str)], args: &[&str]) -> Serve {
        let dir = unique_dir("tole-ready");
        std::fs::create_dir_all(&dir).unwrap();
        Serve::spawn_in_env_args("http://127.0.0.1:9", dir, true, env, args)
    }

    /// `GET /ready` without a credential: the status code and the status word only.
    fn ready(s: &Serve) -> (u16, Value) {
        s.http_with("GET", "/ready", false, None)
    }

    /// `GET /ready` with the token: the same status, and the checks.
    fn checks(s: &Serve) -> (u16, Value) {
        s.http("GET", "/ready", None)
    }

    fn until_ready(s: &Serve) -> Value {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let (code, _) = ready(s);
            if code == 200 || Instant::now() > until {
                assert_eq!(code, 200, "never ready: {:?}", checks(s));
                return checks(s).1;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn a_healthy_serve_is_ready_without_a_token_and_a_credential_earns_the_checks() {
        let s = serve(&[], &[]);
        assert_eq!(ready(&s), (200, json!({"status": "ok"})));
        let all = json!({"state": true, "cron": true, "webhook": true});
        assert_eq!(checks(&s), (200, json!({"status": "ok", "checks": all})));
        let text = checks(&s).1.to_string();
        assert!(!text.contains(s.dir.to_str().unwrap()) && !text.contains("tole-ready"));
        // `/health` needs no token and `/info` does; a wrong credential on `/ready` is a 401
        // like on any route, never a quiet downgrade.
        assert_eq!(s.http_with("GET", "/health", false, None).0, 200);
        assert_eq!(s.http_with("GET", "/info", false, None).0, 401);
        assert_eq!(s.http_with("GET", "/ready", false, None).0, 200);
        let (code, body) = raw_bearer(&s, "wrong");
        assert_eq!((code, &body["error"]), (401, &json!("unauthorized")));
        // The probe left nothing behind, and made nothing.
        let left: Vec<_> = walk(&home_of(&s.dir))
            .into_iter()
            .filter(|p| p.to_string_lossy().contains(".ready-probe"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    fn raw_bearer(s: &Serve, token: &str) -> (u16, Value) {
        use std::io::{Read, Write};
        let mut c = std::net::TcpStream::connect(("127.0.0.1", s.port)).unwrap();
        let req = format!(
            "GET /ready HTTP/1.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        c.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        let code = out.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = out.split("\r\n\r\n").nth(1).unwrap_or("");
        (
            code,
            serde_json::from_str(body.trim()).unwrap_or(Value::Null),
        )
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            out.push(e.path());
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            }
        }
        out
    }

    #[test]
    fn a_sessions_dir_that_is_a_file_makes_serve_alive_but_not_ready() {
        let file = file_in_place_of_dir("sessions");
        let s = serve(&[], &["--sessions-dir", file.join("s").to_str().unwrap()]);
        // Anyone sees the word; no one without the token sees which check failed.
        assert_eq!(ready(&s), (503, json!({"status": "not ready"})));
        let (code, body) = checks(&s);
        assert_eq!(code, 503, "{body}");
        assert_eq!(
            body["checks"],
            json!({"state": false, "cron": true, "webhook": true})
        );
        assert!(!body.to_string().contains(file.to_str().unwrap()));
        // Alive all the same: liveness is a different question.
        let (code, health) = s.http_with("GET", "/health", false, None);
        assert_eq!((code, &health["status"]), (200, &json!("ok")));
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn serve_cron_is_ready_when_the_ticker_beats_and_not_when_it_cannot_record() {
        let cron = unique_dir("tole-ready-cron");
        let env = [("TOLE_CRON_DIR", cron.to_str().unwrap())];
        let s = serve(&env, &["--cron"]);
        assert_eq!(until_ready(&s)["checks"]["cron"], true);

        // Replace the heartbeat file by a directory: the ticker's next beat cannot be renamed
        // over it, and the file cannot be read as a beat. The state check still holds.
        let beat = cron.join("ticker.json");
        std::fs::remove_file(&beat).unwrap();
        std::fs::create_dir_all(beat.join("x")).unwrap();
        assert_eq!(ready(&s), (503, json!({"status": "not ready"})));
        let (code, body) = checks(&s);
        assert_eq!(code, 503, "{body}");
        assert_eq!(
            body["checks"],
            json!({"state": true, "cron": false, "webhook": true})
        );
        let _ = std::fs::remove_dir_all(&cron);
    }

    #[test]
    fn without_cron_no_ticker_is_asked_for() {
        let cron = unique_dir("tole-ready-nocron");
        let env = [("TOLE_CRON_DIR", cron.to_str().unwrap())];
        let s = serve(&env, &[]);
        assert_eq!(until_ready(&s)["checks"]["cron"], true);
        assert!(!cron.join("ticker.json").exists(), "no ticker was started");
    }

    #[test]
    fn a_configured_webhook_destination_is_ready_while_its_sender_runs() {
        let dir = unique_dir("tole-ready-hook");
        let home = home_of(&dir);
        std::fs::create_dir_all(home.join("tole")).unwrap();
        std::fs::write(
            home.join("tole/config.toml"),
            "[webhook_out.gateway]\nurl = \"http://127.0.0.1:9/hook\"\n\
             hmac_key_env = \"READY_SIGN_KEY\"\nallow_loopback = true\n\
             events = [\"cron_run_finished\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let env = [("READY_SIGN_KEY", "ready-signing-key-0123456789abcdef")];
        let s = Serve::spawn_in_env_args("http://127.0.0.1:9", dir, true, &env, &[]);
        assert_eq!(until_ready(&s)["checks"]["webhook"], true);
    }

    #[test]
    fn a_draining_serve_is_not_ready() {
        let gate = Gate::new();
        let base = spawn_mock_gated(0, vec![("HOLD", Arc::clone(&gate))]);
        let dir = unique_dir("tole-ready-drain");
        std::fs::create_dir_all(&dir).unwrap();
        let s = Serve::spawn_in_env_args(&base, dir, true, &[], &[]);
        assert_eq!(until_ready(&s)["status"], "ok");
        let id = crate::serve_harness::new_session(&s);
        std::thread::scope(|sc| {
            let turn = sc.spawn(|| {
                let path = format!("/sessions/{id}/prompt");
                s.http("POST", &path, Some(r#"{"text":"HOLD the turn"}"#))
            });
            gate.wait_entered();
            let pid = s.child.id().to_string();
            assert!(std::process::Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success());
            let until = Instant::now() + Duration::from_secs(20);
            while ready(&s).0 != 503 && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            // The held turn keeps the drain open: both probes say so, and nothing else is served.
            assert_eq!(ready(&s).0, 503);
            assert_eq!(checks(&s).0, 503);
            assert_eq!(s.http_with("GET", "/health", false, None).0, 503);
            gate.open();
            let _ = turn.join();
        });
    }
}
