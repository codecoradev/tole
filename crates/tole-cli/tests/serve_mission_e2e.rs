//! #947 missions over the REAL serve face: `POST /missions` runs the CLI's
//! mission loop on a thread, `GET /missions/{id}` reports it,
//! `POST /missions/{id}/cancel` stops it. The verify command is the owner's
//! configuration and never a request field.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::time::{Duration, Instant};

mod common;
mod serve_harness;
use serve_harness::{spawn_mock, Serve};

fn poll_until_ended(serve: &Serve, id: &str) -> Value {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let (code, v) = serve.http("GET", &format!("/missions/{id}"), None);
        assert_eq!(code, 200, "{v}");
        if v["running"] == false {
            return v;
        }
        assert!(Instant::now() < end, "mission did not end: {v}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn post(serve: &Serve, body: Value) -> (u16, Value) {
    serve.http("POST", "/missions", Some(&body.to_string()))
}

/// A mission runs, ends on the step budget and leaves an ordinary session
/// file with the `fact/mission` register; a second one is allowed after it.
#[test]
fn a_mission_runs_to_its_budget_and_reports_its_summary() {
    let serve = Serve::spawn(&spawn_mock(0));
    let (code, v) = post(&serve, json!({"goal": "tidy up", "max_steps": 2}));
    assert_eq!(code, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("mission-"));
    assert_eq!(v["status"], "running");

    let done = poll_until_ended(&serve, &id);
    assert_eq!(done["status"], "exhausted_steps", "{done}");
    assert!(done["turns"].as_u64().unwrap() >= 1);
    assert!(done["steps"].as_u64().unwrap() >= 2);
    assert_eq!(done["summary"]["goal"], "tidy up");
    assert_eq!(done["summary"]["budget"]["max_steps"], 2);
    // The session file is on disk and carries the same register.
    let s = tole_core::storage::JsonlStorage::open_read_only(serve.session_file(&id)).unwrap();
    assert_eq!(
        tole_core::storage::Storage::get_register(&s, "fact", "mission").unwrap()["status"],
        "exhausted_steps"
    );
    // The next one is accepted.
    let (code, _) = post(&serve, json!({"goal": "again", "max_steps": 1}));
    assert_eq!(code, 202);
}

/// Bad bodies are 400s and start nothing; unknown missions are 404; an
/// over-ceiling budget is refused.
#[test]
fn bad_bodies_are_400_and_unknown_missions_404() {
    let serve = Serve::spawn(&spawn_mock(0));
    for body in [
        json!({}),
        json!({"goal": ""}),
        json!({"goal": 5}),
        json!({"goal": "x", "max_steps": 0}),
        json!({"goal": "x", "max_steps": "3"}),
        json!({"goal": "x", "max_minutes": 100000}),
        json!({"goal": "x", "max_tokens": 99999999999u64}),
        json!({"goal": "x", "cwd": "/"}),
        json!({"goal": "x", "profile": "p"}),
        json!({"goal": "x", "allow": ["*"]}),
    ] {
        let (code, v) = post(&serve, body.clone());
        assert_eq!(code, 400, "{body}: {v}");
    }
    let (code, _) = serve.http("POST", "/missions", Some("not json"));
    assert_eq!(code, 400);
    let (code, _) = serve.http("GET", "/missions/mission-nope", None);
    assert_eq!(code, 404);
    let (code, _) = serve.http("GET", "/missions/..%2Fx", None);
    assert_eq!(code, 404);
    let (code, _) = serve.http("POST", "/missions/mission-nope/cancel", None);
    assert_eq!(code, 404);
    // Nothing was started by any of the above.
    let dir = serve.dir.join(".tole/sessions");
    let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(n, 0, "a refused request must not leave a session file");
}

/// A slow mission is `running`, a second is 409, cancel stops it, and a
/// finished one cannot be cancelled again (409). Needs the token.
#[test]
fn one_mission_at_a_time_and_cancel_ends_it() {
    let serve = Serve::spawn(&spawn_mock(60_000));
    let (code, _) = serve.http_with("POST", "/missions", false, Some("{\"goal\":\"slow\"}"));
    assert_eq!(code, 401);
    let (code, v) = post(&serve, json!({"goal": "slow work"}));
    assert_eq!(code, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let (code, v) = serve.http("GET", &format!("/missions/{id}"), None);
    assert_eq!(
        (code, v["status"].clone(), v["running"].clone()),
        (200, json!("running"), json!(true))
    );
    let (code, v) = post(&serve, json!({"goal": "second"}));
    assert_eq!(code, 409, "{v}");

    let (code, v) = serve.http("POST", &format!("/missions/{id}/cancel"), None);
    assert_eq!(code, 202, "{v}");
    let done = poll_until_ended(&serve, &id);
    assert_eq!(done["status"], "stopped", "{done}");
    let (code, _) = serve.http("POST", &format!("/missions/{id}/cancel"), None);
    assert_eq!(code, 409);
}

/// A write the allow list does not cover queues in the existing approvals
/// queue under the mission's id, and the mission stops there.
#[test]
fn a_mission_write_waits_in_the_serve_approvals_queue() {
    let serve = Serve::spawn(&spawn_mock(0));
    let (code, v) = post(&serve, json!({"goal": "TOOL_MISSION go"}));
    assert_eq!(code, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let done = poll_until_ended(&serve, &id);
    assert_eq!(done["status"], "stopped", "{done}");
    let (code, v) = serve.http("GET", &format!("/approvals?session={id}"), None);
    assert_eq!(code, 200);
    let rows = v["approvals"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["tool"], "write_file");
    assert!(!serve.dir.join("approved.txt").exists());
}

/// The verify command comes from the owner's user config: a `verify` in the
/// body is a 400, and the configured one gates the mission (`false` never
/// passes, so the mission ends `verify_failed` after three strikes).
#[test]
fn the_verify_command_is_owner_configuration_never_a_request_field() {
    let home = common::TestDir::new("tole-mission-serve-home");
    let cfg = home.join("tole");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "[mission]\nverify = \"false\"\n\n[sandbox.consumers]\nverify = \"off\"\n",
    )
    .unwrap();
    let h = home.path().to_str().unwrap().to_string();
    let serve = Serve::spawn_with_env(
        &spawn_mock(0),
        &[("CODECORA_HOME", &h), ("HOME", &h), ("TOLE_STREAM", "0")],
    );
    // A request cannot name, override or blank it.
    for verify in [json!("true"), json!(""), json!(null), json!(false)] {
        let (code, v) = post(&serve, json!({"goal": "x", "verify": verify}));
        assert_eq!(code, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("verify"), "{v}");
    }
    assert!(
        !serve.dir.join(".tole/sessions").exists()
            || std::fs::read_dir(serve.dir.join(".tole/sessions"))
                .unwrap()
                .count()
                == 0
    );
    // The owner's command runs: the mock claims no completion and `false`
    // fails every time.
    let (code, v) = post(&serve, json!({"goal": "check"}));
    assert_eq!(code, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let done = poll_until_ended(&serve, &id);
    assert_eq!(done["status"], "verify_failed", "{done}");
    assert_eq!(done["summary"]["verify_failures"], 3);
}
