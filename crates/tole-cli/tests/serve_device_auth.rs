//! Device tokens (#1320, part 2): the authentication of `tole serve` with them. A revoked token is
//! refused on the next request, a wrong, unknown and revoked token get one and the same answer, the
//! failed-auth limiter counts them, the shared token and Basic answer as before, and no token
//! reaches a log or a body.

#![cfg(feature = "shell-tools")]

use tole_cli::auth::PasswordHash;
use tole_cli::serve_support::device_tokens::{Scope, PREFIX};

mod common;
mod serve_harness;
use serve_harness::raw::{
    device_token, json_of, raw, raw_declared, spawn_logged, status_of, store_of,
};
use serve_harness::{spawn_mock, Serve};

const UNKNOWN: &str = "tole_dt_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn bearer(t: &str) -> String {
    format!("Bearer {t}")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn a_revoked_token_is_refused_on_the_next_request() {
    let serve = Serve::spawn(&spawn_mock(0));
    let t = bearer(&device_token(&serve, "phone", Scope::Full));
    assert_eq!(
        status_of(&raw(serve.port, "GET", "/sessions", Some(&t), "")),
        200
    );
    store_of(&serve).revoke("phone", now()).unwrap();
    let a = raw(serve.port, "GET", "/sessions", Some(&t), "");
    assert_eq!(status_of(&a), 401, "{a}");
    // A token made after the server started works without a restart.
    let t2 = bearer(&device_token(&serve, "tablet", Scope::ReadOnly));
    assert_eq!(
        status_of(&raw(serve.port, "GET", "/sessions", Some(&t2), "")),
        200
    );
}

/// The shared token's answers are the bytes they were before device tokens: nothing in them says a
/// device token exists, and a wrong, unknown, revoked or malformed credential gets one 401.
#[test]
fn the_shared_token_and_every_refusal_answer_as_before() {
    let serve = Serve::spawn(&spawn_mock(0));
    let revoked = bearer(&device_token(&serve, "old", Scope::Full));
    store_of(&serve).revoke("old", now()).unwrap();
    let live = bearer(&device_token(&serve, "phone", Scope::ReadOnly));

    let unauthorized = "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\n\
                        Content-Length: 24\r\nConnection: close\r\n\r\n{\"error\":\"unauthorized\"}";
    for credential in [
        None,
        Some("Bearer wrong".to_string()),
        Some("Bearer t0ken-and-more".to_string()),
        Some(UNKNOWN.to_string()),
        Some(bearer(UNKNOWN)),
        Some(revoked),
        Some(bearer(&format!("{PREFIX}short"))),
        Some(format!("Basic {}", live.trim_start_matches("Bearer "))),
        Some(live.replace("Bearer", "Token")),
    ] {
        let a = raw(serve.port, "GET", "/sessions", credential.as_deref(), "");
        assert_eq!(a, unauthorized, "{credential:?}");
    }
    // The shared token, any case of the scheme, and `/health` without a credential.
    let ok = raw(serve.port, "GET", "/sessions", Some("Bearer t0ken"), "");
    assert_eq!(
        ok,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\
         Connection: close\r\n\r\n{\"sessions\":[]}"
    );
    assert_eq!(
        status_of(&raw(
            serve.port,
            "GET",
            "/sessions",
            Some("bEaReR t0ken"),
            ""
        )),
        200
    );
    assert_eq!(status_of(&raw(serve.port, "GET", "/health", None, "")), 200);
    assert_eq!(
        status_of(&raw(serve.port, "GET", "/health", Some(&live), "")),
        200
    );
}

/// A `full` device token does what the shared token does: open a session, run a turn, read the
/// transcript, upload an image.
#[test]
fn a_full_device_token_does_what_the_shared_token_does() {
    let serve = Serve::spawn(&spawn_mock(0));
    let t = bearer(&device_token(&serve, "phone", Scope::Full));
    let call = |m: &str, p: &str, body: &str| raw(serve.port, m, p, Some(&t), body);
    let opened = call("POST", "/sessions", "{}");
    assert_eq!(status_of(&opened), 200, "{opened}");
    let id = json_of(&opened)["sessionId"].as_str().unwrap().to_string();
    let turn = call(
        "POST",
        &format!("/sessions/{id}/prompt"),
        "{\"text\":\"hello\"}",
    );
    assert_eq!(status_of(&turn), 200, "{turn}");
    let entries = call("GET", &format!("/sessions/{id}/entries"), "");
    assert_eq!(status_of(&entries), 200, "{entries}");
    // Past the 1 MiB body ceiling of everything else: only a token that may write gets the upload
    // ceiling, so this is the image route's own answer, not a 413.
    let big = "x".repeat(1_100_000);
    let upload = call("POST", &format!("/sessions/{id}/images"), &big);
    assert!(
        ![401, 403, 413].contains(&status_of(&upload)),
        "{}",
        upload.chars().take(80).collect::<String>()
    );
    let ro = bearer(&device_token(&serve, "watch", Scope::ReadOnly));
    // Declared only: the server answers 413 from the header and closes, so the body is not sent.
    let refused = raw_declared(
        serve.port,
        "POST",
        &format!("/sessions/{id}/images"),
        Some(&ro),
        big.len(),
    );
    assert_eq!(
        status_of(&refused),
        413,
        "no larger ceiling for a read-only token"
    );
}

/// The limiter counts device-token failures like any other: ten of them and the address is refused
/// with 429, a valid token included, as it is for a wrong static token.
#[test]
fn failed_device_tokens_count_towards_the_limiter() {
    let serve = Serve::spawn(&spawn_mock(0));
    let good = bearer(&device_token(&serve, "phone", Scope::Full));
    for n in 0..tole_cli::auth::MAX_AUTH_FAILURES {
        let a = raw(serve.port, "GET", "/sessions", Some(&bearer(UNKNOWN)), "");
        assert_eq!(status_of(&a), 401, "failure {n}");
    }
    let a = raw(serve.port, "GET", "/sessions", Some(&good), "");
    assert_eq!(status_of(&a), 429, "{a}");
    assert_eq!(json_of(&a)["error"], "too many failed auth attempts");
}

/// With `--auth basic` the device tokens work next to the password, a refusal carries the same
/// challenge as before, and the scopes hold.
#[test]
fn device_tokens_work_next_to_basic() {
    use base64::Engine;
    let hash = PasswordHash::create(b"correct horse battery", 10_000).unwrap();
    let dir = serve_harness::workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let serve = Serve::spawn_in_env_args(
        &spawn_mock(0),
        dir,
        true,
        &[("TOLE_SERVE_BASIC_HASH", hash.as_str())],
        &["--auth", "basic", "--basic-user", "owner"],
    );
    let ro = bearer(&device_token(&serve, "watch", Scope::ReadOnly));
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("owner:correct horse battery")
    );
    let info = raw(serve.port, "GET", "/info", Some(&basic), "");
    assert_eq!(json_of(&info)["scope"], "full", "{info}");
    let info = raw(serve.port, "GET", "/info", Some(&ro), "");
    assert_eq!(json_of(&info)["scope"], "read-only", "{info}");
    let a = raw(serve.port, "POST", "/sessions", Some(&ro), "{}");
    assert_eq!(status_of(&a), 403, "{a}");
    let a = raw(serve.port, "POST", "/sessions", Some(&basic), "{}");
    assert_eq!(status_of(&a), 200, "{a}");

    let wrong_basic = raw(
        serve.port,
        "GET",
        "/info",
        Some("Basic b3duZXI6bm9wZQ=="),
        "",
    );
    assert_eq!(status_of(&wrong_basic), 401);
    assert!(
        wrong_basic.contains("WWW-Authenticate: Basic realm=\"tole\""),
        "{wrong_basic}"
    );
    for credential in [bearer(UNKNOWN), bearer("wrong")] {
        assert_eq!(
            raw(serve.port, "GET", "/info", Some(&credential), ""),
            wrong_basic
        );
    }
}

