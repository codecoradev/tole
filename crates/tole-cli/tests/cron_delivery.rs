//! `tole cron` delivery targets (#579): `log`, `file:`, trust-gated
//! `command:`, and redaction of web-header secrets from run output.
#![cfg(feature = "shell-tools")]

use std::path::{Path, PathBuf};

use tole_cli::cron::{self, store, CreateSpec, TickOptions};
use tole_core::ledger::{DeliveryStatus, RunStatus};

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

fn deliveries(dir: &Path, fire: &str) -> Vec<(String, DeliveryStatus, Option<String>)> {
    let l = store::open_read(dir).unwrap();
    l.state()
        .deliveries(fire)
        .map(|d| (d.target.clone(), d.status, d.error.clone()))
        .collect()
}

#[test]
fn file_target_appends_inside_the_workdir() {
    let dir = tmp("file");
    let spec = script(
        &dir,
        "f",
        "* * * * *",
        "echo report",
        &["file:reports/daily.md", "log"],
    );
    cron::create(&dir, spec, T).unwrap();
    let r1 = cron::tick(&dir, T + 60, &OPTS).unwrap();
    cron::tick(&dir, T + 120, &OPTS).unwrap();
    let text = std::fs::read_to_string(dir.join("reports/daily.md")).unwrap();
    assert_eq!(text.matches("report\n").count(), 2, "{text}");
    assert!(text.starts_with("--- f 2026-01-01 00:01Z ok ("), "{text}");
    let d = deliveries(&dir, &r1.ran[0].0);
    assert_eq!(d.len(), 2);
    assert!(
        d.iter().all(|(_, s, _)| *s == DeliveryStatus::Sent),
        "{d:?}"
    );
    assert_eq!(d[0].0, "file:reports/daily.md");
}

#[test]
fn bad_targets_are_refused_on_save() {
    let dir = tmp("badtarget");
    for bad in [
        "file:../out.md",
        "file:/etc/x",
        "file:",
        "smtp:me",
        "session",
        "command:rm -rf /",
    ] {
        let e = cron::create(&dir, script(&dir, "b", "@daily", "echo x", &[bad]), T);
        assert!(e.is_err(), "{bad} accepted");
    }
}

#[cfg(unix)]
#[test]
fn a_file_target_symlinked_out_of_the_workdir_fails_delivery_not_the_run() {
    let dir = tmp("symlink");
    let outside = tmp("symlink-outside");
    std::os::unix::fs::symlink(&outside, dir.join("out")).unwrap();
    cron::create(
        &dir,
        script(&dir, "s", "* * * * *", "echo x", &["file:out/x.md"]),
        T,
    )
    .unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    assert_eq!(r.ran[0].1, RunStatus::Ok);
    let d = deliveries(&dir, &r.ran[0].0);
    assert_eq!(d[0].1, DeliveryStatus::Failed, "{d:?}");
    assert!(!outside.join("x.md").exists());
}

#[test]
fn command_target_gets_the_output_on_stdin_and_its_failure_is_separate() {
    let dir = tmp("command");
    let spec = script(
        &dir,
        "c",
        "* * * * *",
        "echo piped",
        &["command:tee got.txt", "command:false"],
    );
    cron::create(&dir, spec, T).unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    assert_eq!(
        r.ran[0].1,
        RunStatus::Ok,
        "a failed delivery is not a failed run"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("got.txt")).unwrap(),
        "piped\n"
    );
    // Listed by target: `command:false` sorts first.
    let d = deliveries(&dir, &r.ran[0].0);
    assert_eq!(d[0].0, "command:false");
    assert_eq!(d[0].1, DeliveryStatus::Failed);
    assert!(
        d[0].2.as_deref().unwrap().starts_with("exit Some(1)"),
        "{d:?}"
    );
    assert_eq!(
        (d[1].0.as_str(), d[1].1),
        ("command:tee got.txt", DeliveryStatus::Sent)
    );
}

#[test]
fn a_command_target_on_a_job_not_saved_by_the_cli_is_blocked() {
    let dir = tmp("untrusted");
    let spec = script(&dir, "u", "* * * * *", "echo x", &["command:tee got.txt"]);
    let mut job = cron::create(&dir, spec, T).unwrap();
    job.extra.insert("created_by".into(), "agent".into());
    job.at = T + 1;
    store::with_lock(&dir, |l| Ok(l.put_job(job)?)).unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    assert!(r.ran.is_empty());
    assert_eq!(r.blocked, 1);
    let l = store::open_read(&dir).unwrap();
    assert_eq!(
        l.state().blocked().next().unwrap().reason,
        "delivery_untrusted"
    );
    assert!(!dir.join("got.txt").exists());
}

#[test]
fn web_header_secrets_are_redacted_from_run_output() {
    // Not secret-shaped by name: only the web header setting marks it.
    let value = "hdr-value-cron-0123456789";
    std::env::set_var("CRON_TEST_HEADER_VAL", value);
    std::env::set_var("TOLE_WEB_FETCH_HEADERS", "X-Key: ${CRON_TEST_HEADER_VAL}");
    let dir = tmp("hdr");
    let line = format!("echo key={value}");
    cron::create(
        &dir,
        script(&dir, "h", "* * * * *", &line, &["file:h.md"]),
        T,
    )
    .unwrap();
    let r = cron::tick(&dir, T + 60, &OPTS).unwrap();
    let out = std::fs::read_to_string(cron::tick::output_path(&dir, &r.ran[0].0)).unwrap();
    assert!(out.starts_with("key=") && !out.contains(value), "{out}");
    let file = std::fs::read_to_string(dir.join("h.md")).unwrap();
    assert!(!file.contains(value), "{file}");
}
