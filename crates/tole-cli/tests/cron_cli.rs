//! `tole cron` (#579): job store, script runs, tick loop, recovery.
#![cfg(feature = "shell-tools")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use tole_cli::cron::{self, store, tick::run_bounded, CreateSpec, TickOptions};
use tole_core::ledger::{DeliveryStatus, Due, Lease, RunStatus};

/// 2026-01-01T00:00:00Z.
const T: i64 = 1_767_225_600;
const OPTS: TickOptions = TickOptions::standalone(4);

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-cron-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn script(dir: &Path, id: &str, schedule: &str, line: &str) -> CreateSpec {
    CreateSpec {
        id: Some(id.into()),
        schedule: schedule.into(),
        script: Some(line.into()),
        workdir: dir.to_path_buf(),
        ..CreateSpec::default()
    }
}

fn output(dir: &Path, fire_id: &str) -> String {
    std::fs::read_to_string(cron::tick::output_path(dir, fire_id)).unwrap_or_default()
}

#[test]
fn script_job_runs_once_when_due_and_delivers_its_output() {
    let dir = tmp("run");
    cron::create(&dir, script(&dir, "hello", "*/5 * * * *", "echo hello"), T).unwrap();
    let early = cron::tick(&dir, T + 299, &OPTS).unwrap();
    assert!(early.ran.is_empty(), "not due yet");

    let r = cron::tick(&dir, T + 300, &OPTS).unwrap();
    assert_eq!(r.ran.len(), 1);
    let (fire_id, status) = &r.ran[0];
    assert_eq!(*status, RunStatus::Ok);
    assert_eq!(output(&dir, fire_id).trim(), "hello");

    // A second tick at the same instant never runs it again.
    assert!(cron::tick(&dir, T + 300, &OPTS).unwrap().ran.is_empty());
    let l = store::open_read(&dir).unwrap();
    let d: Vec<_> = l.state().deliveries(fire_id).collect();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].status, DeliveryStatus::Sent);
    let rows = cron::runs(&dir, Some("hello")).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, Some(RunStatus::Ok));
}

#[test]
fn silent_or_empty_output_delivers_nothing_and_failures_are_errors() {
    let dir = tmp("silent");
    cron::create(
        &dir,
        script(&dir, "quiet", "* * * * *", "printf '[SILENT]\\nx'"),
        T,
    )
    .unwrap();
    cron::create(&dir, script(&dir, "empty", "* * * * *", "true"), T).unwrap();
    cron::create(&dir, script(&dir, "fails", "* * * * *", "false"), T).unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    assert_eq!(r.ran.len(), 3);
    let l = store::open_read(&dir).unwrap();
    for (fire, status) in &r.ran {
        let job = l.state().fire(fire).unwrap().job_id.clone();
        let delivered = l.state().deliveries(fire).next().unwrap().status;
        match job.as_str() {
            "quiet" | "empty" => {
                assert_eq!(*status, RunStatus::Ok);
                assert_eq!(delivered, DeliveryStatus::Skipped, "{job}");
            }
            "fails" => {
                assert_eq!(*status, RunStatus::Error);
                assert_eq!(l.state().done(fire).unwrap().status, RunStatus::Error);
            }
            other => panic!("unexpected {other}"),
        }
    }
}

#[test]
fn agent_job_without_a_provider_is_blocked_before_spending() {
    // No provider in this test binary's environment (agent runs with a
    // mock provider live in tests/cron_agent.rs).
    for k in [
        "TOLE_BASE_URL",
        "OPENAI_BASE_URL",
        "TOLE_MODEL",
        "OPENAI_MODEL",
    ] {
        std::env::remove_var(k);
    }
    let dir = tmp("agent");
    cron::create(
        &dir,
        CreateSpec {
            id: Some("ask".into()),
            schedule: "* * * * *".into(),
            prompt: Some("summarize the inbox".into()),
            workdir: dir.clone(),
            ..CreateSpec::default()
        },
        T,
    )
    .unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    assert!(r.ran.is_empty());
    assert_eq!(r.blocked, 1);
    let l = store::open_read(&dir).unwrap();
    let b = l.state().blocked().next().unwrap();
    assert_eq!(b.reason, "provider_missing");
    assert_eq!(cron::list(&dir).unwrap()[0].blocked, 1);
}

