//! Device tokens (#1320, part 2): what a `read-only` token may do on the REAL `tole serve`,
//! checked over the whole route table, so a new route cannot be forgotten.
//!
//! The rule under test: a read-only token may `GET` the routes in `READS` and nothing else; every
//! other method, every path that is not listed and every route added later without a line in
//! `src/serve/scope.rs` is refused with `403 {"error": "this token is read-only"}` before the
//! route runs. No POST is a read, so no POST is let through.

#![cfg(feature = "shell-tools")]

use serde_json::json;
use tole_cli::serve_support::device_tokens::Scope;

mod common;
mod serve_harness;
use serve_harness::raw::{device_token, json_of, raw, status_of};
use serve_harness::routes::SERVE_ROUTES;
use serve_harness::{new_session, prompt, spawn_mock, Serve};

const REFUSED: &str = "this token is read-only";

/// The GET routes a read-only token may call.
const READS: &[&str] = &[
    "/health",
    "/info",
    "/ready",
    "/models",
    "/sessions",
    "/sessions/index",
    "/sessions/{id}",
    "/sessions/{id}/status",
    "/sessions/{id}/entries",
    "/sessions/{id}/events",
    "/sessions/{id}/tree",
    "/sessions/{id}/images/{hash}",
    "/approvals",
    "/missions/{id}",
    "/discussions",
    "/discussions/{id}",
    "/discussions/{id}/events",
    "/profiles",
    "/skills",
    "/tools",
    "/cron/jobs",
    "/cron/runs",
];

/// GET routes a read-only token may NOT call (a GET that changes state would be listed here).
/// None today. A GET route in neither list fails `every_route_is_classified`.
const GET_REFUSED: &[&str] = &[];

/// Signed by the sender, not by a token: the token is never read there.
fn is_hook(path: &str) -> bool {
    path.starts_with("/hooks/")
}

fn is_read(method: &str, path: &str) -> bool {
    method == "GET" && READS.contains(&path)
}

fn fill(template: &str, session: &str, approval: &str) -> String {
    let id = if template.starts_with("/approvals/") {
        approval
    } else if template.starts_with("/sessions/") {
        session
    } else {
        "nope"
    };
    template
        .replace("{id}", id)
        .replace("{hash}", "0123456789abcdef")
        .replace("{name}", "nope")
}

fn bearer(t: &str) -> String {
    format!("Bearer {t}")
}

#[test]
fn every_route_is_classified() {
    for (_, p) in SERVE_ROUTES.iter().filter(|(m, _)| *m == "GET") {
        let listed = (READS.contains(p) as u8) + (GET_REFUSED.contains(p) as u8);
        assert_eq!(
            listed, 1,
            "GET {p} is in neither READS nor GET_REFUSED here (decide: may a read-only token \
             call it? then add it to READ in src/serve/scope.rs too)"
        );
    }
    for p in READS.iter().chain(GET_REFUSED) {
        assert!(
            SERVE_ROUTES.contains(&("GET", *p)),
            "READS names `GET {p}`, which serve does not have"
        );
    }
    assert!(
        SERVE_ROUTES
            .iter()
            .all(|(m, _)| matches!(*m, "GET" | "POST")),
        "a route with another method needs a decision here"
    );
}

