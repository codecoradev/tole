//! #524 event stream — end to end over the REAL serve face.
//! `GET /sessions/{id}/events` replays the durable log from a `seq`, follows
//! new entries, carries best-effort ephemeral frames (streamed text, tool
//! events, approvals), resumes losslessly after a dropped connection, and is
//! capped so streams cannot starve the other routes.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::time::Duration;

mod common;
mod serve_harness;
use serve_harness::{new_session, spawn_mock, Frame, Serve, Sse};

const WAIT: Duration = Duration::from_secs(15);

fn events_path(id: &str) -> String {
    format!("/sessions/{id}/events")
}

/// The log as `tole resume` replays it, as wire JSON.
fn log(serve: &Serve, id: &str) -> Vec<Value> {
    tole_core::storage::Storage::entries(
        &tole_core::storage::JsonlStorage::open_read_only(serve.session_file(id)).unwrap(),
    )
    .iter()
    .map(|e| serde_json::to_value(e).unwrap())
    .collect()
}

fn entry_frames(frames: &[Frame]) -> Vec<&Frame> {
    frames.iter().filter(|f| f.event == "entry").collect()
}

fn is_final_answer(f: &Frame) -> bool {
    f.event == "entry" && f.data["payload"]["text"] == "ENTRIES-ANSWER"
}

#[test]
fn a_stream_replays_the_log_then_follows_a_live_turn() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let (code, _) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "first"}).to_string()),
    );
    assert_eq!(code, 200);
    let before = log(&serve, &id);

    let sse = Sse::open(&serve, &events_path(&id), "", true).unwrap();
    // Replay: every durable entry, in order, each tagged with its seq, then
    // `ready` marks the end of the replay.
    let frames = sse.until(WAIT, |f| f.event == "ready");
    let replay = entry_frames(&frames);
    assert_eq!(replay.len(), before.len());
    for (f, e) in replay.iter().zip(&before) {
        assert_eq!(f.id, e["seq"].as_u64(), "the SSE id is the entry seq");
        assert_eq!(f.data, *e, "the frame carries the log entry as stored");
    }
    let ready = frames.last().unwrap();
    assert_eq!(ready.id, None);
    assert_eq!(ready.data["lastSeq"], before.last().unwrap()["seq"]);
    assert_eq!(ready.data["busy"], false);
    assert_eq!(ready.data["id"], id);
    let last_replayed = before.last().unwrap()["seq"].as_u64().unwrap();

    // A turn that starts while the stream is open: streamed text arrives as
    // an ephemeral frame, the new entries follow in order, and the prompt
    // answer itself is unchanged.
    let (reply_code, reply) = std::thread::scope(|scope| {
        let turn = scope.spawn(|| {
            serve.http(
                "POST",
                &format!("/sessions/{id}/prompt"),
                Some(&json!({"text": "second"}).to_string()),
            )
        });
        let frames = sse.until(WAIT, is_final_answer);
        let delta = frames
            .iter()
            .find(|f| f.event == "text_delta")
            .expect("a streamed text_delta frame");
        assert_eq!(delta.id, None, "ephemeral frames carry no id");
        assert_eq!(delta.data["text"], "ENTRIES-ANSWER");
        let seqs: Vec<u64> = entry_frames(&frames).iter().filter_map(|f| f.id).collect();
        assert!(seqs.len() >= 2, "user message and answer: {seqs:?}");
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "in order: {seqs:?}");
        assert!(seqs[0] > last_replayed, "no entry delivered twice");
        assert!(
            frames
                .iter()
                .any(|f| f.event == "entry" && f.data["payload"]["text"] == "second"),
            "the new user message is a durable frame"
        );
        turn.join().unwrap()
    });
    assert_eq!(reply_code, 200, "{reply}");
    assert_eq!(reply["text"], "ENTRIES-ANSWER");
}

#[test]
fn a_reconnect_after_a_seq_is_lossless_and_exact() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    serve_harness::prompt(&serve, &id, "one");

    let first = Sse::open(&serve, &events_path(&id), "", true).unwrap();
    let frames = first.until(WAIT, |f| f.event == "ready");
    let seen = entry_frames(&frames).last().unwrap().id.unwrap();
    first.close();

    // Turns while nobody is connected: nothing is lost, because the log is
    // the source (and with no stream open the turn runs as it always did).
    serve_harness::prompt(&serve, &id, "two while away");
    let all = log(&serve, &id);
    let missed: Vec<u64> = all
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .filter(|s| *s > seen)
        .collect();
    assert!(missed.len() >= 2);

    // The standard way: Last-Event-ID, as EventSource sends it.
    let by_header = Sse::open(
        &serve,
        &events_path(&id),
        &format!("Last-Event-ID: {seen}\r\n"),
        true,
    )
    .unwrap();
    let got = by_header.until(WAIT, |f| f.event == "ready");
    let ids: Vec<u64> = entry_frames(&got).iter().filter_map(|f| f.id).collect();
    assert_eq!(ids, missed, "exactly the entries after the last seen one");
    assert!(
        by_header.next(Duration::from_millis(500)).is_none(),
        "no stray frames after ready"
    );

    // An explicit `after` wins over a stale header.
    let by_query = Sse::open(
        &serve,
        &format!("{}?after={seen}", events_path(&id)),
        "Last-Event-ID: 0\r\n",
        true,
    )
    .unwrap();
    let got = by_query.until(WAIT, |f| f.event == "ready");
    let ids: Vec<u64> = entry_frames(&got).iter().filter_map(|f| f.id).collect();
    assert_eq!(ids, missed);

    // Past the end of the log: just `ready`, nothing replayed.
    let end = all.last().unwrap()["seq"].as_u64().unwrap();
    let none = Sse::open(
        &serve,
        &format!("{}?after={end}", events_path(&id)),
        "",
        true,
    )
    .unwrap();
    let got = none.until(WAIT, |f| f.event == "ready");
    assert!(entry_frames(&got).is_empty());
}