#[test]
fn a_failed_output_write_is_recorded_and_every_claim_still_settles() {
    let dir = tmp("outerr");
    cron::create(&dir, script(&dir, "a", "* * * * *", "echo one"), T).unwrap();
    cron::create(&dir, script(&dir, "b", "* * * * *", "echo two"), T).unwrap();
    // `runs` is a file, so no output file can be written under it.
    std::fs::write(dir.join("runs"), b"not a dir").unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    assert_eq!(r.ran.len(), 2, "both claims settled");
    let l = store::open_read(&dir).unwrap();
    for (fire, status) in &r.ran {
        assert_eq!(*status, RunStatus::Ok);
        let usage = l.state().done(fire).unwrap().usage.clone().unwrap();
        assert!(usage["output_error"].is_string(), "{usage}");
    }
    assert!(l.state().pending().next().is_none(), "no dangling claim");
}

#[test]
fn run_output_is_redacted_before_it_reaches_disk() {
    let secret = "sk-cron-redact-0123456789abcdef";
    std::env::set_var("CRON_REDACT_TEST_API_KEY", secret);
    let dir = tmp("redact");
    let line = format!("echo token={secret}");
    cron::create(&dir, script(&dir, "leak", "* * * * *", &line), T).unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    let out = output(&dir, &r.ran[0].0);
    assert!(out.starts_with("token="), "{out}");
    assert!(!out.contains(secret), "secret written to disk: {out}");
}

#[test]
fn pause_resume_remove_and_name_resolution() {
    let dir = tmp("pause");
    let mut spec = script(&dir, "a1", "* * * * *", "true");
    spec.name = Some("Backup".into());
    cron::create(&dir, spec, T).unwrap();
    cron::update(&dir, "backup", T + 1, |j| j.paused = true).unwrap();
    assert!(cron::tick(&dir, T + 120, &OPTS).unwrap().ran.is_empty());
    assert_eq!(cron::list(&dir).unwrap()[0].state, "paused");
    cron::update(&dir, "a1", T + 121, |j| j.paused = false).unwrap();
    assert_eq!(cron::tick(&dir, T + 180, &OPTS).unwrap().ran.len(), 1);
    cron::update(&dir, "a1", T + 181, |j| j.removed = true).unwrap();
    assert!(cron::list(&dir).unwrap().is_empty());
    assert!(cron::tick(&dir, T + 600, &OPTS).unwrap().ran.is_empty());

    let mut twin = script(&dir, "b1", "@daily", "true");
    twin.name = Some("dup".into());
    cron::create(&dir, twin, T).unwrap();
    let mut twin = script(&dir, "b2", "@daily", "true");
    twin.name = Some("DUP".into());
    cron::create(&dir, twin, T).unwrap();
    let e = cron::update(&dir, "dup", T, |_| {})
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("ambiguous") && e.contains("b1") && e.contains("b2"),
        "{e}"
    );
}

#[test]
fn invalid_jobs_are_rejected_on_save() {
    let dir = tmp("invalid");
    let cases = [
        (script(&dir, "x", "0 0 30 2 *", "true"), "never fires"),
        (script(&dir, "x", "* * * * *", "rm -rf /"), "refused"),
        (script(&dir, "x", "* * * * *", "echo 'open"), "unterminated"),
        (script(&dir, "x", "* * * * *", "  "), "empty"),
        (
            CreateSpec {
                workdir: PathBuf::from("relative"),
                ..script(&dir, "x", "* * * * *", "true")
            },
            "absolute",
        ),
        (
            CreateSpec {
                prompt: None,
                script: None,
                ..script(&dir, "x", "* * * * *", "true")
            },
            "needs a PROMPT",
        ),
    ];
    for (spec, want) in cases {
        let e = cron::create(&dir, spec, T).unwrap_err().to_string();
        assert!(e.contains(want), "{e}");
    }
    cron::create(&dir, script(&dir, "x", "@daily", "true"), T).unwrap();
    let e = cron::create(&dir, script(&dir, "x", "@daily", "true"), T).unwrap_err();
    assert!(e.to_string().contains("already exists"));
}

