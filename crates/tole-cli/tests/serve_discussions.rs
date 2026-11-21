//! #1042 discussions over the REAL `tole serve` face, read through the typed
//! client: the list and show are the audit view of `tole discuss`, the
//! estimate is its dry run, and none of them can widen what the owner types.

#![cfg(feature = "shell-tools")]

use serde::Serialize;
use serde_json::{json, Value};
use tole_client::{Client, DiscussionRequest, Error};
use tole_core::discussion::Contract;
use tole_core::discussion_log::DiscussionLog;
use tole_core::secrets::SecretSet;

mod common;
mod serve_harness;
use serve_harness::{home_of, spawn_mock, workspace_dir, Serve};

/// A serve whose owner (the user config) defines two profiles, over a data
/// directory holding one open and one closed discussion.
fn serve() -> Serve {
    let dir = workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let data = home_of(&dir).join("tole");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.toml"),
        "[profiles.coder]\n[profiles.ops]\n",
    )
    .unwrap();
    let secrets = SecretSet::default();
    let names = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for id in ["open-one", "closed-one"] {
        let c = Contract::new(
            id,
            "Which database?",
            &names(&["a choice is named"]),
            &names(&["coder", "ops"]),
            100_000,
            None,
            &secrets,
        )
        .unwrap();
        DiscussionLog::at(&data.join("discussions"), id)
            .unwrap()
            .start(c, 1)
            .unwrap();
    }
    let log = DiscussionLog::at(&data.join("discussions"), "closed-one").unwrap();
    log.stop(2).unwrap();
    log.close("Stopped by the owner.", 3).unwrap();
    Serve::spawn_in(&spawn_mock(0), dir, true)
}

fn client(serve: &Serve) -> Client {
    let url = format!("http://127.0.0.1:{}", serve.port);
    Client::connect_tcp(&url, Some("t0ken".into()), false).unwrap()
}

fn lossless<T: Serialize>(what: &str, typed: &T, raw: &Value) {
    assert_eq!(&serde_json::to_value(typed).unwrap(), raw, "{what}");
}

fn request() -> DiscussionRequest {
    DiscussionRequest {
        question: "Which invoice tool?".into(),
        criteria: vec!["one tool is named".into()],
        participants: vec!["coder".into(), "ops".into()],
        budget_tokens: 100_000,
        max_rounds: None,
        files: Vec::new(),
        id: None,
    }
}

#[test]
fn the_list_and_the_show_are_the_audit_view() {
    let serve = serve();
    let c = client(&serve);

    let list = c.discussions().unwrap();
    let rows: Vec<(&str, &str)> = list
        .discussions
        .iter()
        .map(|r| (r.id.as_str(), r.status.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("closed-one", "closed (escalate: stopped)"),
            ("open-one", "open")
        ]
    );
    let (code, raw) = serve.http("GET", "/discussions", None);
    assert_eq!(code, 200);
    // The server's own directory is not part of the answer.
    assert!(raw.get("dir").is_none(), "{raw}");
    lossless("list", &list, &raw);

    let shown = c.discussion("closed-one").unwrap();
    assert_eq!(shown.question, "Which database?");
    assert_eq!((shown.budget, shown.max_rounds), (100_000, 4));
    let outcome = shown.outcome.clone().expect("a closed discussion has one");
    assert_eq!(outcome.gate, "escalate: stopped");
    assert_eq!(outcome.text, "Stopped by the owner.");
    let (_, raw) = serve.http("GET", "/discussions/closed-one", None);
    lossless("show", &shown, &raw);
    assert!(c.discussion("open-one").unwrap().outcome.is_none());

    // The audit's own refusals: an unknown id is a 404 that names no path, a
    // malformed one a 400, a missing token a 401.
    assert!(matches!(
        c.discussion("nope"),
        Err(Error::Status { status: 404, ref message, .. }) if message == "no such discussion"
    ));
    assert!(matches!(
        c.discussion("Not Valid"),
        Err(Error::Status { status: 400, .. })
    ));
    let (code, _) = serve.http_with("GET", "/discussions", false, None);
    assert_eq!(code, 401);
}

#[test]
fn the_estimate_is_the_dry_run_and_writes_nothing() {
    let serve = serve();
    let c = client(&serve);
    let data = home_of(&serve.dir).join("tole/discussions");
    let before = std::fs::read_dir(&data).unwrap().count();

    // A fixed id: the default one is the clock, and the plan text names it.
    let mut req = request();
    req.id = Some("estimate-one".into());
    let est = c.estimate_discussion(&req).unwrap();
    assert_eq!(est.participants, ["coder", "ops"]);
    assert_eq!((est.max_rounds, est.max_calls), (4, 16));
    assert!(est.round_floor_tokens > 0 && est.plan.contains("4 rounds"));
    let body = serde_json::to_string(&req).unwrap();
    let (code, raw) = serve.http("POST", "/discussions/estimate", Some(&body));
    assert_eq!(code, 200, "{raw}");
    lossless("estimate", &est, &raw);
    assert_eq!(std::fs::read_dir(&data).unwrap().count(), before);

    // A background file inside the workspace is named in the estimate.
    std::fs::write(serve.dir.join("notes.txt"), "context").unwrap();
    let mut with_file = request();
    with_file.files = vec!["notes.txt".into()];
    let est = c.estimate_discussion(&with_file).unwrap();
    assert_eq!(est.files, ["notes.txt"]);
    assert!(est.plan.contains("Background files"));
}

#[test]
fn the_estimate_applies_the_rules_of_tole_discuss_start() {
    let serve = serve();
    let c = client(&serve);
    let refused = |req: DiscussionRequest| match c.estimate_discussion(&req) {
        Err(Error::Status {
            status, message, ..
        }) => (status, message),
        other => panic!("expected a refusal, got {other:?}"),
    };
    // The contract's own rules: a budget, a criterion, a known profile, a
    // profile of the owner's config rather than a built-in role.
    let mut r = request();
    r.budget_tokens = 0;
    assert_eq!(refused(r).0, 400);
    let mut r = request();
    r.criteria.clear();
    assert_eq!(refused(r).0, 400);
    let mut r = request();
    r.participants = vec!["stranger".into()];
    assert!(refused(r).1.contains("stranger"));
    let mut r = request();
    r.participants = vec!["scribe".into()];
    assert!(refused(r).1.contains("built-in"));
    // A file outside the workspace is refused like `--file` outside the folder.
    let mut r = request();
    r.files = vec!["../outside.txt".into()];
    assert_eq!(refused(r).0, 400);
}

#[test]
fn a_request_cannot_carry_more_than_the_owner_types() {
    let serve = serve();
    for extra in ["tools", "allow", "profile", "modes", "secrets", "depth"] {
        let mut body = serde_json::to_value(request()).unwrap();
        body[extra] = json!(["x"]);
        let (code, v) = serve.http("POST", "/discussions/estimate", Some(&body.to_string()));
        assert_eq!(code, 400, "{extra}: {v}");
        assert!(v["error"].as_str().unwrap().contains(extra), "{v}");
    }
    for bad in ["not json", "[]", r#"{"question": 1}"#] {
        let (code, _) = serve.http("POST", "/discussions/estimate", Some(bad));
        assert_eq!(code, 400, "{bad}");
    }
}
