//! #1314: the `error` text of a `tole serve` 500 is redacted with the egress
//! secret set and cut, like the `detail` of the `problem` field. Over the REAL
//! serve face.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

mod common;
mod serve_harness;
use serve_harness::{new_session, spawn_mock, Serve, LEAKED};

/// A storage error that echoes a value of the log: a session file that no
/// longer opens names the offending header value in its message.
#[test]
fn a_secret_in_a_storage_error_never_leaves_in_a_500() {
    let env = [("TOLE_TEST_API_KEY", LEAKED)];
    let serve = Serve::spawn_with_env(&spawn_mock(0), &env);
    let id = new_session(&serve);
    std::fs::write(serve.session_file(&id), format!("\"{LEAKED}\"\n")).unwrap();
    let (code, body) = serve.http("GET", &format!("/sessions/{id}/entries"), None);
    assert_eq!(code, 500, "{body}");
    let error = body["error"].as_str().expect("the error string");
    assert!(error.contains("corrupt session file"), "{body}");
    assert!(
        !body.to_string().contains(LEAKED),
        "the secret left: {body}"
    );
    assert!(!error.contains("sk-leak-"), "a head of it is left: {body}");
    // The shape of the answer is unchanged: one `error` string.
    assert_eq!(body.as_object().map(|o| o.len()), Some(1), "{body}");
}

/// Every status, not only 500: a 4xx that echoes a path holding a secret
/// (the workspace of a session the client named) is redacted as well.
#[test]
fn a_secret_in_a_4xx_error_never_leaves() {
    let env = [("TOLE_TEST_API_KEY", LEAKED)];
    let serve = Serve::spawn_with_env(&spawn_mock(0), &env);
    let cwd = serve.dir.join(LEAKED).join("gone");
    let body = serde_json::json!({"cwd": cwd.to_string_lossy()}).to_string();
    let (code, answer) = serve.http("POST", "/sessions", Some(&body));
    assert!((400..500).contains(&code), "{code}: {answer}");
    assert!(answer["error"].as_str().is_some(), "{answer}");
    assert!(
        !answer.to_string().contains(LEAKED),
        "the secret left: {answer}"
    );
}

/// A mission that fails to start its session directory is a 200 with
/// `status: error`, not a 500: its `error` text is redacted too.
#[test]
fn a_secret_in_a_mission_error_never_leaves() {
    let ws = serve_harness::workspace_dir().join(LEAKED);
    std::fs::create_dir_all(&ws).unwrap();
    // `.tole` is a file, so the session directory cannot be created.
    std::fs::write(ws.join(".tole"), "").unwrap();
    let env = [("TOLE_TEST_API_KEY", LEAKED)];
    let serve = Serve::spawn_in_env(&spawn_mock(0), ws, true, &env);
    let post = serde_json::json!({"goal": "tidy up"}).to_string();
    let (code, started) = serve.http("POST", "/missions", Some(&post));
    assert_eq!(code, 202, "{started}");
    let id = started["id"].as_str().unwrap();
    let path = format!("/missions/{id}");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let done = loop {
        let (code, v) = serve.http("GET", &path, None);
        assert_eq!(code, 200, "{v}");
        if v["running"] == false {
            break v;
        }
        assert!(std::time::Instant::now() < end, "mission did not end: {v}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_eq!(done["status"], "error", "{done}");
    assert!(done["error"].as_str().is_some(), "{done}");
    assert!(
        !done.to_string().contains(LEAKED),
        "the secret left: {done}"
    );
}
