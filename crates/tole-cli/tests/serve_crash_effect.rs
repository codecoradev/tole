//! #1018, second slice: SIGKILL the REAL `tole serve` inside a tool effect and
//! around an approval, again and again, and check the promises of the README
//! ("kill the process mid-turn and resume exactly where it stopped") that the
//! first slice (`serve_crash_resume.rs`) does not reach.
//!
//! The effect is a `run_command` that appends a line to `effect.log` and then
//! sleeps one second (see `crash_loop::mock`), so the test sees the effect
//! start and kills at a random point of it: `effect.log` counts how often the
//! effect ran. `TOLE_CRASH_LOOP_N` is the number of rounds, `TOLE_CRASH_LOOP_SEED`
//! replays the delays (see `crash_loop`).
//!
//! What is pinned, and where the answer is not "it just works":
//! - A kill inside the effect leaves the intent durable (`replay: guarded`),
//!   the pending cell set and no settlement. The log parses; the register
//!   still says `executing`.
//! - **The effect IS replayed when the operator approves the replay**:
//!   `tole resume --allow run_command` runs the command a second time under
//!   the same intent id and settles it once. A command has no idempotency key,
//!   so this is the documented contract of a `guarded` intent
//!   (`docs/crash-resume.md`), not a bug; what keeps it safe is that it goes
//!   through the approver again.
//! - Without that approval (`tole resume`, nothing allowed) the replay is
//!   refused: the intent settles as an error that says the outcome is UNKNOWN,
//!   the effect does not run again, and the session takes the next prompt.
//! - An effect that settled before the kill is never replayed, and `tole
//!   resume` says there is nothing to resume.
//! - An approval parked in serve's queue is in memory and is gone after a
//!   kill (`docs/desktop-design.md`, "Crash or restart"): the restarted serve
//!   lists none, the effect has not run, and when the model asks again there
//!   is exactly one new row and one allow runs the effect once.
//! - A kill after the allow, inside the effect it started, is the first case
//!   again: the grant was consumed by the killed process, so a restart does not
//!   run the effect on its own.
//!
//! Not covered by a kill test (and why): the deferred settlement fsync of #483
//! (`turn/mod.rs`). SIGKILL ends the process, not the kernel: every line the
//! process `write`s is in the page cache and survives it, fsync or not. The
//! window #483 opens (a settle written but not synced) shows only on a power
//! loss or a kernel crash, which a process-level test cannot cause.

#![cfg(all(unix, feature = "shell-tools"))]

use serde_json::{json, Value};
use std::time::Duration;

mod common;
mod crash_loop;
mod serve_harness;
use crash_loop::mock::{self, ANSWER};
use crash_loop::{
    answers_with, cli_resume, effect_runs, intents, last_pc, read_log, rounds, seed, send_prompt,
    settles, Rng,
};
use serve_harness::{home_of, new_session, prompt, Serve};

/// A serve that may run `run_command` without asking (`--allow`).
fn serve_allowing(base: &str) -> Serve {
    Serve::spawn_with_args(base, &["--allow", "run_command"])
}

fn effect_log(serve: &Serve) -> std::path::PathBuf {
    serve.dir.join("effect.log")
}

fn restart(base: &str, dead: &Serve, id: &str, ctx: &str) -> Serve {
    let second = Serve::spawn_in(base, dead.dir.clone(), false);
    let (code, body) = second.http(
        "POST",
        "/sessions",
        Some(&json!({"resume": id}).to_string()),
    );
    assert_eq!(
        (code, &body["resumed"]),
        (200, &json!(true)),
        "{ctx}: {body}"
    );
    second
}

fn approvals(serve: &Serve) -> Vec<Value> {
    let (code, list) = serve.http("GET", "/approvals", None);
    assert_eq!(code, 200, "{list}");
    list["approvals"].as_array().unwrap().clone()
}

