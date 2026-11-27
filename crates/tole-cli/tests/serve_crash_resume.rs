//! #1018, first slice: SIGKILL the REAL `tole serve` at random points in a turn
//! and check what the README promises ("kill the process mid-turn and resume
//! exactly where it stopped"): the session log still parses, the interrupted
//! turn is recoverable, and nothing is written twice.
//!
//! The kill delays are drawn from a seed that is printed, so a red run can be
//! replayed with the same delays: `TOLE_CRASH_LOOP_SEED=<seed> cargo test -p
//! tole-cli --test serve_crash_resume`. The seed fixes the delays, not the
//! timing of the machine. `TOLE_CRASH_LOOP_N` is the number of rounds (2 when
//! it is not set; the nightly job sets more).
//!
//! The rounds take turns between three kill windows, counted from the moment
//! the prompt is sent (the provider mock answers at 1.5 s): before or during
//! the commit of the user message (0 to 40 ms), inside the provider call (40
//! to 900 ms), and around the answer and its settle (1.4 to 1.7 s). The
//! third window only shows up from the third round on. The test says how many
//! rounds landed where, and fails when no round had the message durable, so a
//! prompt that is rejected cannot make every round pass for nothing.
//!
//! What is pinned, and where the answer is not "it just works":
//! - A turn killed while the provider is answering leaves its user message
//!   durable and no settle. `tole resume <id>` finishes that turn in the same
//!   session; over serve a resume settles it itself (#1230): the answer says
//!   `"interrupted": true`, the prompt that was cut off is not answered, and the
//!   next prompt is accepted. Two tests pin it, with a second resume and a
//!   resume during a running turn.
//! - A parked approval is in memory and is gone after a kill (documented in
//!   `docs/desktop-design.md`, "Crash or restart"); the session resumes, the
//!   write has not run, the model asks again, and one allow settles one write.
//!
//! The other kill points are in sibling files, on the same helpers
//! (`crash_loop/`): a kill inside a tool effect and around an approval
//! (`serve_crash_effect.rs`), `tole mission` (`mission_crash_resume.rs`) and
//! the webhook outbox (`serve_crash_outbox.rs`). The whole contract is in
//! `docs/crash-resume.md`.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(all(unix, feature = "shell-tools"))]

use serde_json::{json, Value};
use std::time::Duration;

mod common;
mod crash_loop;
mod serve_harness;
use crash_loop::{
    entry_seqs, last_entry, last_pc, mid_flight, rounds, seed, send_prompt, user_messages_with, Rng,
};
use serve_harness::{home_of, new_session, prompt, spawn_mock, Serve};

/// Milliseconds after the prompt is sent at which round `round` kills the
/// serve: the three windows of the module doc, in turn.
fn kill_delay(rng: &mut Rng, round: usize) -> u64 {
    match round % 3 {
        0 => rng.below(40),
        1 => 40 + rng.below(860),
        _ => 1400 + rng.below(300),
    }
}

/// The session file as `(items, torn tail)`; see [`crash_loop::read_log`].
fn read_log(serve: &Serve, id: &str, ctx: &str) -> (Vec<Value>, bool) {
    let log = crash_loop::read_log(&serve.session_file(id), ctx);
    (log.items, log.torn)
}

/// How many assistant answers the log holds (the mock answers every turn with
/// the same text), so a turn that is answered twice, or not at all, shows.
fn answers(log: &[Value]) -> usize {
    log.iter()
        .filter(|e| e["kind"] == "entry" && e["type"] == "message")
        .filter(|e| e["payload"]["role"] == "assistant")
        .filter(|e| e["payload"].to_string().contains("ENTRIES-ANSWER"))
        .count()
}

/// How many `write_file` calls settled with a result that says they worked.
fn settled_writes(log: &[Value]) -> usize {
    crash_loop::settles(log, "write_file")
        .iter()
        .filter(|e| e["type"] == "tool_result" && e["payload"]["ok"] == true)
        .count()
}

/// The `seq` of what `GET .../entries` returned.
fn api_seqs(page: &[Value]) -> Vec<u64> {
    page.iter().filter_map(|e| e["seq"].as_u64()).collect()
}

