//! #1042 cron over the REAL `tole serve` face, through the typed client: the
//! jobs and their history are read, any job can be paused, a job its owner
//! reviewed can be resumed, and nothing else about a job can be changed.

#![cfg(feature = "shell-tools")]

use serde::Serialize;
use serde_json::Value;
use tole_cli::cron::{self, store, CreateSpec, TickOptions};
use tole_client::{Client, Error};

mod common;
mod serve_harness;
use serve_harness::{home_of, spawn_mock, workspace_dir, Serve};

fn cron_dir(serve: &Serve) -> std::path::PathBuf {
    home_of(&serve.dir).join("tole/cron")
}

/// A serve over a cron dir with an owner's script job `nightly` (named
/// "Nightly report") and an agent's job (saved paused, never enabled).
fn serve(args: &[&str]) -> (Serve, String) {
    let dir = workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let cdir = home_of(&dir).join("tole/cron");
    let spec = |id: Option<&str>, script: Option<&str>| CreateSpec {
        id: id.map(str::to_string),
        name: id.map(|_| "Nightly report".to_string()),
        schedule: "0 9 * * *".into(),
        script: script.map(str::to_string),
        prompt: script.is_none().then(|| "summarise the day".to_string()),
        workdir: dir.clone(),
        ..CreateSpec::default()
    };
    let now = cron::now();
    cron::create(&cdir, spec(Some("nightly"), Some("echo hi")), now).unwrap();
    let agent = cron::create_by_agent(&cdir, spec(None, None), &[], now).unwrap();
    let serve = Serve::spawn_in_env_args(&spawn_mock(0), dir, true, &[], args);
    (serve, agent.id)
}

fn client(serve: &Serve) -> Client {
    let url = format!("http://127.0.0.1:{}", serve.port);
    Client::connect_tcp(&url, Some("t0ken".into()), false).unwrap()
}

fn lossless<T: Serialize>(what: &str, typed: &T, raw: &Value) {
    assert_eq!(&serde_json::to_value(typed).unwrap(), raw, "{what}");
}