/// What a kill left, and what recovery may and may not do with it. `allow`
/// pre-approves the replay (`tole resume --allow run_command`).
fn recover_and_check(base: &str, dead: &Serve, id: &str, allow: bool, ctx: &str) -> bool {
    let file = dead.session_file(id);
    let effect = effect_log(dead);
    let before = read_log(&file, ctx).items;
    assert_eq!(
        intents(&before, "run_command").len(),
        1,
        "{ctx}: one intent"
    );
    assert!(settles(&before, "run_command").len() <= 1, "{ctx}");
    let pending = settles(&before, "run_command").is_empty();
    let runs = effect_runs(&effect);
    assert_eq!(runs, 1, "{ctx}: the kill came after the effect began, once");
    let home = home_of(&dead.dir);
    let extra: &[&str] = if allow {
        &["--allow", "run_command"]
    } else {
        &[]
    };
    let ran = cli_resume(base, &dead.dir, &home, id, extra);
    let after = read_log(&file, ctx).items;
    let results = settles(&after, "run_command");
    assert_eq!(results.len(), 1, "{ctx}: the intent settles exactly once");
    if !pending {
        // Settled before the kill: never replayed, nothing to resume.
        assert_eq!(
            effect_runs(&effect),
            1,
            "{ctx}: a settled effect is not replayed"
        );
        assert!(
            ran.stderr.contains("nothing to resume") || ran.code == Some(0),
            "{ctx}: {}",
            ran.stderr
        );
    } else if allow {
        // PINNED: the effect IS replayed, under the same intent, once.
        assert_eq!(ran.code, Some(0), "{ctx}: {}", ran.stderr);
        assert_eq!(
            effect_runs(&effect),
            2,
            "{ctx}: an approved replay runs it again"
        );
        assert_eq!(results[0]["payload"]["ok"], true, "{ctx}");
        assert_eq!(last_pc(&after).as_deref(), Some("final"), "{ctx}");
        assert_eq!(answers_with(&after, ANSWER), 1, "{ctx}");
    } else {
        // Refused: an error that says the outcome is unknown, no second run.
        assert_eq!(ran.code, Some(2), "{ctx}: {}", ran.stderr);
        assert_eq!(effect_runs(&effect), 1, "{ctx}: no approval, no replay");
        let error = results[0]["payload"]["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("replay denied") && error.contains("UNKNOWN"),
            "{ctx}: {error}"
        );
        // The session is parked, not broken: a restarted serve takes a prompt.
        let second = restart(base, dead, id, ctx);
        prompt(&second, id, "after the refused replay");
        let end = read_log(&file, ctx).items;
        assert_eq!(answers_with(&end, ANSWER), 1, "{ctx}");
        assert_eq!(
            effect_runs(&effect),
            1,
            "{ctx}: the new turn did not run it"
        );
    }
    pending
}

#[test]
fn a_kill_inside_a_tool_effect_is_replayed_only_through_a_fresh_approval() {
    let (seed, rounds) = (seed(), rounds(4));
    eprintln!(
        "effect loop: seed={seed} rounds={rounds} (TOLE_CRASH_LOOP_SEED / TOLE_CRASH_LOOP_N)"
    );
    let mut rng = Rng(seed);
    let base = mock::spawn(0);
    let (mut inside, mut settled, mut starved) = (0, 0, false);
    for round in 0..rounds {
        // The first two rounds are fixed so every run covers both windows:
        // inside the effect (a kill 0 to 600 ms into its second), then after
        // it has settled (the test waits for the settlement). The rest are
        // anywhere from its start to just after its end.
        let delay = match round {
            0 => rng.below(600),
            1 => rng.below(150),
            _ => rng.below(1400),
        };
        let allow = round % 2 == 1;
        let ctx =
            format!("round {round}, kill {delay} ms into the effect, allow {allow}, seed {seed}");
        let mut serve = serve_allowing(&base);
        let id = new_session(&serve);
        let stream = send_prompt(serve.port, &id, "RUN-EFFECT go");
        let effect = effect_log(&serve);
        common::wait_until(common::WAIT, "the effect to start", || {
            effect_runs(&effect) >= 1
        });
        let started = std::time::Instant::now();
        if round == 1 {
            let file = serve.session_file(&id);
            common::wait_until(common::WAIT, "the effect to settle", || {
                std::fs::read_to_string(&file).is_ok_and(|t| t.contains("\"tool_result\""))
            });
        }
        std::thread::sleep(Duration::from_millis(delay));
        serve.kill();
        drop(stream);
        // A starved machine can stretch the first round past the effect.
        starved |= round == 0 && started.elapsed() > Duration::from_millis(900);
        if recover_and_check(&base, &serve, &id, allow, &ctx) {
            inside += 1;
        } else {
            settled += 1;
        }
    }
    eprintln!("effect loop: {inside} killed inside the effect, {settled} after it settled");
    if rounds >= 2 && !starved {
        assert!(inside >= 1 && settled >= 1, "both windows are covered");
    }
}

