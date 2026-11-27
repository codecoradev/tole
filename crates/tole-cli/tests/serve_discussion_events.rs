//! #1094: `GET /discussions/{id}/events` over the REAL `tole serve`: a client
//! follows a discussion to its close without polling, a dropped connection
//! resumes without losing or repeating a step, and nothing a participant wrote
//! is on the stream.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::time::Duration;
use tole_client::{Client, Event, ReconnectPolicy, StreamItem};
use tole_core::discussion::{parse_digest, Contract, CriterionVerdict, RoundSummary};
use tole_core::discussion_log::{DiscussionLog, Reserve};
use tole_core::secrets::SecretSet;

mod common;
mod serve_harness;
use serve_harness::discussion_mock::discussion_mock;
use serve_harness::{home_of, workspace_dir, Frame, Serve, Sse};

const WITHIN: Duration = Duration::from_secs(60);
/// A secret-shaped variable of the serve process (`*_API_KEY`): its value is in
/// the process secret set, which is what the discussion routes redact with.
const SECRET: &str = "sk-planted-0123456789abcdef";
const RAW: &str = "RAW-TURN-MARKER";

fn names(l: &[&str]) -> Vec<String> {
    l.iter().map(|s| s.to_string()).collect()
}

/// A workspace whose owner config has two profiles, and the discussions
/// directory of its data dir.
fn workspace() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let data = home_of(&dir).join("tole");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.toml"),
        "[profiles.coder]\n[profiles.ops]\n",
    )
    .unwrap();
    (dir, data.join("discussions"))
}

fn contract(id: &str, question: &str) -> Contract {
    let (criteria, who) = (names(&["a choice is named"]), names(&["coder", "ops"]));
    let secrets = SecretSet::default();
    Contract::new(id, question, &criteria, &who, 100_000, None, &secrets).unwrap()
}

/// A discussion that ran to its report, in which both speakers' raw turns and
/// the digest and the closing text all carry the planted secret.
fn finished(ledger: &std::path::Path, id: &str) {
    let l = DiscussionLog::at(ledger, id).unwrap();
    l.start(contract(id, &format!("Which database? {SECRET}")), 1)
        .unwrap();
    let Reserve::Go(call) = l.reserve(1, "participant-coder", 500, 2).unwrap() else {
        panic!("the budget fits")
    };
    l.settle(&call, 120, 3).unwrap();
    let raw = format!("{RAW} key {SECRET}");
    let a = l.contribute(1, "coder", &raw, 4).unwrap();
    let b = l.contribute(1, "ops", &raw, 5).unwrap();
    let digest = json!({
        "claims": [
            {"text": format!("use SQLite, key {SECRET}"), "by": "coder", "entry": a.entry},
            {"text": "Postgres is better", "by": "ops", "entry": b.entry},
        ],
        "delta": [format!("{RAW} {SECRET}")],
    })
    .to_string();
    let none = SecretSet::default();
    let d = parse_digest(&digest, 1, &[a.clone(), b.clone()], &none).unwrap();
    l.summarize(RoundSummary::Digest(d), 6).unwrap();
    let met = CriterionVerdict {
        met: true,
        evidence: vec![a.entry],
    };
    l.check(1, vec![met], 7).unwrap();
    l.decide(1, 8).unwrap();
    l.close(&format!("picked SQLite {SECRET} {RAW}"), 9)
        .unwrap();
}

fn ledger_lines(ledger: &std::path::Path, id: &str) -> u64 {
    let text = std::fs::read_to_string(ledger.join(format!("{id}.jsonl"))).unwrap();
    text.lines().filter(|l| !l.trim().is_empty()).count() as u64
}

fn open(serve: &Serve, id: &str, after: Option<u64>) -> Sse {
    let query = after.map_or(String::new(), |n| format!("?after={n}"));
    Sse::open(serve, &format!("/discussions/{id}/events{query}"), "", true)
        .unwrap_or_else(|e| panic!("the stream was refused: {e:?}"))
}