fn status_of<T: std::fmt::Debug>(r: Result<T, Error>) -> u16 {
    match r {
        Err(Error::Status { status, .. }) => status,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_jobs_are_listed_with_their_schedule_trust_and_history() {
    let (serve, agent) = serve(&[]);
    let c = client(&serve);
    let list = c.cron_jobs().unwrap();
    let nightly = list.jobs.iter().find(|j| j.id == "nightly").unwrap();
    assert_eq!(
        (nightly.state.as_str(), nightly.created_by.as_str()),
        ("active", "cli")
    );
    assert!(nightly.next_run.is_some() && !nightly.awaiting_review && nightly.last.is_none());
    assert_eq!(
        (nightly.schedule.as_str(), nightly.mode.as_str()),
        ("0 9 * * *", "script")
    );
    let job = list.jobs.iter().find(|j| j.id == agent).unwrap();
    assert_eq!(
        (job.state.as_str(), job.created_by.as_str()),
        ("paused", "agent")
    );
    assert!(job.awaiting_review && job.next_run.is_none());
    // No ticker has run on this home: the client is told, not left to guess.
    assert!(!list.scheduler.alive && list.scheduler.host.is_none());
    let (code, raw) = serve.http("GET", "/cron/jobs", None);
    assert_eq!(code, 200);
    lossless("jobs", &list, &raw);

    // A run lands in the history with its status, output and delivery record.
    let r = cron::run_now(
        &cron_dir(&serve),
        "nightly",
        cron::now(),
        &TickOptions::standalone(1),
    )
    .unwrap();
    assert_eq!(r.ran.len(), 1);
    let runs = c.cron_runs(Some("nightly"), Some(5)).unwrap();
    assert_eq!(runs.runs.len(), 1);
    assert_eq!(runs.runs[0].status.as_deref(), Some("ok"));
    assert_eq!(runs.runs[0].output, "hi\n");
    let (_, raw) = serve.http("GET", "/cron/runs?job=nightly&limit=5", None);
    lossless("runs", &runs, &raw);
    let jobs = c.cron_jobs().unwrap();
    assert_eq!(
        jobs.jobs
            .iter()
            .find(|j| j.id == "nightly")
            .unwrap()
            .last
            .as_deref(),
        Some("ok")
    );
    assert_eq!(status_of(c.cron_runs(Some("nope"), None)), 404);
    let (code, _) = serve.http_with("GET", "/cron/jobs", false, None);
    assert_eq!(code, 401);
}

#[test]
fn any_job_can_be_paused_and_a_reviewed_one_resumed() {
    let (serve, _) = serve(&[]);
    let c = client(&serve);
    assert_eq!(c.pause_cron_job("nightly").unwrap().state, "paused");
    let jobs = c.cron_jobs().unwrap();
    assert_eq!(
        jobs.jobs
            .iter()
            .find(|j| j.id == "nightly")
            .unwrap()
            .next_run,
        None
    );
    assert_eq!(c.resume_cron_job("nightly").unwrap().state, "active");
    // The owner's own record is not rewritten by a resume over serve.
    let l = store::open_read(&cron_dir(&serve)).unwrap();
    let job = &l.state().job("nightly").unwrap().job;
    assert!(job.extra.get("enabled_by").is_none() && !job.paused);

    // A job is addressed by its id, not by its name, and must exist.
    assert_eq!(status_of(c.pause_cron_job("Nightly report")), 404);
    assert_eq!(status_of(c.resume_cron_job("nope")), 404);
}

#[test]
fn a_job_an_agent_saved_cannot_be_resumed_without_the_owners_review() {
    let (serve, agent) = serve(&[]);
    let c = client(&serve);
    let refused = c.resume_cron_job(&agent);
    match &refused {
        Err(Error::Status {
            status: 403,
            message,
            ..
        }) => {
            assert!(message.contains("tole cron resume"), "{message}")
        }
        other => panic!("expected a 403, got {other:?}"),
    }
    let l = store::open_read(&cron_dir(&serve)).unwrap();
    let job = &l.state().job(&agent).unwrap().job;
    assert!(job.paused && job.extra.get("enabled_by").is_none());
    // Pausing it is allowed; once the owner reviewed it at a terminal, a pause
    // and a resume over serve work.
    assert_eq!(c.pause_cron_job(&agent).unwrap().state, "paused");
    cron::enable(&cron_dir(&serve), &agent, cron::now()).unwrap();
    assert_eq!(c.pause_cron_job(&agent).unwrap().state, "paused");
    assert_eq!(c.resume_cron_job(&agent).unwrap().state, "active");
}

#[test]
fn nothing_else_about_a_job_has_a_route_and_plan_mode_does_not_resume() {
    let (serve, _) = serve(&["--plan-mode"]);
    let c = client(&serve);
    // Create, edit, remove, run: none is a route, whatever the method.
    for (method, path) in [
        ("POST", "/cron/jobs"),
        ("PUT", "/cron/jobs/nightly"),
        ("PATCH", "/cron/jobs/nightly"),
        ("DELETE", "/cron/jobs/nightly"),
        ("POST", "/cron/jobs/nightly/remove"),
        ("POST", "/cron/jobs/nightly/edit"),
        ("POST", "/cron/jobs/nightly/run"),
    ] {
        let (code, v) = serve.http(method, path, Some(r#"{"schedule":"* * * * *"}"#));
        assert!(code == 404 || code == 405, "{method} {path}: {code} {v}");
    }
    let l = store::open_read(&cron_dir(&serve)).unwrap();
    assert_eq!(l.state().jobs().filter(|j| !j.job.removed).count(), 2);
    // A read-only server stops a job but does not start one.
    assert_eq!(c.pause_cron_job("nightly").unwrap().state, "paused");
    assert_eq!(status_of(c.resume_cron_job("nightly")), 409);
}

/// A path that is only the suffix of a job route (`/cron/jobs/pause`) starts
/// with the prefix and ends with the suffix at once, with no room for an id
/// between them. It is an unknown route (404), not a request to pause a job
/// with an empty name, and the server keeps answering: slicing it by the two
/// lengths made a reversed range and panicked the connection's thread.
#[test]
fn a_path_that_is_only_a_route_suffix_is_not_found_and_does_not_panic() {
    let (serve, _) = serve(&[]);
    for path in ["/cron/jobs/pause", "/cron/jobs/resume"] {
        let (code, body) = serve.http("POST", path, None);
        assert_eq!(code, 404, "POST {path}: {body}");
    }
    // An empty id between the two is a job that does not exist.
    for path in ["/cron/jobs//pause", "/cron/jobs//resume"] {
        let (code, body) = serve.http("POST", path, None);
        assert_eq!(code, 404, "POST {path}: {body}");
    }
    let (code, _) = serve.http("GET", "/health", None);
    assert_eq!(code, 200);
}