/// Park a `run_command` at serve's approval queue: `(row id)`.
fn park(serve: &Serve, id: &str, text: &str, ctx: &str) -> String {
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{id}/prompt"),
        Some(&json!({"text": text}).to_string()),
    );
    assert_eq!(code, 200, "{ctx}: {body}");
    assert_eq!(body["stopReason"], "approval_required", "{ctx}: {body}");
    body["approval"]["id"].as_str().unwrap().to_string()
}

fn allow(serve: &Serve, row: &str, ctx: &str) {
    let (code, decision) = serve.http(
        "POST",
        &format!("/approvals/{row}/decision"),
        Some(&json!({"decision": "allow"}).to_string()),
    );
    // 202: the allow resumes the parked turn in the background.
    assert_eq!(code, 202, "{ctx}: {decision}");
}

#[test]
fn a_kill_around_an_approval_never_loses_the_session_or_runs_the_effect_twice() {
    let (seed, rounds) = (seed(), rounds(3));
    eprintln!(
        "approval loop: seed={seed} rounds={rounds} (TOLE_CRASH_LOOP_SEED / TOLE_CRASH_LOOP_N)"
    );
    let mut rng = Rng(seed);
    let base = mock::spawn(0);
    for round in 0..rounds {
        let variant = round % 3;
        let ctx = format!("round {round}, variant {variant}, seed {seed}");
        let mut serve = Serve::spawn(&base);
        let id = new_session(&serve);
        let row = park(&serve, &id, "RUN-EFFECT go", &ctx);
        let effect = effect_log(&serve);
        assert_eq!(
            effect_runs(&effect),
            0,
            "{ctx}: nothing ran before the approval"
        );
        match variant {
            // Killed while the approval is pending.
            0 => std::thread::sleep(Duration::from_millis(rng.below(200))),
            // Killed inside the effect the allow started.
            1 => {
                allow(&serve, &row, &ctx);
                common::wait_until(common::WAIT, "the effect to start", || {
                    effect_runs(&effect) >= 1
                });
                std::thread::sleep(Duration::from_millis(rng.below(1300)));
            }
            // Killed after the allowed turn settled.
            _ => {
                allow(&serve, &row, &ctx);
                let file = serve.session_file(&id);
                common::wait_until(common::WAIT, "the allowed turn to settle", || {
                    last_pc(&read_log(&file, &ctx).items).as_deref() == Some("final")
                });
            }
        }
        serve.kill();

        if variant == 0 {
            let (log, runs) = (
                read_log(&serve.session_file(&id), &ctx).items,
                effect_runs(&effect),
            );
            assert_eq!((runs, settles(&log, "run_command").len()), (0, 0), "{ctx}");
            // The queue is in memory: the restarted serve has no row, and a
            // resume does not bring one back.
            let second = restart(&base, &serve, &id, &ctx);
            assert!(approvals(&second).is_empty(), "{ctx}: the row is gone");
            // The model asks again: one NEW row (not the old one, not two),
            // and one allow runs the effect once.
            let again = park(&second, &id, "RUN-EFFECT again", &ctx);
            assert_ne!(again, row, "{ctx}: the old row is not resurrected");
            assert_eq!(approvals(&second).len(), 1, "{ctx}: one pending approval");
            allow(&second, &again, &ctx);
            let file = second.session_file(&id);
            common::wait_until(common::WAIT, "the allowed turn to settle", || {
                last_pc(&read_log(&file, &ctx).items).as_deref() == Some("final")
            });
            assert_eq!(effect_runs(&effect), 1, "{ctx}: one allow, one run");
            let log = read_log(&file, &ctx).items;
            assert_eq!(
                settles(&log, "run_command").len(),
                1,
                "{ctx}: one settlement"
            );
            // The row stays listed as `approved`; nothing is left pending.
            assert!(
                approvals(&second).iter().all(|r| r["status"] != "pending"),
                "{ctx}: the row is consumed"
            );
        } else {
            // The grant went to the killed process: the restart has no row and
            // does not run the effect by itself; recovery is `tole resume`.
            let second = Serve::spawn_in(&base, serve.dir.clone(), false);
            assert!(
                approvals(&second).is_empty(),
                "{ctx}: no row after a restart"
            );
            drop(second);
            recover_and_check(&base, &serve, &id, false, &ctx);
        }
    }
}
