//! Agent-created cron jobs (#581 part 2): an agent creates a job only
//! through the `cron_create` tool. The host records who created it
//! (`created_by`) and the creator's grant (`creator_allow`), and saves it
//! paused. A job that a human did not create through the CLI runs only
//! after a human enables it with `tole cron resume`, never with more than
//! its creator held, and never as a script. Hand-written ledger lines are
//! held to the same rules.

#![cfg(feature = "shell-tools")]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{json, Value};
use tole_cli::cron::{self, store, tool::CronCreateTool, CreateSpec, EditSpec, TickOptions};
use tole_core::ledger::Job;
use tole_core::tool::Tool;

/// A provider that refuses every request (400, not retried): an enabled
/// job is claimed and fails fast instead of being blocked.
fn env() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                use std::io::{Read, Write};
                let _ = s.read(&mut [0u8; 65536]);
                let body = r#"{"error":{"message":"refused","type":"invalid_request_error"}}"#;
                let _ = write!(
                    s,
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        std::env::set_var("TOLE_BASE_URL", format!("http://{addr}/v1"));
        std::env::set_var("TOLE_MODEL", "mock-model");
        std::env::set_var("TOLE_API_KEY", "sk-test-cron-trust");
    });
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-cron-trust-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// A cron dir and a workdir that does not hold it.
fn dirs(tag: &str) -> (PathBuf, PathBuf) {
    (tmp(&format!("{tag}-cron")), tmp(&format!("{tag}-work")))
}

fn grant(g: &[&str]) -> Vec<String> {
    g.iter().map(|s| s.to_string()).collect()
}

