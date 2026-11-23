//! The typed client percent-encodes every path segment and query value it
//! sends; `tole serve` decodes the ones it reads as an id, after routing, and
//! strictly. A cron job named `weekly report` is reachable, a malformed escape
//! is a 400, and an encoded `/` cannot add a segment or reach another route.

#![cfg(feature = "shell-tools")]

use serde_json::Value;
use tole_cli::cron::{self, CreateSpec, TickOptions};
use tole_client::{Client, Error};

mod common;
mod serve_harness;
use serve_harness::{home_of, new_session, spawn_mock, workspace_dir, Serve};

/// `(id, name)`: a job id is `[A-Za-z0-9._-]` (the ledger refuses any other),
/// so what needs encoding on the wire is a NAME, which `GET /cron/runs?job=`
/// takes in place of an id and which is free text.
const JOBS: [(&str, &str); 4] = [
    ("weekly", "weekly report"),
    ("unicode", "wéekly отчёт ✓"),
    ("plus", "a+b"),
    ("my.job_1-x", "100%"),
];

fn serve() -> Serve {
    let dir = workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let cdir = home_of(&dir).join("tole/cron");
    for (id, name) in JOBS {
        let spec = CreateSpec {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            schedule: "0 9 * * *".into(),
            script: Some(format!("echo {id}")),
            workdir: dir.clone(),
            ..CreateSpec::default()
        };
        cron::create(&cdir, spec, cron::now()).unwrap();
    }
    Serve::spawn_in_env_args(&spawn_mock(0), dir, true, &[], &[])
}

fn client(serve: &Serve) -> Client {
    let url = format!("http://127.0.0.1:{}", serve.port);
    Client::connect_tcp(&url, Some("t0ken".into()), false).unwrap()
}