/// The whole table, once with placeholder ids and once with real ones (a session that waits for an
/// approval): a read-only token reads exactly what the full token reads, and every write is
/// refused with the same 403, before the route looks at its ids, body or state, so nothing
/// changed afterwards.
#[test]
fn a_read_only_token_gets_its_reads_and_a_403_on_every_write() {
    let serve = Serve::spawn(&spawn_mock(0));
    let ro = bearer(&device_token(&serve, "watch", Scope::ReadOnly));
    let full = bearer(&device_token(&serve, "phone", Scope::Full));
    let session = new_session(&serve);
    prompt(&serve, &session, "TOOL_MISSION write it");
    let (_, list) = serve.http("GET", "/approvals", None);
    let approval = list["approvals"][0]["id"].as_str().unwrap().to_string();
    let entries = || {
        serve
            .http("GET", &format!("/sessions/{session}/entries"), None)
            .1
    };
    let (before_entries, before_list) = (entries(), list);

    let mut writes = 0;
    for (id_session, id_approval) in [("nope", "nope"), (session.as_str(), approval.as_str())] {
        for (method, template) in SERVE_ROUTES {
            let path = fill(template, id_session, id_approval);
            let real = id_session == session;
            if is_hook(template) {
                // Not behind the token: a read-only token changes nothing there.
                let (with, without) = (
                    raw(serve.port, method, &path, Some(&ro), "{}"),
                    raw(serve.port, method, &path, None, "{}"),
                );
                assert_eq!(with, without, "{method} {template}");
            } else if is_read(method, template) {
                // The real session streams events until it ends: its events are read below.
                if real && template.ends_with("/events") {
                    continue;
                }
                let (a, b) = (
                    raw(serve.port, method, &path, Some(&ro), ""),
                    raw(serve.port, method, &path, Some(&full), ""),
                );
                assert_ne!(status_of(&a), 403, "{method} {path}: {a}");
                assert_eq!(status_of(&a), status_of(&b), "{method} {path}: {a} / {b}");
            } else {
                writes += 1;
                let a = raw(
                    serve.port,
                    method,
                    &path,
                    Some(&ro),
                    "{\"decision\":\"allow\"}",
                );
                assert_eq!(status_of(&a), 403, "{method} {path}: {a}");
                assert_eq!(json_of(&a), json!({"error": REFUSED}), "{method} {path}");
                assert!(!a.contains(&ro["Bearer ".len()..]), "the token is echoed");
            }
        }
    }
    assert!(
        writes >= 40,
        "the table had {writes} writes: is it read at all?"
    );

    // Fail closed: another method, an unlisted path, and a path that is not a route.
    for (m, p) in [
        ("DELETE", "/sessions/nope"),
        ("PUT", "/sessions"),
        ("PATCH", "/info"),
        ("OPTIONS", "/info"),
        ("HEAD", "/sessions"),
        ("GET", "/nope"),
        ("GET", "/sessions/nope/prompt"),
        ("GET", "/sessions/nope/fork"),
        ("GET", "/approvals/nope/decision"),
        ("GET", "/sessions/nope/images"),
        ("POST", "/info"),
        ("POST", "/nope"),
    ] {
        let a = raw(serve.port, m, p, Some(&ro), "{}");
        assert_eq!(status_of(&a), 403, "{m} {p}: {a}");
    }

    // Nothing changed: still one session, its transcript, and the approval still pending.
    assert_eq!(
        serve.http("GET", "/sessions", None).1["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(entries(), before_entries);
    let (_, after) = serve.http("GET", "/approvals", None);
    assert_eq!(after, before_list);
    assert_eq!(after["approvals"][0]["status"], "pending");
}

/// An approval can never be answered with a read-only token: not allow, not deny, not a delegated
/// child's, whatever the body says; the same decision from a full token works afterwards.
#[test]
fn a_read_only_token_never_answers_an_approval() {
    let serve = Serve::spawn(&spawn_mock(0));
    let ro = bearer(&device_token(&serve, "watch", Scope::ReadOnly));
    let full = bearer(&device_token(&serve, "phone", Scope::Full));
    let session = new_session(&serve);
    prompt(&serve, &session, "TOOL_MISSION write it");

    let listed = raw(serve.port, "GET", "/approvals", Some(&ro), "");
    assert_eq!(status_of(&listed), 200, "a read-only token lists approvals");
    let id = json_of(&listed)["approvals"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    for (target, body) in [
        (id.as_str(), json!({"decision": "allow"})),
        (id.as_str(), json!({"decision": "deny"})),
        (id.as_str(), json!({})),
        ("dap-nope", json!({"decision": "allow"})),
    ] {
        let a = raw(
            serve.port,
            "POST",
            &format!("/approvals/{target}/decision"),
            Some(&ro),
            &body.to_string(),
        );
        assert_eq!(status_of(&a), 403, "{target} {body}: {a}");
    }
    let (_, still) = serve.http("GET", "/approvals", None);
    assert_eq!(still["approvals"][0]["status"], "pending", "{still}");
    assert!(!serve.dir.join("approved.txt").exists());

    let denied = raw(
        serve.port,
        "POST",
        &format!("/approvals/{id}/decision"),
        Some(&full),
        &json!({"decision": "deny"}).to_string(),
    );
    assert_eq!(status_of(&denied), 200, "{denied}");
    assert_eq!(json_of(&denied)["status"], "denied");
}

/// `GET /info` says what the calling token may do; the shared token is `full`.
#[test]
fn info_reports_the_scope_of_the_caller() {
    let serve = Serve::spawn(&spawn_mock(0));
    let ro = bearer(&device_token(&serve, "watch", Scope::ReadOnly));
    let full = bearer(&device_token(&serve, "phone", Scope::Full));
    for (credential, scope) in [
        (Some("Bearer t0ken".to_string()), "full"),
        (Some(full), "full"),
        (Some(ro), "read-only"),
    ] {
        let a = raw(serve.port, "GET", "/info", credential.as_deref(), "");
        assert_eq!(status_of(&a), 200, "{a}");
        assert_eq!(json_of(&a)["scope"], scope);
        assert!(json_of(&a)["features"].is_array());
    }
}
