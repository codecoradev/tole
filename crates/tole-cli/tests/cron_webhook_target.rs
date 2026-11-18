//! `webhook:<name>` delivery targets (#911 part 2): the target is parsed and
//! checked against the destinations the host loaded, an agent-created job may
//! only name a destination that allows it, and until the sender exists a
//! webhook delivery records `failed` (`not_implemented`), never `sent`.
#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tole_cli::config::WebhookOutConfig;
use tole_cli::cron::{self, deliver, store, tool::CronCreateTool, CreateSpec, TickOptions};
use tole_cli::webhook_out::{self, Destinations};
use tole_core::ledger::{DeliveryStatus, Job};
use tole_core::tool::Tool;

/// 2026-01-01T00:00:00Z.
const T: i64 = 1_767_225_600;

fn tmp(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-webhook-target-{tag}"))
}

fn dest(allow_agent_jobs: bool, events: &[&str]) -> Destinations {
    let cfg = BTreeMap::from([(
        "gateway".to_string(),
        WebhookOutConfig {
            url: Some("https://hooks.example.com/tole".into()),
            hmac_key_env: Some("S".into()),
            events: Some(events.iter().map(|e| e.to_string()).collect()),
            allow_agent_jobs: Some(allow_agent_jobs),
            ..WebhookOutConfig::default()
        },
    )]);
    webhook_out::load(&cfg, &|_| Some("a-long-enough-test-secret".into())).unwrap()
}

fn script(dir: &Path, id: &str, deliver: &[&str]) -> CreateSpec {
    CreateSpec {
        id: Some(id.into()),
        schedule: "* * * * *".into(),
        script: Some("echo report".into()),
        workdir: dir.to_path_buf(),
        deliver: deliver.iter().map(|s| s.to_string()).collect(),
        ..CreateSpec::default()
    }
}

fn job(cron: &Path, id: &str) -> Job {
    store::open_read(cron)
        .unwrap()
        .state()
        .job(id)
        .unwrap()
        .job
        .clone()
}

fn with(d: Destinations) -> TickOptions {
    TickOptions {
        webhooks: Some(Arc::new(d)),
        ..TickOptions::standalone(2)
    }
}

#[test]
fn the_target_shape_is_checked_on_save() {
    let dir = tmp("shape");
    for bad in [
        "webhook:",
        "webhook:Bad Name",
        "webhook:UPPER",
        "webhook:a/b",
        "webhook:https://evil.example/",
        &format!("webhook:{}", "a".repeat(33)),
    ] {
        let e = cron::create(&dir, script(&dir, "j", &[bad]), T).unwrap_err();
        assert!(e.to_string().contains("webhook"), "{bad}: {e}");
    }
    let ok = cron::create(&dir, script(&dir, "ok", &["webhook:gateway", "log"]), T).unwrap();
    assert_eq!(ok.extra["deliver"], json!(["webhook:gateway", "log"]));
}

#[test]
fn a_cli_job_may_name_a_loaded_destination() {
    let dir = tmp("cli");
    cron::create(&dir, script(&dir, "j", &["webhook:gateway"]), T).unwrap();
    let j = job(&dir, "j");
    let d = dest(false, &["cron_run_finished"]);
    assert_eq!(deliver::check(&j, Some(&d)), Ok(()));
}

#[test]
fn an_unknown_unloaded_or_unlisted_destination_blocks_the_job() {
    let dir = tmp("blocked");
    cron::create(&dir, script(&dir, "j", &["webhook:gateway"]), T).unwrap();
    let j = job(&dir, "j");
    // Nothing loaded: fail closed.
    let (code, detail) = deliver::check(&j, None).unwrap_err();
    assert_eq!(code, "delivery_invalid");
    assert!(detail.contains("webhook_out.gateway"), "{detail}");
    // Loaded, but another name.
    cron::create(&dir, script(&dir, "k", &["webhook:other"]), T).unwrap();
    let d = dest(false, &["cron_run_finished"]);
    assert_eq!(
        deliver::check(&job(&dir, "k"), Some(&d)).unwrap_err().0,
        "delivery_invalid"
    );
    // The destination does not list the cron event.
    let d = dest(false, &["approval_requested"]);
    let (code, detail) = deliver::check(&j, Some(&d)).unwrap_err();
    assert_eq!(code, "delivery_invalid");
    assert!(detail.contains("cron_run_finished"), "{detail}");
}

#[test]
fn an_agent_created_job_needs_a_destination_that_allows_agent_jobs() {
    let cron_dir = tmp("agent-cron");
    let work = tmp("agent-work");
    let out = CronCreateTool::new(
        Some(cron_dir.to_path_buf()),
        work.to_path_buf(),
        vec!["write_file".to_string()],
    )
    .execute(json!({
        "schedule": "* * * * *",
        "prompt": "summarize",
        "allow": ["write_file"],
        "deliver": ["webhook:gateway"],
    }))
    .unwrap();
    let j = job(&cron_dir, out["id"].as_str().unwrap());
    let closed = dest(false, &["cron_run_finished"]);
    let (code, detail) = deliver::check(&j, Some(&closed)).unwrap_err();
    assert_eq!(code, "delivery_untrusted");
    assert!(detail.contains("allow_agent_jobs"), "{detail}");
    let open = dest(true, &["cron_run_finished"]);
    assert_eq!(deliver::check(&j, Some(&open)), Ok(()));
}

#[test]
fn a_standalone_tick_without_destinations_blocks_the_occurrence() {
    let dir = tmp("tick-blocked");
    cron::create(&dir, script(&dir, "j", &["webhook:gateway"]), T).unwrap();
    let r = cron::tick(&dir, T + 60, &TickOptions::standalone(2)).unwrap();
    assert!(r.ran.is_empty(), "{r:?}");
    let l = store::open_read(&dir).unwrap();
    let b = l
        .state()
        .blocked()
        .last()
        .expect("a blocked record")
        .clone();
    assert_eq!(b.reason, "delivery_invalid");
}

#[test]
fn until_the_sender_exists_a_webhook_delivery_fails_visibly() {
    let dir = tmp("tick-fails");
    cron::create(&dir, script(&dir, "j", &["webhook:gateway", "log"]), T).unwrap();
    let r = cron::tick(&dir, T + 60, &with(dest(false, &["cron_run_finished"]))).unwrap();
    assert_eq!(r.ran.len(), 1, "{r:?}");
    let l = store::open_read(&dir).unwrap();
    let d: Vec<_> = l
        .state()
        .deliveries(&r.ran[0].0)
        .map(|d| (d.target.clone(), d.status, d.error.clone()))
        .collect();
    let hook = d.iter().find(|(t, _, _)| t == "webhook:gateway").unwrap();
    assert_eq!(hook.1, DeliveryStatus::Failed, "{d:?}");
    assert!(
        hook.2.as_deref().unwrap().contains("not_implemented"),
        "{d:?}"
    );
    // The other target is unaffected, and the run itself still settled ok.
    let log = d.iter().find(|(t, _, _)| t == "log").unwrap();
    assert_eq!(log.1, DeliveryStatus::Sent, "{d:?}");
}
