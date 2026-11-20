//! #572 `GET /sessions/index` over the REAL serve face: past sessions after
//! a restart with name, folder and last activity, newest first and paged;
//! the token is required; no transcript text; an id from the list resumes.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::Value;

mod common;
mod serve_harness;
use serve_harness::{new_session, prompt, spawn_mock, Serve};

#[test]
fn past_sessions_survive_a_restart_page_and_resume() {
    let base = spawn_mock(0);
    let mut first = Serve::spawn(&base);
    let a = new_session(&first);
    prompt(&first, &a, "UNIQUE-PROMPT-TEXT-572");
    std::thread::sleep(std::time::Duration::from_millis(30));
    let b = new_session(&first);
    prompt(&first, &b, "second");
    first.kill();

    let second = Serve::spawn_in(&base, first.dir.clone(), false);
    // Not live after the restart, yet listed.
    assert_eq!(
        second.http("GET", "/sessions", None).1["sessions"],
        Value::Array(vec![])
    );
    let (code, v) = second.http("GET", "/sessions/index", None);
    assert_eq!(code, 200, "{v}");
    let ids: Vec<&str> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [b.as_str(), a.as_str()], "newest first: {v}");
    let row = &v["sessions"][1];
    assert_eq!(row["live"], false);
    assert!(row["lastActivity"].as_u64().unwrap() > 0);
    assert!(row["turns"].as_u64().unwrap() >= 1);
    assert!(row["folder"].is_string());
    assert!(
        !v.to_string().contains("UNIQUE-PROMPT-TEXT-572"),
        "no transcript text: {v}"
    );

    // Paging with a one-row page, then a bad cursor.
    let (_, p1) = second.http("GET", "/sessions/index?limit=1", None);
    assert_eq!(p1["sessions"][0]["id"], b.as_str());
    let cur = p1["nextCursor"].as_str().unwrap();
    let (_, p2) = second.http(
        "GET",
        &format!("/sessions/index?limit=1&cursor={cur}"),
        None,
    );
    assert_eq!(p2["sessions"][0]["id"], a.as_str());
    assert!(p2["nextCursor"].is_null());
    assert_eq!(
        second
            .http("GET", "/sessions/index?cursor=../../etc", None)
            .0,
        400
    );

    // An id from the list resumes.
    let (code, r) = second.http("POST", "/sessions", Some(&format!(r#"{{"resume":"{a}"}}"#)));
    assert_eq!(code, 200, "{r}");
    let (_, v) = second.http("GET", "/sessions/index", None);
    let live: Vec<bool> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["live"].as_bool().unwrap())
        .collect();
    assert_eq!(live, [false, true]);
}

#[test]
fn the_index_needs_the_token() {
    let serve = Serve::spawn(&spawn_mock(0));
    for path in ["/sessions/index", "/sessions/index?limit=1"] {
        assert_eq!(serve.http_with("GET", path, false, None).0, 401, "{path}");
    }
    assert_eq!(serve.http("GET", "/sessions/index", None).0, 200);
}