#[test]
fn streams_need_the_token_refuse_bad_input_and_are_capped() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let refused = |path: &str, auth: bool| Sse::open(&serve, path, "", auth).err().map(|e| e.0);
    assert_eq!(refused(&events_path(&id), false), Some(401));
    assert_eq!(refused("/sessions/srv-nope/events", true), Some(404));
    assert_eq!(
        refused(&format!("{}?after=x", events_path(&id)), true),
        Some(400)
    );
    assert_eq!(
        refused(&format!("{}?after=-1", events_path(&id)), true),
        Some(400)
    );

    // The stream cap: 8 open, the 9th is refused (429) rather than queued.
    let open: Vec<Sse> = (0..8)
        .map(|_| Sse::open(&serve, &events_path(&id), "", true).expect("within the cap"))
        .collect();
    for s in &open {
        s.until(WAIT, |f| f.event == "ready");
    }
    assert_eq!(refused(&events_path(&id), true), Some(429));
    // The other routes keep working with every stream slot taken.
    assert_eq!(serve.http("GET", "/sessions", None).0, 200);
    assert_eq!(serve.http("GET", "/approvals", None).0, 200);
    serve_harness::prompt(&serve, &id, "still serving");
}

#[test]
fn approval_frames_reach_the_stream() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let sse = Sse::open(&serve, &events_path(&id), "", true).unwrap();
    sse.until(WAIT, |f| f.event == "ready");

    // A Write that is not pre-authorized queues for a remote decision.
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "TOOL_MISSION write it"}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["stopReason"], "refusal", "fail-closed settle");

    let frames = sse.until(WAIT, |f| f.event == "approval_requested");
    let requested = frames.last().unwrap();
    assert_eq!(requested.data["tool"], "write_file");
    assert_eq!(requested.data["status"], "pending");
    assert_eq!(requested.data["sessionId"], id);
    let approval = requested.data["id"].as_str().unwrap().to_string();

    // Allow: resolved is announced, the replayed write runs, and the resumed
    // turn's durable entries (its final answer) arrive from the log even
    // though that turn has no live observer.
    let (code, _) = serve.http(
        "POST",
        &format!("/approvals/{approval}/decision"),
        Some(&json!({"decision": "allow"}).to_string()),
    );
    assert_eq!(code, 202);
    let frames = sse.until(WAIT, is_final_answer);
    let resolved = frames
        .iter()
        .find(|f| f.event == "approval_resolved")
        .expect("an approval_resolved frame");
    assert_eq!(resolved.data["id"], approval);
    assert_eq!(resolved.data["status"], "approved");
    // The decision is announced before the effect it caused: the resolved
    // frame precedes the final answer.
    let pos = |pred: &dyn Fn(&Frame) -> bool| frames.iter().position(pred).unwrap();
    assert!(
        pos(&|f| f.event == "approval_resolved") < pos(&is_final_answer),
        "resolved comes before the resumed turn's answer"
    );
    assert!(serve.dir.join("approved.txt").exists(), "the work executed");
}

/// An idle stream must heartbeat: that is how a dead peer is found (the
/// write fails) and its slot freed. The interval is shortened to 1 s here.
#[test]
fn an_idle_stream_sends_heartbeats() {
    let serve = Serve::spawn_with_env(&spawn_mock(0), &[("TOLE_SERVE_HEARTBEAT_SECS", "1")]);
    let id = new_session(&serve);
    let sse = Sse::open(&serve, &events_path(&id), "", true).unwrap();
    sse.until(WAIT, |f| f.event == "ready");
    // Nothing happens on the session: only heartbeats may arrive, and more
    // than one (the clock restarts after each real write).
    let first = sse.until(Duration::from_secs(5), |f| f.event == "ping");
    assert!(first
        .iter()
        .all(|f| f.event == "ping" || f.event == "approval_requested"));
    sse.until(Duration::from_secs(5), |f| f.event == "ping");
}
