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