/// No token value reaches the serve's log or a body, however it was presented.
#[test]
fn no_token_value_reaches_a_log_or_a_body() {
    let serve = spawn_logged(&spawn_mock(0));
    let (store, port) = (serve.store(), serve.port);

    let secret = |name, scope| {
        store
            .add(name, scope, now())
            .unwrap()
            .secret
            .expose()
            .to_string()
    };
    let (ro, full, old) = (
        secret("watch", Scope::ReadOnly),
        secret("phone", Scope::Full),
        secret("old", Scope::Full),
    );
    store.revoke("old", now()).unwrap();
    let mut answers = String::new();
    for (token, method, path) in [
        (&ro, "GET", "/sessions"),
        (&ro, "POST", "/sessions"),
        (&ro, "GET", "/info"),
        (&full, "GET", "/info"),
        (&full, "POST", "/approvals/nope/decision"),
        (&old, "GET", "/sessions"),
        (&old, "POST", "/sessions"),
    ] {
        answers.push_str(&raw(port, method, path, Some(&bearer(token)), "{}"));
    }
    let logged = serve.text();
    assert!(logged.contains("listening on"));
    for (name, token) in [("read-only", &ro), ("full", &full), ("revoked", &old)] {
        assert!(
            !answers.contains(token.as_str()),
            "{name} token in an answer"
        );
        assert!(!logged.contains(token.as_str()), "{name} token in the log");
    }
}

fn said(serve: &serve_harness::raw::Logged) -> usize {
    serve.text().matches("device tokens are refused").count()
}

/// A token file that cannot be read refuses device tokens and is said on stderr once per change of
/// state, and only when a token of the right shape asks: a wrong-shape token never reaches the file.
#[test]
fn a_damaged_token_file_is_said_once_per_state_and_only_to_a_device_token() {
    let serve = spawn_logged(&spawn_mock(0));
    let port = serve.port;
    let store = serve.store();
    let good = bearer(
        store
            .add("phone", Scope::Full, now())
            .unwrap()
            .secret
            .expose(),
    );
    let file = serve.home.join("tole/serve-tokens.jsonl");
    let healthy = std::fs::read_to_string(&file).unwrap();
    let damage = || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        f.write_all(b"oops\n").unwrap();
    };
    let get = |credential: &str| status_of(&raw(port, "GET", "/sessions", Some(credential), ""));

    damage();
    // A mistyped shared token or a wrong-shape bearer costs no look at the file.
    assert_eq!(get("Bearer t0ken-typo"), 401);
    assert_eq!(get("Bearer wrong"), 401);
    assert_eq!(said(&serve), 0, "{}", serve.text());
    // A token of the right shape is refused, and the state is said once.
    assert_eq!(get(&good), 401);
    assert_eq!(get(&good), 401);
    assert_eq!(get(&bearer(UNKNOWN)), 401);
    assert_eq!(said(&serve), 1, "{}", serve.text());
    // Mended: the token works again. Damaged the same way again: said again.
    std::fs::write(&file, &healthy).unwrap();
    assert_eq!(get(&good), 200);
    damage();
    assert_eq!(get(&good), 401);
    assert_eq!(said(&serve), 2, "{}", serve.text());
}