fn changed(frames: &[Frame]) -> Vec<(u64, Value)> {
    let of = |f: &Frame| (f.event == "changed").then(|| (f.id.unwrap(), f.data.clone()));
    frames.iter().filter_map(of).collect()
}

fn client(serve: &Serve) -> Client {
    let url = format!("http://127.0.0.1:{}", serve.port);
    Client::connect_tcp(&url, Some("t0ken".into()), false).unwrap()
}

#[test]
fn a_planted_secret_in_a_contribution_never_reaches_the_stream() {
    let (dir, ledger) = workspace();
    finished(&ledger, "pick-db");
    // The precondition: the raw ledger really holds what must not come out.
    let raw = std::fs::read_to_string(ledger.join("pick-db.jsonl")).unwrap();
    assert!(raw.contains(SECRET) && raw.contains(RAW), "{raw}");
    let env = [("PLANTED_API_KEY", SECRET)];
    let serve = Serve::spawn_in_env(&discussion_mock(0), dir, true, &env);

    let sse = open(&serve, "pick-db", None);
    let frames = sse.until(WITHIN, |f| f.event == "closed");
    let wire = serde_json::to_string(&frames.iter().map(|f| &f.data).collect::<Vec<_>>()).unwrap();
    for leak in [SECRET, &SECRET[..12], RAW, "SQLite", "database"] {
        assert!(!wire.contains(leak), "{leak} reached the stream: {wire}");
    }
    // The stream is a signal of a closed set: these frames, these keys, and
    // no string in them but one word of `step` and the id.
    let events: Vec<&str> = frames.iter().map(|f| f.event.as_str()).collect();
    assert_eq!(events, ["changed", "ready", "closed"], "{frames:?}");
    let keys = |v: &Value| {
        let mut k: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    let want = ["closed", "round", "seq", "step", "tokens_used"];
    assert_eq!(keys(&frames[0].data), want);
    let step = frames[0].data["step"].as_str().unwrap();
    assert!([
        "contribute",
        "summarize",
        "check",
        "decide",
        "finish",
        "done"
    ]
    .contains(&step));
    // The audit view the client then reads is redacted the same way (the
    // control: the secret IS in the process set and is hidden there too).
    let (code, shown) = serve.http("GET", "/discussions/pick-db", None);
    assert_eq!(code, 200);
    assert!(!shown.to_string().contains(SECRET), "{shown}");
}

#[test]
fn a_replay_ends_closed_and_resume_neither_loses_nor_repeats() {
    let (dir, ledger) = workspace();
    finished(&ledger, "done-one");
    let last = ledger_lines(&ledger, "done-one");
    let serve = Serve::spawn_in(&discussion_mock(0), dir, true);

    // From the start: one signal for the whole ledger, where it ended.
    let all = open(&serve, "done-one", None).until(WITHIN, |f| f.event == "closed");
    let sigs = changed(&all);
    assert_eq!(sigs.len(), 1, "{all:?}");
    assert_eq!(
        sigs[0].0, last,
        "the id of a frame is the seq of the ledger"
    );
    assert_eq!(sigs[0].1["seq"], last);
    assert_eq!(sigs[0].1["closed"], true);
    assert_eq!(sigs[0].1["step"], "done");
    let ready = all.iter().find(|f| f.event == "ready").unwrap();
    assert_eq!(ready.data["lastSeq"], last);
    assert_eq!(ready.data["busy"], false);

    // Resuming after the last seq (by query, and by Last-Event-ID): nothing
    // repeats, and the stream still ends.
    for sse in [
        open(&serve, "done-one", Some(last)),
        Sse::open(
            &serve,
            "/discussions/done-one/events",
            &format!("Last-Event-ID: {last}\r\n"),
            true,
        )
        .unwrap(),
    ] {
        let again = sse.until(WITHIN, |f| f.event == "closed");
        assert!(changed(&again).is_empty(), "{again:?}");
    }
    // One step behind: the one signal that says where it is now.
    let behind = open(&serve, "done-one", Some(last - 1)).until(WITHIN, |f| f.event == "closed");
    assert_eq!(changed(&behind).len(), 1);
}

#[test]
fn a_running_discussion_is_followed_to_its_close_across_a_dropped_connection() {
    let (dir, ledger) = workspace();
    let mock = discussion_mock(600);
    let serve = Serve::spawn_in(&mock, dir, true);
    let c = client(&serve);
    let req = tole_client::DiscussionRequest {
        question: "hold this one".into(),
        criteria: names(&["one tool is named"]),
        participants: names(&["coder", "ops"]),
        budget_tokens: 100_000,
        max_rounds: None,
        files: Vec::new(),
        id: Some("live-one".into()),
    };
    c.start_discussion(&req).unwrap();

    // The first connection sees it run, then is dropped on purpose.
    let sse = open(&serve, "live-one", None);
    let first = sse.until(WITHIN, |f| f.event == "ready");
    let mut seqs: Vec<u64> = changed(&first).iter().map(|s| s.0).collect();
    assert!(first.iter().any(|f| f.event == "ready"), "{first:?}");
    sse.close();

    // The typed client resumes from there to the close: strictly increasing
    // seqs, none at or below the last one seen, the end is `closed`.
    let policy = ReconnectPolicy::default();
    let mut stream = c.follow_discussion("live-one", seqs[0], policy);
    let mut end = None;
    for item in stream.by_ref() {
        match item.unwrap() {
            StreamItem::Event(Event::Changed(s)) => {
                seqs.push(s.seq);
                end = Some(s);
            }
            StreamItem::Event(Event::Closed { id }) => assert_eq!(id, "live-one"),
            _ => {}
        }
    }
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    let end = end.expect("a change after the first");
    assert!(end.closed && end.step == "done", "{end:?}");
    assert_eq!(end.seq, ledger_lines(&ledger, "live-one"));
    assert_eq!(c.discussion("live-one").unwrap().state, "closed");
}

#[test]
fn a_resume_of_an_interrupted_one_is_announced_as_a_state() {
    let (dir, ledger) = workspace();
    // Nobody drives it: interrupted.
    let log = DiscussionLog::at(&ledger, "held-one").unwrap();
    log.start(contract("held-one", "hold this one"), 1).unwrap();
    let serve = Serve::spawn_in(&discussion_mock(1500), dir, true);

    let sse = open(&serve, "held-one", None);
    let first = sse.until(WITHIN, |f| f.event == "ready");
    let ready = first.iter().find(|f| f.event == "ready").unwrap();
    assert_eq!(ready.data["busy"], false, "{first:?}");
    let (code, body) = serve.http("POST", "/discussions/held-one/resume", None);
    assert_eq!(code, 202, "{body}");
    let rest = sse.until(WITHIN, |f| f.event == "state");
    assert_eq!(rest.last().unwrap().data, json!({"state": "running"}));
    // And it still ends in `closed`.
    let end = sse.until(WITHIN, |f| f.event == "closed");
    assert_eq!(end.last().unwrap().data, json!({"id": "held-one"}));
}

#[test]
fn a_bad_request_is_refused_before_a_stream_opens() {
    let (dir, ledger) = workspace();
    finished(&ledger, "done-one");
    let serve = Serve::spawn_in(&discussion_mock(0), dir, true);
    let refused = |path: &str, auth: bool| match Sse::open(&serve, path, "", auth) {
        Ok(_) => panic!("{path} opened a stream"),
        Err(e) => e,
    };
    let (code, body) = refused("/discussions/nope/events", true);
    assert_eq!((code, &body["error"]), (404, &json!("no such discussion")));
    for bad in ["Bad-Id", "-x", "a%2Fb"] {
        let (code, _) = refused(&format!("/discussions/{bad}/events"), true);
        assert_eq!(code, 400, "{bad}");
    }
    let (code, _) = refused("/discussions/done-one/events?after=-1", true);
    assert_eq!(code, 400);
    let (code, _) = refused("/discussions/done-one/events", false);
    assert_eq!(code, 401);
}
