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
use serve_harness::discussion_mock::discussion_mock;
use serve_harness::{home_of, spawn_mock, workspace_dir, Serve};

/// A serve whose owner (the user config) defines two profiles, over a data
/// directory holding one open and one closed discussion.
fn serve() -> Serve {
    serve_on(&spawn_mock(0))
}

fn serve_on(base: &str) -> Serve {
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
    Serve::spawn_in(base, dir, true)
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

fn wait_closed(c: &Client, id: &str) -> tole_client::Discussion {
    let mut last = None;
    common::wait_until(common::WAIT, "the discussion to close", || {
        last = c.discussion(id).ok().filter(|d| d.state == "closed");
        last.is_some()
    });
    last.unwrap()
}

fn ledger_files(serve: &Serve) -> usize {
    std::fs::read_dir(home_of(&serve.dir).join("tole/discussions"))
        .unwrap()
        .count()
}

#[test]
fn a_start_runs_to_its_end_and_is_read_back() {
    let serve = serve_on(&discussion_mock(0));
    let c = client(&serve);
    let started = c.start_discussion(&request()).unwrap();
    assert_eq!(started.status, "running");
    assert!(started.plan.contains("2 participants"), "{}", started.plan);

    let done = wait_closed(&c, &started.id);
    assert_eq!(done.outcome.as_ref().unwrap().gate, "report");
    assert_eq!(done.participants, ["coder", "ops"]);
    assert!(done.tokens_used > 0 && done.tokens_used <= done.budget);
    let row = c.discussions().unwrap();
    let row = row.discussions.iter().find(|r| r.id == started.id).unwrap();
    assert_eq!(row.status, "closed (report)");

    // An id that is taken is a conflict, and nothing is overwritten.
    let mut again = request();
    again.id = Some(started.id.clone());
    assert!(matches!(
        c.start_discussion(&again),
        Err(Error::Status { status: 409, .. })
    ));
}

#[test]
fn a_start_applies_the_budget_participant_and_depth_rules_of_the_cli() {
    let serve = serve_on(&discussion_mock(0));
    let c = client(&serve);
    // The budget is a hard cap: a call that does not fit stops the discussion
    // with an escalation before anything is spent.
    let mut tiny = request();
    tiny.budget_tokens = 1;
    let id = c.start_discussion(&tiny).unwrap().id;
    let done = wait_closed(&c, &id);
    assert_eq!(done.outcome.unwrap().gate, "escalate: budget");
    assert_eq!(done.tokens_used, 0);

    // What the estimate refuses a start refuses too, and leaves no ledger.
    let files = ledger_files(&serve);
    let mut unknown = request();
    unknown.participants = vec!["stranger".into()];
    let mut role = request();
    role.participants = vec!["scribe".into()];
    let mut outside = request();
    outside.files = vec!["../outside.txt".into()];
    for bad in [unknown, role, outside] {
        let r = c.start_discussion(&bad);
        assert!(matches!(r, Err(Error::Status { status: 400, .. })), "{r:?}");
    }
    // Depth 0, inline voices, no tools: a request cannot ask for more.
    for extra in ["tools", "modes", "depth", "allow", "profile"] {
        let mut body = serde_json::to_value(request()).unwrap();
        body[extra] = json!(["x"]);
        let (code, _) = serve.http("POST", "/discussions", Some(&body.to_string()));
        assert_eq!(code, 400, "{extra}");
    }
    assert_eq!(ledger_files(&serve), files);
}

#[test]
fn a_stop_closes_it_and_a_resume_continues_an_interrupted_one() {
    let serve = serve_on(&discussion_mock(0));
    let c = client(&serve);
    // `open-one` has a contract and nobody drives it: interrupted.
    assert_eq!(c.discussion("open-one").unwrap().state, "interrupted");
    let resumed = c.resume_discussion("open-one").unwrap();
    assert_eq!(resumed.status, "running");
    let done = wait_closed(&c, "open-one");
    assert_eq!(done.outcome.unwrap().gate, "report");

    // A closed one is not resumed, an unknown one is not found.
    for id in ["open-one", "closed-one"] {
        let r = c.resume_discussion(id);
        assert!(matches!(r, Err(Error::Status { status: 409, .. })), "{r:?}");
    }
    assert!(matches!(
        c.resume_discussion("nope"),
        Err(Error::Status { status: 404, .. })
    ));

    // A stop on one that is already closed changes nothing, however often.
    for _ in 0..2 {
        assert_eq!(c.stop_discussion("closed-one").unwrap().state, "closed");
    }
    assert!(matches!(
        c.stop_discussion("nope"),
        Err(Error::Status { status: 404, .. })
    ));
}

#[test]
fn any_number_of_discussions_run_at_once_each_to_its_own_end() {
    let serve = serve_on(&discussion_mock(800));
    let c = client(&serve);
    let ids: Vec<String> = (0..4).map(|i| format!("many-{i}")).collect();
    for id in &ids {
        let mut req = request();
        req.question = "hold this one".into();
        req.id = Some(id.clone());
        c.start_discussion(&req).unwrap();
    }
    // All four are running together: none was refused for the others.
    for id in &ids {
        assert_eq!(c.discussion(id).unwrap().state, "running", "{id}");
    }
    for id in &ids {
        let done = wait_closed(&c, id);
        assert_eq!(done.outcome.as_ref().unwrap().gate, "report", "{id}");
        assert!(
            done.tokens_used > 0 && done.tokens_used <= done.budget,
            "{id}"
        );
    }
}

#[test]
fn a_running_one_is_stopped_and_the_same_id_cannot_run_twice() {
    let serve = serve_on(&discussion_mock(30_000));
    let c = client(&serve);
    let mut held = request();
    held.question = "hold this one".into();
    held.id = Some("first-one".into());
    let first = c.start_discussion(&held).unwrap().id;
    assert_eq!(c.discussion(&first).unwrap().state, "running");
    // The same id, or a resume of the running one, is a conflict; another id
    // is not.
    let again = c.start_discussion(&held);
    assert!(
        matches!(again, Err(Error::Status { status: 409, .. })),
        "{again:?}"
    );
    let again = c.resume_discussion(&first);
    assert!(
        matches!(again, Err(Error::Status { status: 409, .. })),
        "{again:?}"
    );
    held.id = Some("second-one".into());
    c.start_discussion(&held).unwrap();
    // A stop is recorded at once; the call in flight finishes on its own.
    let stopped = c.stop_discussion(&first).unwrap();
    assert_eq!(stopped.state, "running");
    assert!(stopped.outcome.contains("stopped"), "{}", stopped.outcome);
}

#[test]
fn a_plan_mode_server_starts_and_resumes_nothing() {
    let serve = Serve::spawn_with_args(&discussion_mock(0), &["--plan-mode"]);
    let c = client(&serve);
    let r = c.start_discussion(&request());
    assert!(matches!(r, Err(Error::Status { status: 409, .. })), "{r:?}");
    let r = c.resume_discussion("open-one");
    assert!(matches!(r, Err(Error::Status { status: 409, .. })), "{r:?}");
}

/// The id of a route is checked the same way on every route (`id_ok`), before
/// the ledger is looked at: an id that could name another path, or that is not
/// a discussion id at all, is a 400 on show, resume and stop alike.
#[test]
fn an_id_that_is_not_an_id_is_refused_the_same_on_every_route() {
    let serve = serve();
    let long = "a".repeat(49);
    let bad = ["../x", "a/b", "A", "-x", "x.y", long.as_str()];
    for id in bad {
        // The router takes show's id as the rest of the path when it has no
        // slash, so a slash-bearing id only reaches resume and stop.
        let routes: Vec<(&str, String, bool)> = vec![
            ("GET", format!("/discussions/{id}"), !id.contains('/')),
            ("POST", format!("/discussions/{id}/resume"), true),
            ("POST", format!("/discussions/{id}/stop"), true),
        ];
        for (method, path, reaches_the_route) in routes {
            if !reaches_the_route {
                continue;
            }
            let (code, body) = serve.http(method, &path, None);
            assert_eq!(code, 400, "{method} {path}: {body}");
            assert_eq!(body["error"], "invalid discussion id", "{method} {path}");
        }
    }
    // Nothing was written outside the discussions directory by any of them.
    let escaped = serve.dir.join("x.jsonl");
    assert!(!escaped.exists(), "{}", escaped.display());
}

/// A path that is only a route's suffix after the collection (`/discussions/resume`,
/// `/discussions/stop`, `/missions/cancel`) starts with the route's prefix and
/// ends with its suffix at the same time, with no room for an id between them.
/// Slicing it as `{prefix}{id}{suffix}` made a reversed range and panicked the
/// connection's thread, so the client got no answer at all. It is an unknown
/// route, and the server keeps answering.
#[test]
fn a_path_that_is_only_a_route_suffix_is_not_found_and_does_not_panic() {
    let serve = serve();
    for path in [
        "/discussions/resume",
        "/discussions/stop",
        "/missions/cancel",
    ] {
        let (code, body) = serve.http("POST", path, None);
        assert_eq!(code, 404, "POST {path}: {body}");
    }
    // An empty id between the two is a bad id, not a missing route.
    for path in ["/discussions//resume", "/discussions//stop"] {
        let (code, body) = serve.http("POST", path, None);
        assert_eq!(code, 400, "POST {path}: {body}");
    }
    // The server is still up after all of them.
    let (code, _) = serve.http("GET", "/health", None);
    assert_eq!(code, 200);
}