fn api_entries(serve: &Serve, id: &str) -> Vec<Value> {
    let (code, page) = serve.http("GET", &format!("/sessions/{id}/entries?limit=500"), None);
    assert_eq!(code, 200, "{page}");
    page["entries"].as_array().unwrap().clone()
}

/// `POST /sessions {"resume"}` on a session the server does not host: the answer.
fn resume_over_serve(serve: &Serve, id: &str, ctx: &str) -> Value {
    let (code, body) = serve.http(
        "POST",
        "/sessions",
        Some(&json!({"resume": id}).to_string()),
    );
    assert_eq!(code, 200, "{ctx}: {body}");
    assert_eq!(body["resumed"], true, "{ctx}: {body}");
    body
}

/// `tole resume <id>` on the serve's sessions directory: the documented
/// recovery of an interrupted turn. Returns the exit code and the answer.
fn cli_resume(base: &str, serve: &Serve, id: &str) -> (Option<i32>, String) {
    let ran = crash_loop::cli_resume(base, &serve.dir, &home_of(&serve.dir), id, &[]);
    (ran.code, ran.stdout)
}

/// Wait until the session file holds `needle` (the prompt reached the log).
fn wait_for_the_log_to_hold(serve: &Serve, id: &str, needle: &str) {
    let file = serve.session_file(id);
    common::wait_until(common::WAIT, "the user message to be durable", || {
        std::fs::read_to_string(&file).is_ok_and(|text| text.contains(needle))
    });
}

/// A serve killed while its provider holds a prompt: `(serve, id)`; the
/// serve's drop removes the workspace.
fn killed_mid_turn(base: &str, marker: &str) -> (Serve, String) {
    let mut serve = Serve::spawn(base);
    let id = new_session(&serve);
    let stream = send_prompt(serve.port, &id, marker);
    // The provider holds the prompt for 1.5 s; the kill follows the commit of
    // the user message at once, inside that time.
    wait_for_the_log_to_hold(&serve, &id, marker);
    serve.kill();
    drop(stream);
    (serve, id)
}

/// Kill a serve at a random point of a turn, again and again: the log always
/// parses, the turn is never written twice or lost, `tole resume` finishes it
/// when it was cut off, and the session then takes a new prompt over serve.
#[test]
fn a_serve_killed_at_a_random_point_in_a_turn_leaves_a_session_that_resumes() {
    let seed = seed();
    let rounds = rounds(2);
    eprintln!("crash loop: seed={seed} rounds={rounds} (TOLE_CRASH_LOOP_SEED / TOLE_CRASH_LOOP_N)");
    let mut rng = Rng(seed);
    let base = spawn_mock(1500);
    // Where the rounds landed: no message yet, a cut-off turn that was
    // finished by `tole resume`, a turn that had settled already, a torn tail.
    let (mut unsent, mut recovered, mut settled, mut torn_tails) = (0, 0, 0, 0);
    for round in 0..rounds {
        let delay = kill_delay(&mut rng, round);
        let ctx = format!("round {round}, kill after {delay} ms, seed {seed}");
        let marker = format!("slow marker-{seed}-{round}");

        let mut serve = Serve::spawn(&base);
        let id = new_session(&serve);
        let stream = send_prompt(serve.port, &id, &marker);
        std::thread::sleep(Duration::from_millis(delay));
        serve.kill();
        drop(stream);

        // 1. What the kill left: a log that parses, and at most one user
        // message (it is durable or it is not; it is never doubled).
        let (log, torn) = read_log(&serve, &id, &ctx);
        torn_tails += usize::from(torn);
        let sent = user_messages_with(&log, &marker);
        assert!(
            sent <= 1,
            "{ctx}: the user message is in the log {sent} times"
        );
        let pc = last_pc(&log);

        // 2. A turn that was cut off is finished by `tole resume`, in the
        // same session, with the same single user message and one answer.
        if sent == 1 && mid_flight(pc.as_deref()) {
            recovered += 1;
            let (code, answer) = cli_resume(&base, &serve, &id);
            assert_eq!(code, Some(0), "{ctx}: tole resume said {answer}");
            assert!(answer.contains("ENTRIES-ANSWER"), "{ctx}: {answer}");
            let (after, _) = read_log(&serve, &id, &ctx);
            assert_eq!(user_messages_with(&after, &marker), 1, "{ctx}");
            assert_eq!(answers(&after), 1, "{ctx}: the turn is answered once");
            assert_eq!(
                last_pc(&after).as_deref(),
                Some("final"),
                "{ctx}: the register after tole resume"
            );
            let last = last_entry(&after).unwrap();
            assert!(
                last["payload"]["role"] == "assistant",
                "{ctx}: the resumed turn did not end with the answer: {last}"
            );
        } else if sent == 1 {
            settled += 1;
            assert_eq!(answers(&log), 1, "{ctx}: a settled turn holds its answer");
        } else {
            unsent += 1;
            assert_eq!(answers(&log), 0, "{ctx}: no message, no answer");
        }
        let (now, _) = read_log(&serve, &id, &ctx);

        // 3. A new serve opens the session, shows exactly what the file holds,
        // and takes a new prompt: one more user message, one more answer.
        let second = Serve::spawn_in(&base, serve.dir.clone(), false);
        let opened = resume_over_serve(&second, &id, &ctx);
        assert!(opened.get("interrupted").is_none(), "{ctx}: {opened}");
        assert_eq!(
            api_seqs(&api_entries(&second, &id)),
            entry_seqs(&now),
            "{ctx}"
        );
        let next = format!("after round {round}");
        prompt(&second, &id, &next);
        let (end, _) = read_log(&second, &id, &ctx);
        assert_eq!(user_messages_with(&end, &marker), sent, "{ctx}");
        assert_eq!(user_messages_with(&end, &next), 1, "{ctx}");
        assert_eq!(answers(&end), sent + 1, "{ctx}: one answer per turn");
        assert!(end.len() > now.len(), "{ctx}: the new turn left nothing");
    }
    eprintln!(
        "crash loop: {unsent} without the message, {recovered} cut off and recovered by \
         `tole resume`, {settled} already settled, {torn_tails} with a torn tail"
    );
    // A prompt that never reaches the log (a bad route, a changed token) would
    // make every round pass for nothing.
    assert!(
        recovered + settled >= 1,
        "no round had the user message in the log: the prompt never arrived (seed {seed})"
    );
}

