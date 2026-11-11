//! `tole cron` control (#579): `run`/`cancel`/`edit`, ticker status and
//! doctor, `runs_json` and `GET /cron/runs` on `tole serve`.
#![cfg(feature = "shell-tools")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tole_cli::cron::{self, status, store, CreateSpec, EditSpec, TickOptions};
use tole_core::ledger::RunStatus;

/// 2026-01-01T00:00:00Z.
const T: i64 = 1_767_225_600;
const OPTS: TickOptions = TickOptions::standalone(4);

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-cronctl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn script(dir: &Path, id: &str, schedule: &str, line: &str, deliver: &[&str]) -> CreateSpec {
    CreateSpec {
        id: Some(id.into()),
        schedule: schedule.into(),
        script: Some(line.into()),
        workdir: dir.to_path_buf(),
        deliver: deliver.iter().map(|s| s.to_string()).collect(),
        ..CreateSpec::default()
    }
}

#[test]
fn run_now_runs_once_and_keeps_the_next_scheduled_run() {
    let dir = tmp("runnow");
    cron::create(&dir, script(&dir, "r", "0 9 * * *", "echo now", &[]), T).unwrap();
    let r = cron::run_now(&dir, "r", T + 3_600, &OPTS).unwrap();
    assert_eq!(r.ran.len(), 1);
    assert_eq!(r.ran[0].1, RunStatus::Ok);
    let out = std::fs::read_to_string(cron::tick::output_path(&dir, &r.ran[0].0)).unwrap();
    assert_eq!(out, "now\n");
    let l = store::open_read(&dir).unwrap();
    assert_eq!(
        l.state().next_run("r"),
        Some(T + 9 * 3_600),
        "09:00 still owed"
    );
    // Paused jobs are refused.
    cron::update(&dir, "r", T + 3_700, |j| j.paused = true).unwrap();
    assert!(cron::run_now(&dir, "r", T + 3_800, &OPTS).is_err());
}

#[test]
fn run_now_on_an_on_time_occurrence_runs_that_occurrence() {
    let dir = tmp("runontime");
    cron::create(&dir, script(&dir, "o", "0 1 * * *", "echo o", &[]), T).unwrap();
    let r = cron::run_now(&dir, "o", T + 3_600 + 30, &OPTS).unwrap();
    assert_eq!((r.ran.len(), r.missed), (1, 0));
    let l = store::open_read(&dir).unwrap();
    let fire = l.state().fire(&r.ran[0].0).unwrap();
    assert_eq!(
        fire.scheduled_for,
        T + 3_600,
        "the owed 01:00 run, not a new one"
    );
}

#[test]
fn run_now_on_a_late_job_records_the_missed_occurrence() {
    let dir = tmp("runlate");
    cron::create(&dir, script(&dir, "l", "0 1 * * *", "echo late", &[]), T).unwrap();
    // 01:00 was owed and is now two hours late (catch_up skip).
    let r = cron::run_now(&dir, "l", T + 3 * 3_600, &OPTS).unwrap();
    assert_eq!((r.ran.len(), r.missed), (1, 1));
}

#[test]
fn cancel_stops_a_running_script() {
    let dir = tmp("cancel");
    cron::create(&dir, script(&dir, "z", "* * * * *", "sleep 30", &[]), T).unwrap();
    assert!(cron::cancel(&dir, "z").is_err(), "nothing running yet");
    let d2 = dir.clone();
    let start = Instant::now();
    let runner = std::thread::spawn(move || cron::tick(&d2, T + 60, &OPTS).unwrap());
    let fires = loop {
        if let Ok(f) = cron::cancel(&dir, "z") {
            break f;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "never claimed");
        std::thread::sleep(Duration::from_millis(20));
    };
    let r = runner.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(10), "not cancelled");
    assert_eq!(r.ran, vec![(fires[0].clone(), RunStatus::Cancelled)]);
    assert!(
        !cron::tick::cancel_path(&dir, &fires[0]).exists(),
        "marker removed"
    );
}

