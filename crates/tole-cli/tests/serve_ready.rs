//! #1017: `GET /ready`, the readiness answer next to `/health`'s liveness. Real serve
//! processes for what a process can show (healthy, a state path that is not a directory,
//! `--cron` with a ticker that cannot record, a webhook destination), and the check itself
//! with real directories, files and threads for what a process cannot be made to do (a ticker
//! or a sender thread that died, a stale beat, a foreign beat).

// The ticker is the cron module's and the harness spawns the real `tole serve` binary
// (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
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

fn checks(answer: &(u16, Value)) -> [bool; 3] {
    let c = &answer.1["checks"];
    ["state", "cron", "webhook"].map(|k| c[k].as_bool().unwrap())
}

#[test]
fn probe_routes_are_exactly_health_and_ready() {
    assert!(is_probe("/health") && is_probe("/ready"));
    for p in ["/info", "/ready/", "/readyz", "/ready?x", "/sessions", ""] {
        assert!(!is_probe(p), "{p}");
    }
}

#[test]
fn state_dir_that_cannot_take_a_write_is_not_ready() {
    let dir = std::env::temp_dir().join(format!("tole-ready-ok-{}", std::process::id()));
    let ready = Readiness {
        state_dirs: vec![dir.clone()],
        ..Default::default()
    };
    let a = ready.answer(0);
    assert_eq!((a.0, checks(&a)), (200, [true; 3]), "made when missing");
    assert_eq!(a.1["status"], "ok");
    // The probe file is gone again.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();

    // One bad directory among good ones is enough, and nothing names it.
    let file = file_in_place_of_dir("state");
    let ready = Readiness {
        state_dirs: vec![std::env::temp_dir(), file.join("sessions")],
        ..Default::default()
    };
    let a = ready.answer(0);
    assert_eq!((a.0, checks(&a)), (503, [false, true, true]));
    assert_eq!(a.1["status"], "not ready");
    assert!(
        !a.1.to_string().contains("tole-ready"),
        "no path in the body"
    );
    std::fs::remove_file(&file).unwrap();
}

#[test]
fn concurrent_probes_do_not_remove_each_others_file() {
    let dir = std::env::temp_dir().join(format!("tole-ready-race-{}", std::process::id()));
    let ready = std::sync::Arc::new(Readiness {
        state_dirs: vec![dir.clone()],
        ..Default::default()
    });
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let ready = std::sync::Arc::clone(&ready);
            std::thread::spawn(move || (0..100).all(|_| ready.answer(0).0 == 200))
        })
        .collect();
    for t in threads {
        assert!(t.join().unwrap(), "a probe failed against a healthy dir");
    }
    std::fs::remove_dir_all(&dir).unwrap();
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
    let cron_ok = |r: &Readiness| checks(&r.answer(now))[1];

    assert!(
        cron_ok(&ready(false)),
        "--cron not asked for: nothing to check"
    );
    assert!(!cron_ok(&ready(true)), "asked for, no beat yet");
    beat(&dir, now, true);
    assert!(cron_ok(&ready(true)), "a fresh beat of this process");
    assert!(
        ready(true).answer(now + STALE_SECS).0 == 200,
        "the edge is inside"
    );
    assert_eq!(
        ready(true).answer(now + STALE_SECS + 1).0,
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
    assert!(checks(&ready.answer(0))[2], "a running sender");
    assert_eq!(ready.helpers.running(), 1);

    // A ticker thread that ended is the ticker check's business, not the webhook one.
    let ticker = std::thread::spawn(|| {});
    ready.helpers.add(false, ticker);
    while ready.helpers.running() > 1 {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(checks(&ready.answer(0))[2]);

    // The sender ends (stop is dropped, `recv` fails): not ready, and the check says which.
    drop(stop);
    let until = Instant::now() + Duration::from_secs(5);
    while ready.helpers.running() > 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    let a = ready.answer(0);
    assert_eq!((a.0, checks(&a)), (503, [true, true, false]));
}

// ------------------------------------------------------------------ a real serve

mod real {
    use super::*;
    use crate::serve_harness::{home_of, unique_dir, Serve};

    /// A serve that never calls the provider, over a fresh workspace.
    fn serve(env: &[(&str, &str)], args: &[&str]) -> Serve {
        let dir = unique_dir("tole-ready");
        std::fs::create_dir_all(&dir).unwrap();
        Serve::spawn_in_env_args("http://127.0.0.1:9", dir, true, env, args)
    }

    /// `GET /ready` without a credential.
    fn ready(s: &Serve) -> (u16, Value) {
        s.http_with("GET", "/ready", false, None)
    }

    fn until_ready(s: &Serve) -> Value {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let (code, body) = ready(s);
            if code == 200 || Instant::now() > until {
                assert_eq!(code, 200, "never ready: {body}");
                return body;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn a_healthy_serve_is_ready_without_a_token_and_says_nothing_else() {
        let s = serve(&[], &[]);
        let (code, body) = ready(&s);
        let all = json!({"state": true, "cron": true, "webhook": true});
        assert_eq!(
            (code, &body),
            (200, &json!({"status": "ok", "checks": all}))
        );
        let text = body.to_string();
        assert!(!text.contains(s.dir.to_str().unwrap()) && !text.contains("tole-ready"));
        // Behind no token, like `/health`; the routes with state still are.
        assert_eq!(s.http_with("GET", "/health", false, None).0, 200);
        assert_eq!(s.http_with("GET", "/info", false, None).0, 401);
        // The authenticated caller gets the same answer: there is nothing more to tell.
        assert_eq!(s.http("GET", "/ready", None), (200, body));
        // The probe left nothing behind in the state directory.
        let left: Vec<_> = walk(&home_of(&s.dir))
            .into_iter()
            .filter(|p| p.to_string_lossy().contains(".ready-probe"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
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
        let (code, body) = ready(&s);
        assert_eq!(code, 503, "{body}");
        assert_eq!(body["status"], "not ready");
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
        let (code, body) = ready(&s);
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
}