/// #1230: a resume settles the turn a kill cut off (no model call, no tool
/// run, append only) and says so; the interrupted prompt is not answered; a
/// second resume is a no-op; the next prompt is accepted and answered.
#[test]
fn a_resume_settles_a_killed_turn_and_the_next_prompt_is_accepted() {
    let base = spawn_mock(1500);
    let (first, id) = killed_mid_turn(&base, "slow marker-pinned");
    let (log, _) = read_log(&first, &id, "killed");
    assert_eq!(user_messages_with(&log, "marker-pinned"), 1);
    assert_eq!(last_pc(&log).as_deref(), Some("planning"), "no settle yet");
    let was = log.len();

    let second = Serve::spawn_in(&base, first.dir.clone(), false);
    let opened = resume_over_serve(&second, &id, "killed");
    assert_eq!(opened["interrupted"], true, "{opened}");
    let (log, _) = read_log(&second, &id, "settled");
    assert_eq!(
        last_pc(&log).as_deref(),
        Some("final"),
        "the turn is settled"
    );
    assert_eq!(answers(&log), 0, "the interrupted prompt is not answered");
    assert_eq!(user_messages_with(&log, "marker-pinned"), 1);
    assert!(log.len() > was, "the settle is appended");
    let seqs = api_seqs(&api_entries(&second, &id));

    // A second resume finds the session live and settled: nothing to do.
    let (code, again) = second.http(
        "POST",
        "/sessions",
        Some(&json!({"resume": id}).to_string()),
    );
    assert_eq!(
        (code, &again["alreadyLive"]),
        (200, &json!(true)),
        "{again}"
    );
    assert!(again.get("interrupted").is_none(), "{again}");
    assert_eq!(
        api_seqs(&api_entries(&second, &id)),
        seqs,
        "nothing written"
    );

    let (code, body) = second.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "next question"}).to_string()),
    );
    assert_eq!(
        (code, &body["stopReason"]),
        (200, &json!("end_turn")),
        "{body}"
    );
    let (log, _) = read_log(&second, &id, "next");
    assert_eq!(user_messages_with(&log, "next question"), 1);
    assert_eq!(answers(&log), 1, "only the new prompt is answered");
}