#[test]
fn a_claim_left_by_a_dead_process_is_interrupted_not_rerun() {
    let dir = tmp("recover");
    cron::create(&dir, script(&dir, "r", "* * * * *", "true"), T).unwrap();
    // A previous tick claimed T+60 and died before settling.
    let dead = Lease {
        pid: u32::MAX - 7,
        start_time: 1,
    };
    store::with_lock(&dir, |l| {
        let due: Vec<Due> = l.ready(T + 60)?;
        l.claim(&due[0], dead, T + 60)?.unwrap();
        Ok(())
    })
    .unwrap();
    let r = cron::tick(&dir, T + 70, &OPTS).unwrap();
    assert_eq!(r.recovered, 1);
    assert!(r.ran.is_empty(), "the interrupted occurrence is not re-run");
    let l = store::open_read(&dir).unwrap();
    assert_eq!(
        l.state().last_outcome("r").unwrap().status,
        RunStatus::Interrupted
    );
    assert_eq!(cron::tick(&dir, T + 120, &OPTS).unwrap().ran.len(), 1);
}

#[test]
fn script_runs_get_no_secret_shaped_env() {
    std::env::set_var("CRON_TEST_API_KEY", "sk-should-not-leak");
    std::env::set_var("CRON_TEST_PLAIN", "visible");
    let dir = tmp("env");
    cron::create(&dir, script(&dir, "env", "* * * * *", "env"), T).unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    let out = output(&dir, &r.ran[0].0);
    assert!(out.contains("CRON_TEST_PLAIN=visible"), "{out}");
    assert!(
        !out.contains("sk-should-not-leak"),
        "secret leaked to a script run"
    );
}

#[test]
fn run_bounded_never_exceeds_the_limit() {
    let live = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let out = run_bounded((0..12).collect(), 3, |i: i32| {
        let n = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(20));
        live.fetch_sub(1, Ordering::SeqCst);
        i * 2
    });
    assert_eq!(out, (0..12).map(|i| i * 2).collect::<Vec<_>>());
    assert!(peak.load(Ordering::SeqCst) <= 3);
    assert!(peak.load(Ordering::SeqCst) >= 2, "runs are concurrent");
}

#[test]
fn cli_create_list_tick_round_trip() {
    let dir = tmp("bin");
    let tole = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(args)
            .env("TOLE_CRON_DIR", &dir)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let created = tole(&[
        "cron",
        "create",
        "@every 1m",
        "--id",
        "ping",
        "--script",
        "echo pong",
    ]);
    assert!(
        created.contains("created ping (script), next run:"),
        "{created}"
    );
    let list = tole(&["cron", "list"]);
    assert!(
        list.contains("ping  ping  [script] @every 1m active"),
        "{list}"
    );
    let t = tole(&["cron", "tick"]);
    assert!(
        t.contains("tick: ran 0"),
        "not due within the first minute: {t}"
    );
    assert!(tole(&["cron", "runs"]).is_empty());
    let status = tole(&["cron", "status"]);
    assert!(status.contains("ticker alive (tick pid"), "{status}");
    assert!(status.contains("jobs: 1 active, 0 paused"), "{status}");
    let edited = tole(&["cron", "edit", "ping", "--deliver", "file:pong.md"]);
    assert!(edited.starts_with("edited ping"), "{edited}");
    let ran = tole(&["cron", "run", "ping"]);
    assert!(
        ran.starts_with("run: ran 1") && ran.contains(" ok"),
        "{ran}"
    );
    assert!(std::fs::read_to_string(dir.join("pong.md"))
        .unwrap()
        .contains("pong\n"));
    assert!(tole(&["cron", "runs", "ping"]).contains("file:pong.md:sent"));
    assert!(tole(&["cron", "pause", "ping"]).contains("paused ping"));
}

#[test]
fn file_store_survives_a_torn_tail() {
    let dir = tmp("torn");
    cron::create(&dir, script(&dir, "t", "* * * * *", "true"), T).unwrap();
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(store::ledger_path(&dir))
        .unwrap();
    f.write_all(b"{\"v\":1,\"kind\":\"fire\",\"fire_").unwrap();
    drop(f);
    assert_eq!(
        cron::list(&dir).unwrap().len(),
        1,
        "read-only list skips the torn tail"
    );
    assert_eq!(cron::tick(&dir, T + 60, &OPTS).unwrap().ran.len(), 1);
    let text = std::fs::read_to_string(store::ledger_path(&dir)).unwrap();
    assert!(text
        .lines()
        .all(|l| tole_core::ledger::Record::parse_line(l).is_ok()));
}