fn status_of<T: std::fmt::Debug>(r: Result<T, Error>) -> u16 {
    match r {
        Err(Error::Status { status, .. }) => status,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn error_of(body: &Value) -> String {
    body["error"].as_str().unwrap_or_default().to_string()
}

#[test]
fn a_job_named_with_spaces_or_non_ascii_text_is_read_through_the_client() {
    let serve = serve();
    let c = client(&serve);
    let cdir = home_of(&serve.dir).join("tole/cron");
    // One run each, so a job's history is told apart from the others'.
    for (id, _) in JOBS {
        let r = cron::run_now(&cdir, id, cron::now(), &TickOptions::standalone(1)).unwrap();
        assert_eq!(r.ran.len(), 1, "{id}");
    }
    for (id, name) in JOBS {
        let runs = c.cron_runs(Some(name), Some(5)).unwrap();
        assert_eq!(runs.runs.len(), 1, "{name}");
        assert_eq!(runs.runs[0].output, format!("{id}\n"), "{name}");
        // The same job by its id, and by its name in another case.
        assert_eq!(c.cron_runs(Some(id), None).unwrap().runs.len(), 1);
        let upper = name.to_ascii_uppercase();
        assert_eq!(c.cron_runs(Some(&upper), None).unwrap().runs.len(), 1);
    }
    // A name that is not there is still a 404, encoded or not.
    assert_eq!(status_of(c.cron_runs(Some("no such job"), None)), 404);
    // The same request by hand: a space is `%20`, and a plus is a plus.
    let (code, v) = serve.http("GET", "/cron/runs?job=weekly%20report&limit=5", None);
    assert_eq!(
        (code, v["runs"].as_array().map(Vec::len)),
        (200, Some(1)),
        "{v}"
    );
    let (code, v) = serve.http("GET", "/cron/runs?job=a+b", None);
    assert_eq!(
        (code, v["runs"].as_array().map(Vec::len)),
        (200, Some(1)),
        "{v}"
    );
    let (code, v) = serve.http("GET", "/cron/runs?job=a%2Bb", None);
    assert_eq!(
        (code, v["runs"].as_array().map(Vec::len)),
        (200, Some(1)),
        "{v}"
    );
    // The query is split before it is decoded: an encoded `&` and `=` stay
    // inside the one value instead of adding a parameter.
    let (code, v) = serve.http("GET", "/cron/runs?job=weekly%20report%26limit%3D1", None);
    assert_eq!(code, 404, "{v}");
}

#[test]
fn a_job_is_paused_and_resumed_by_the_encoded_form_of_its_id() {
    let serve = serve();
    let c = client(&serve);
    for (id, _) in JOBS {
        let paused = c.pause_cron_job(id).unwrap();
        assert_eq!((paused.id.as_str(), paused.state.as_str()), (id, "paused"));
        let resumed = c.resume_cron_job(id).unwrap();
        assert_eq!(
            (resumed.id.as_str(), resumed.state.as_str()),
            (id, "active")
        );
    }
    // By hand, with the id spelled in escapes (`%77` is `w`, `%2E` a dot).
    let (code, v) = serve.http("POST", "/cron/jobs/%77eekly/pause", None);
    assert_eq!((code, v["id"].as_str()), (200, Some("weekly")), "{v}");
    let (code, v) = serve.http("POST", "/cron/jobs/my%2Ejob%5F1%2Dx/pause", None);
    assert_eq!((code, v["id"].as_str()), (200, Some("my.job_1-x")), "{v}");
    let (code, v) = serve.http("POST", "/cron/jobs/%77eekly/resume", None);
    assert_eq!((code, v["state"].as_str()), (200, Some("active")), "{v}");
    // A job is addressed by its id, never by its name, encoded or not.
    let (code, v) = serve.http("POST", "/cron/jobs/weekly%20report/pause", None);
    assert_eq!(code, 404, "{v}");
    // A job that is not there is a 404.
    assert_eq!(status_of(c.pause_cron_job("no such job")), 404);
}

#[test]
fn a_decoded_value_is_decoded_once() {
    let serve = serve();
    // `100%` is a job NAME: `100%25` names it, `100%2525` names `100%25`, which is none.
    let (code, v) = serve.http("GET", "/cron/runs?job=100%25", None);
    assert_eq!(code, 200, "{v}");
    let (code, v) = serve.http("GET", "/cron/runs?job=100%2525", None);
    assert_eq!(code, 404, "{v}");
    assert!(error_of(&v).contains("100%25"), "{v}");
}

#[test]
fn a_malformed_escape_or_invalid_utf8_is_a_400_and_the_server_keeps_answering() {
    let serve = serve();
    for path in [
        "/cron/jobs/%/pause",
        "/cron/jobs/a%2/pause",
        "/cron/jobs/%zz/pause",
        "/cron/jobs/%ff/pause",
        "/cron/jobs/%C0%AF/pause",
    ] {
        let (code, v) = serve.http("POST", path, None);
        assert_eq!(code, 400, "POST {path}: {v}");
    }
    for path in [
        "/cron/runs?job=%zz",
        "/cron/runs?job=%ff",
        "/approvals?session=%2",
        "/sessions/%zz",
        "/sessions/%ff/status",
        "/missions/%zz",
        "/discussions/%zz",
        "/sessions/%ff/events",
        "/sessions/%zz/images/abc",
    ] {
        let (code, v) = serve.http("GET", path, None);
        assert_eq!(code, 400, "GET {path}: {v}");
    }
    for path in [
        "/missions/%zz/cancel",
        "/discussions/%zz/resume",
        "/discussions/%ff/stop",
        "/approvals/%zz/decision",
        "/sessions/%zz/prompt",
        "/sessions/%zz/images",
    ] {
        let (code, v) = serve.http("POST", path, Some("{}"));
        assert_eq!(code, 400, "POST {path}: {v}");
    }
    let (code, _) = serve.http("GET", "/health", None);
    assert_eq!(code, 200);
}

#[test]
fn an_encoded_slash_cannot_add_a_segment_or_reach_another_route() {
    let serve = serve();
    let states = |serve: &Serve| -> Vec<(String, String)> {
        let (_, v) = serve.http("GET", "/cron/jobs", None);
        let mut rows: Vec<_> = v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| (j["id"].to_string(), j["state"].to_string()))
            .collect();
        rows.sort();
        rows
    };
    let before = states(&serve);
    assert_eq!(before.len(), JOBS.len());
    // A value that decodes to a `/`, a NUL or a control character names
    // nothing: it is not decoded, and the route answers its usual "no such".
    for path in [
        "/cron/jobs/%2e%2e%2f/pause",
        "/cron/jobs/%2e%2e%2fhealth/pause",
        "/cron/jobs/weekly%2Fresume/pause",
        "/cron/jobs/a%2Fb/resume",
        "/cron/jobs/%2F/pause",
        "/cron/jobs/%00/resume",
        "/cron/jobs/a%0Ab/pause",
        "/cron/jobs/%7F/pause",
    ] {
        let (code, v) = serve.http("POST", path, None);
        assert_eq!(code, 404, "POST {path}: {v}");
        assert!(v.get("state").is_none(), "{path}: {v}");
    }
    // A decoded `/` is not read as the end of the id: no other route answers.
    for (path, expected) in [
        ("/sessions/%2e%2e%2fhealth", 404),
        ("/sessions/%2e%2e%2fhealth/status", 404),
        ("/sessions/x%2Fcancel", 404),
        ("/missions/%2e%2e%2fdiscussions", 404),
        ("/discussions/%2e%2e%2fresume", 400),
        ("/cron/runs?job=%2e%2e%2fjobs", 404),
        ("/cron/runs?job=a%00", 404),
    ] {
        let (code, v) = serve.http("GET", path, None);
        assert_eq!(code, expected, "GET {path}: {v}");
        assert!(
            v.get("status").is_none() && v.get("jobs").is_none() && v.get("runs").is_none(),
            "{path}: {v}"
        );
    }
    // The approvals list filtered by such a session is the empty list.
    let (code, v) = serve.http("GET", "/approvals?session=%2e%2e%2f", None);
    assert_eq!(
        (code, v["approvals"].as_array().map(Vec::len)),
        (200, Some(0))
    );
    // The dots alone are an id no route accepts: refused or not found, never a hit.
    let (code, _) = serve.http("GET", "/sessions/%2e%2e", None);
    assert_eq!(code, 404);
    let (code, _) = serve.http("POST", "/cron/jobs/%2e%2e/pause", None);
    assert_eq!(code, 404);
    let (code, _) = serve.http("GET", "/discussions/%2e%2e", None);
    assert_eq!(code, 400);
    // Nothing was changed by any of it.
    assert_eq!(states(&serve), before);
}

#[test]
fn an_encoded_form_of_a_valid_session_id_is_that_session_and_nothing_else_becomes_valid() {
    let serve = serve();
    let id = new_session(&serve);
    // Every character encoded: the same session.
    let all: String = id.bytes().map(|b| format!("%{b:02X}")).collect();
    for path in [
        format!("/sessions/{all}"),
        format!("/sessions/{all}/status"),
    ] {
        let (code, v) = serve.http("GET", &path, None);
        assert_eq!(
            (code, v["id"].as_str()),
            (200, Some(id.as_str())),
            "{path}: {v}"
        );
    }
    let (code, _) = serve.http("GET", &format!("/approvals?session={all}"), None);
    assert_eq!(code, 200);
    let c = client(&serve);
    assert_eq!(c.status(&id).unwrap().id, id);
    // A trailing byte that is not part of the id makes it another one.
    let (code, v) = serve.http("GET", &format!("/sessions/{all}%20"), None);
    assert_eq!(code, 404, "{v}");
    let (code, v) = serve.http("GET", &format!("/sessions/{all}%00"), None);
    assert_eq!(code, 404, "{v}");
    let (code, v) = serve.http("GET", &format!("/sessions/{all}%252F"), None);
    assert_eq!(code, 404, "{v}");
}