/// A resume while a turn is running answers as before (`busy`) and settles
/// nothing: the turn finishes and is answered once.
#[test]
fn a_resume_during_a_running_turn_settles_nothing() {
    let base = spawn_mock(1500);
    let serve = Serve::spawn(&base);
    let id = new_session(&serve);
    let _stream = send_prompt(serve.port, &id, "slow marker-running");
    wait_for_the_log_to_hold(&serve, &id, "marker-running");
    let (code, body) = serve.http(
        "POST",
        "/sessions",
        Some(&json!({"resume": id}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        (&body["alreadyLive"], &body["busy"]),
        (&json!(true), &json!(true))
    );
    assert!(body.get("interrupted").is_none(), "{body}");
    common::wait_until(common::WAIT, "the turn to be answered", || {
        answers(&read_log(&serve, &id, "running").0) == 1
    });
    let (log, _) = read_log(&serve, &id, "running");
    assert_eq!(last_pc(&log).as_deref(), Some("final"));
    assert!(!log.iter().any(|e| e["payload"]["error"] == "interrupted"));
}

/// A kill while a turn is parked at an approval: the queue row is gone (it is
/// in memory), the session resumes, the write has not run and is not run by
/// the resume, the model asks again with a new row, and one allow settles
/// exactly one write.
#[test]
fn a_kill_at_a_parked_approval_loses_the_row_but_never_settles_the_write_twice() {
    let base = spawn_mock(0);
    let mut serve = Serve::spawn(&base);
    let id = new_session(&serve);
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "TOOL_MISSION write it"}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["stopReason"], "approval_required", "{body}");
    let first_row = body["approval"]["id"].as_str().unwrap().to_string();
    let effect = serve.dir.join("approved.txt");
    assert!(!effect.exists(), "nothing ran before the approval");
    serve.kill();

    // What the kill left: a log that parses, a write that was asked for and
    // never settled, and no file.
    let (log, _) = read_log(&serve, &id, "parked");
    assert_eq!(settled_writes(&log), 0, "the write did not settle");
    assert!(!effect.exists(), "the kill did not run the effect");

    let second = Serve::spawn_in(&base, serve.dir.clone(), false);
    let listed = |s: &Serve| -> Vec<Value> {
        let (code, list) = s.http("GET", "/approvals", None);
        assert_eq!(code, 200, "{list}");
        list["approvals"].as_array().unwrap().clone()
    };
    assert!(
        listed(&second).is_empty(),
        "the queue is in memory: it is empty after a kill"
    );
    let opened = resume_over_serve(&second, &id, "approval");
    assert!(
        opened.get("interrupted").is_none(),
        "parked is not cut off: {opened}"
    );
    assert!(
        listed(&second).is_empty(),
        "a resume does not bring the row back"
    );
    assert!(!effect.exists(), "the resume did not run the effect");
    let (log, _) = read_log(&second, &id, "resumed");
    assert_eq!(settled_writes(&log), 0, "the resume settled no write");

    // The model asks again: one new row, not the old one, and one only.
    let (code, body) = second.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": "TOOL_MISSION go on"}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["stopReason"], "approval_required", "{body}");
    let row = body["approval"]["id"].as_str().unwrap().to_string();
    assert_ne!(row, first_row, "the old row is not resurrected");
    assert_eq!(listed(&second).len(), 1, "one pending approval, not two");

    // An allow settles exactly one write, and the turn ends settled.
    let (code, decision) = second.http(
        "POST",
        &format!("/approvals/{row}/decision"),
        Some(&json!({"decision": "allow"}).to_string()),
    );
    // 202: the allow resumes the parked turn in the background.
    assert_eq!(code, 202, "{decision}");
    assert_eq!(decision["resuming"], true, "{decision}");
    common::wait_until(common::WAIT, "the allowed turn to settle", || {
        let (log, _) = read_log(&second, &id, "allowed");
        last_pc(&log).as_deref() == Some("final")
    });
    let (log, _) = read_log(&second, &id, "allowed");
    assert_eq!(settled_writes(&log), 1, "one allow, one settled write");
    assert_eq!(std::fs::read_to_string(&effect).unwrap(), "remote ok");
}