fn by_agent(cron: &Path, work: &Path, g: &[&str], input: Value) -> Result<Value, String> {
    CronCreateTool::new(Some(cron.to_path_buf()), work.to_path_buf(), grant(g)).execute(input)
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

/// Appends `job` as a hand-written line would.
fn hand_write(cron: &Path, job: Job) {
    store::with_lock(cron, |l| Ok(l.put_job(job)?)).unwrap();
}

/// What a tick at `at` did with `id`: the code it was blocked with, or
/// `ran`, or `idle`.
fn tick(cron: &Path, id: &str, at: i64) -> String {
    let r = cron::tick(cron, at, &TickOptions::standalone(2)).unwrap();
    let l = store::open_read(cron).unwrap();
    if let Some(b) = l
        .state()
        .blocked()
        .filter(|b| b.job_id == id && b.at == at)
        .last()
    {
        return b.reason.clone();
    }
    if r.ran
        .iter()
        .any(|(f, _)| l.state().done(f).unwrap().job_id == id)
    {
        return "ran".into();
    }
    "idle".into()
}

fn agent_input() -> Value {
    json!({"schedule": "* * * * *", "prompt": "summarize the inbox", "allow": ["write_file"]})
}

#[test]
fn the_tool_saves_a_paused_job_with_its_creator_and_grant() {
    let (cron, work) = dirs("save");
    let g = ["write_file", "!write_file:secret*"];
    let out = by_agent(&cron, &work, &g, agent_input()).unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    assert!(out["note"].as_str().unwrap().contains("tole cron resume"));
    let j = job(&cron, &id);
    assert!(j.paused, "created paused");
    assert_eq!(j.extra["created_by"], "agent");
    assert_eq!(j.extra["creator_allow"], json!(g));
    // The creator's deny patterns always come along.
    assert_eq!(
        j.extra["allow"],
        json!(["write_file", "!write_file:secret*"])
    );
    assert_eq!(j.extra["workdir"], work.display().to_string());
    assert!(j.extra.get("enabled_by").is_none());
}

#[test]
fn the_tool_cannot_set_what_the_host_owns() {
    let (cron, work) = dirs("host");
    for (k, v) in [
        ("created_by", json!("cli")),
        ("creator_allow", json!(["*"])),
        ("enabled_by", json!("cli")),
        ("paused", json!(false)),
        ("workdir", json!("/")),
        ("script", json!("echo hi")),
        ("skills", json!(["x"])),
        ("profile", json!("p")),
        ("id", json!("mine")),
    ] {
        let mut input = agent_input();
        input[k] = v;
        let e = by_agent(&cron, &work, &["write_file"], input).unwrap_err();
        assert!(e.contains(k), "{k}: {e}");
    }
    let l = store::open_read(&cron).unwrap();
    assert_eq!(l.state().jobs().count(), 0, "nothing was saved");
}

#[test]
fn the_tool_refuses_more_than_the_creator_holds() {
    let (cron, work) = dirs("more");
    for allow in [json!(["write_file", "edit_file"]), json!(["*"])] {
        let mut input = agent_input();
        input["allow"] = allow.clone();
        let e = by_agent(&cron, &work, &["write_file"], input).unwrap_err();
        assert!(e.contains("creator"), "{allow}: {e}");
    }
    let mut input = agent_input();
    input["deliver"] = json!(["command:notify-send done"]);
    let e = by_agent(&cron, &work, &["write_file"], input).unwrap_err();
    assert!(e.contains("command"), "{e}");
    assert_eq!(store::open_read(&cron).unwrap().state().jobs().count(), 0);
}

#[test]
fn an_agent_job_runs_only_after_a_human_enables_it() {
    env();
    let (cron, work) = dirs("enable");
    let out = by_agent(&cron, &work, &["write_file"], agent_input()).unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    let b = job(&cron, &id).at;
    assert_eq!(tick(&cron, &id, b + 120), "idle", "paused");
    // A hand-written line that unpauses it is not a human enabling it.
    hand_write(
        &cron,
        Job {
            paused: false,
            at: b + 150,
            ..job(&cron, &id)
        },
    );
    assert_eq!(tick(&cron, &id, b + 240), "not_enabled");
    cron::enable(&cron, &id, b + 270).unwrap();
    assert_eq!(job(&cron, &id).extra["enabled_by"], "cli");
    assert_eq!(tick(&cron, &id, b + 360), "ran");
}

#[test]
fn a_hand_written_line_cannot_widen_an_agent_job() {
    env();
    let (cron, work) = dirs("widen");
    let g = ["write_file", "!write_file:secret*"];
    let id = by_agent(&cron, &work, &g, agent_input()).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = job(&cron, &id).at;
    cron::enable(&cron, &id, b + 10).unwrap();
    // Claims to be the owner's, with a wider grant: the creator and its
    // grant stay those of the creating line.
    let mut wide = job(&cron, &id);
    wide.at = b + 20;
    wide.extra.insert("created_by".into(), "cli".into());
    wide.extra
        .insert("creator_allow".into(), json!(["write_file", "edit_file"]));
    wide.extra
        .insert("allow".into(), json!(["write_file", "edit_file"]));
    hand_write(&cron, wide);
    assert_eq!(job(&cron, &id).extra["created_by"], "agent");
    assert_eq!(tick(&cron, &id, b + 120), "allow_exceeds_creator");
    // Dropping the creator's deny pattern widens it too.
    let mut no_deny = job(&cron, &id);
    no_deny.at = b + 130;
    no_deny.extra.insert("allow".into(), json!(["write_file"]));
    hand_write(&cron, no_deny);
    assert_eq!(tick(&cron, &id, b + 240), "allow_exceeds_creator");
}

#[test]
fn a_line_of_unknown_origin_is_held_like_an_agents() {
    env();
    let (cron, work) = dirs("unknown");
    let spec = CreateSpec {
        id: Some("u".into()),
        schedule: "* * * * *".into(),
        prompt: Some("hi".into()),
        workdir: work.clone(),
        ..CreateSpec::default()
    };
    let mut j = cron::create(&cron, spec, cron::now()).unwrap();
    let b = j.at;
    // The same job written by hand without `created_by`.
    j.extra.remove("created_by");
    j.at = b + 10;
    hand_write(
        &cron,
        Job {
            id: "u2".into(),
            ..j.clone()
        },
    );
    assert_eq!(tick(&cron, "u2", b + 120), "not_enabled");
    // A script job the owner did not save through the CLI never runs,
    // even marked enabled.
    let mut script = j.clone();
    script.id = "s".into();
    script.at = b + 130;
    script.extra.insert("mode".into(), "script".into());
    script.extra.insert("script".into(), "echo hi".into());
    script.extra.insert("created_by".into(), "agent".into());
    script.extra.insert("enabled_by".into(), "cli".into());
    hand_write(&cron, script);
    assert_eq!(tick(&cron, "s", b + 240), "script_untrusted");
    // The owner's own job runs.
    assert_eq!(tick(&cron, "u", b + 360), "ran");
}

#[test]
fn an_agent_run_whose_jail_holds_the_cron_dir_is_blocked() {
    env();
    let work = tmp("jail-work");
    let cron = work.join("cron");
    std::fs::create_dir_all(&cron).unwrap();
    let spec = CreateSpec {
        id: Some("w".into()),
        schedule: "* * * * *".into(),
        prompt: Some("hi".into()),
        workdir: work.clone(),
        ..CreateSpec::default()
    };
    let b = cron::create(&cron, spec, cron::now()).unwrap().at;
    assert_eq!(tick(&cron, "w", b + 120), "ledger_in_workdir");
}

#[test]
fn editing_an_agent_job_keeps_its_creator_and_cannot_widen_it() {
    let (cron, work) = dirs("edit");
    let id = by_agent(&cron, &work, &["write_file"], agent_input()).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = job(&cron, &id).at;
    cron::enable(&cron, &id, b + 10).unwrap();
    let renamed = EditSpec {
        name: Some("inbox".into()),
        ..EditSpec::default()
    };
    let j = cron::edit(&cron, &id, renamed, b + 20).unwrap();
    assert_eq!(j.extra["name"], "inbox");
    assert_eq!(j.extra["created_by"], "agent");
    assert_eq!(j.extra["creator_allow"], json!(["write_file"]));
    assert_eq!(j.extra["enabled_by"], "cli");
    let wider = EditSpec {
        allow: vec!["edit_file".into()],
        ..EditSpec::default()
    };
    let e = cron::edit(&cron, &id, wider, b + 30).unwrap_err();
    assert!(e.to_string().contains("creator"), "{e}");
}