#[test]
fn edit_changes_only_the_given_fields_and_keeps_the_mode() {
    let dir = tmp("edit");
    let mut spec = script(&dir, "e", "@daily", "echo a", &["log"]);
    spec.name = Some("nightly".into());
    cron::create(&dir, spec, T).unwrap();
    let change = EditSpec {
        schedule: Some("0 * * * *".into()),
        deliver: vec!["file:e.md".into()],
        ..EditSpec::default()
    };
    let j = cron::edit(&dir, "nightly", change, T + 10).unwrap();
    assert_eq!(j.schedule, "0 * * * *");
    assert_eq!(cron::field(&j, "script"), Some("echo a"));
    assert_eq!(cron::field(&j, "name"), Some("nightly"));
    assert_eq!(cron::field(&j, "created_by"), Some("cli"));
    assert_eq!(j.extra["deliver"], serde_json::json!(["file:e.md"]));
    let bad = EditSpec {
        deliver: vec!["session".into()],
        ..EditSpec::default()
    };
    assert!(
        cron::edit(&dir, "e", bad, T + 20).is_err(),
        "validated like create"
    );
    let huge = EditSpec {
        max_minutes: Some(u64::MAX),
        ..EditSpec::default()
    };
    assert!(cron::edit(&dir, "e", huge, T + 20).is_err());
}

#[test]
fn an_agent_job_cannot_be_edited_into_a_script() {
    let dir = tmp("editmode");
    let spec = CreateSpec {
        id: Some("a".into()),
        schedule: "@daily".into(),
        prompt: Some("hi".into()),
        workdir: dir.clone(),
        ..CreateSpec::default()
    };
    cron::create(&dir, spec, T).unwrap();
    let change = EditSpec {
        script: Some("echo x".into()),
        ..EditSpec::default()
    };
    assert!(cron::edit(&dir, "a", change, T + 1).is_err());
}

#[test]
fn status_and_doctor_report_a_stale_ticker_while_jobs_are_active() {
    let dir = tmp("status");
    let (lines, ok) = status::doctor(&dir, T);
    assert!(ok, "{lines:?}");
    cron::create(&dir, script(&dir, "s", "@hourly", "echo x", &[]), T).unwrap();
    let (lines, ok) = status::doctor(&dir, T);
    assert!(!ok, "active job, no ticker: {lines:?}");
    cron::tick(&dir, T + 30, &OPTS).unwrap();
    let st = status::status(&dir, T + 60).unwrap();
    assert!(st.alive && st.active == 1, "{st:?}");
    assert_eq!(st.ticker.as_ref().unwrap().host, "tick");
    assert!(status::doctor(&dir, T + 60).1);
    let stale = status::status(&dir, T + 30 + status::STALE_SECS + 1).unwrap();
    assert!(!stale.alive);
    assert!(!status::doctor(&dir, T + 30 + status::STALE_SECS + 1).1);
    cron::update(&dir, "s", T + 40, |j| j.paused = true).unwrap();
    assert!(
        status::doctor(&dir, T + 10_000).1,
        "nothing active, nothing owed"
    );
}

#[test]
fn runs_json_lists_newest_first_with_output_and_deliveries() {
    let dir = tmp("json");
    cron::create(&dir, script(&dir, "j", "* * * * *", "echo out", &[]), T).unwrap();
    for i in 1..=3 {
        cron::tick(&dir, T + 60 * i, &OPTS).unwrap();
    }
    let v = cron::runs_json(&dir, Some("j"), Some(2)).unwrap();
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["scheduled_for"], T + 180);
    assert_eq!(runs[0]["status"], "ok");
    assert_eq!(runs[0]["output"], "out\n");
    assert_eq!(runs[0]["deliveries"][0]["target"], "log");
    assert!(cron::runs_json(&dir, Some("nope"), None).is_err());
}

mod common;

/// One request to a local server: `(status code, body)`.
fn http_get(port: u16, path: &str, token: Option<&str>) -> (u16, serde_json::Value) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(s, "GET {path} HTTP/1.1\r\n{auth}Connection: close\r\n\r\n").unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).unwrap();
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (code, serde_json::from_str(body.trim()).unwrap_or_default())
}

#[test]
fn serve_lists_cron_runs_behind_the_token() {
    let dir = tmp("serve");
    cron::create(
        &dir,
        script(&dir, "pull", "* * * * *", "echo pulled", &[]),
        T,
    )
    .unwrap();
    cron::tick(&dir, T + 60, &OPTS).unwrap();
    let (mut child, port) = common::spawn_listening(|port| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
            .env("TOLE_CRON_DIR", &dir)
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        cmd
    });
    let unauth = http_get(port, "/cron/runs", None);
    let (code, body) = http_get(port, "/cron/runs?job=pull&limit=5", Some("t0ken"));
    let missing = http_get(port, "/cron/runs?job=nope", Some("t0ken"));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(unauth.0, 401);
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["runs"][0]["job_id"], "pull");
    assert_eq!(body["runs"][0]["output"], "pulled\n");
    assert_eq!(missing.0, 404);
}
