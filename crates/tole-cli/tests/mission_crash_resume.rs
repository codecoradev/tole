//! #1018, third slice: SIGKILL the REAL `tole mission` at points of its turn,
//! again and again, and recover it the way the docs say. `serve_crash_resume`
//! and `serve_crash_effect` do the same for `tole serve`.
//!
//! The rounds take turns between three kill points: inside the model call
//! (the provider mock holds the request for 1.5 s), inside a tool effect (a
//! `run_command` that appends to `effect.log` and sleeps one second, so the
//! test sees it start), and at a random moment since the process was spawned
//! (startup, session creation, either of the above). `TOLE_CRASH_LOOP_N` is the
//! number of rounds, `TOLE_CRASH_LOOP_SEED` replays the delays.
//!
//! What is pinned, and where the answer is not "it just works":
//! - The session log parses after every kill, and a user message is in it once
//!   or not at all.
//! - **`tole mission --resume <id>` does NOT recover a turn that was cut off.**
//!   `mission.rs` promises that a crash mid-mission "resumes exactly where it
//!   stopped", but the mission loop starts its next turn with `run_turn`, which
//!   refuses a session whose register is not at rest: the command exits 1
//!   with `status: error` and `run_turn requires pc Idle or Final`, and the
//!   transcript is left as it was. The way through is two steps: `tole resume
//!   <id>` finishes the cut-off turn (see `serve_crash_effect.rs` for what it
//!   does with an effect), then `tole mission --resume <id>` continues the
//!   mission. One test below pins the refusal and the two-step recovery.
//! - A mission run by `tole serve` (`POST /missions`) that stopped at an
//!   approval and was killed there loses the queue row (it is in memory) and
//!   has not run the effect; the session is parked, so `tole mission
//!   --resume` continues it without the two steps and the effect still has
//!   not run: nothing asks again unless the model does. A kill before the
//!   denial is written (the row is queued first) leaves a cut-off turn
//!   instead: the two steps again (#1018, an intermittent CI failure).
//! - After the two steps the mission completes, the goal was sent once, the
//!   `fact/mission` register says `complete` and the effect ran as often as
//!   the contract says: once, plus once more when the replay was approved.

#![cfg(all(unix, feature = "shell-tools"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;
mod crash_loop;
mod serve_harness;
use common::{ChildGuard, TestDir};
use crash_loop::mock::{self, ANSWER};
use crash_loop::{
    answers_with, effect_runs, entry_seqs, intents, last_pc, mid_flight, read_log, rounds, seed,
    settles, user_messages_with, Rng,
};
use serde_json::{json, Value};
use serve_harness::{home_of, Serve};
use tole_core::storage::{JsonlStorage, Storage};

/// A hermetic `tole` command over the round's directory, workspace `w` as its
/// working directory and sessions in `sessions`.
fn tole(dir: &TestDir, base: &str, args: &[&str]) -> Command {
    let mut cmd = common::hermetic_command(
        env!("CARGO_BIN_EXE_tole"),
        &dir.join("home"),
        &[
            ("TOLE_BASE_URL", base),
            ("TOLE_MODEL", "mock-model"),
            ("TOLE_API_KEY", "sk-test"),
            ("TOLE_NO_UPDATE_CHECK", "1"),
        ],
    );
    cmd.arg("-s")
        .arg(dir.join("sessions"))
        .args(args)
        .current_dir(dir.join("w"))
        .stdin(Stdio::null());
    cmd
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(mut cmd: Command) -> Ran {
    let out = cmd.output().unwrap();
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn mission_cmd(dir: &TestDir, base: &str, extra: &[&str], goal: &str) -> Command {
    let w = dir.join("w");
    let mut args = vec!["mission", "--yes", "--max-steps", "12", "--workspace"];
    args.push(w.to_str().unwrap());
    args.extend_from_slice(extra);
    args.push(goal);
    tole(dir, base, &args)
}

/// The one session file of the round, once there is one.
fn session_file(dir: &TestDir) -> Option<PathBuf> {
    std::fs::read_dir(dir.join("sessions"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
}

fn id_of(file: &Path) -> String {
    file.file_stem().unwrap().to_string_lossy().into_owned()
}

/// The `fact/mission` status the session file ends with.
fn mission_status(file: &Path) -> String {
    let s = JsonlStorage::open_read_only(file).unwrap();
    s.get_register("fact", "mission").unwrap()["status"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Spawn a mission, kill it at the point of `kind`, and return the dir, the
/// session file (none when the kill came before it existed) and the tag.
fn killed_mission(base: &str, kind: usize, tag: &str, rng: &mut Rng) -> (TestDir, Option<PathBuf>) {
    let dir = TestDir::new("tole-mission-crash");
    std::fs::create_dir_all(dir.join("w")).unwrap();
    let goal = if kind == 1 {
        format!("RUN-EFFECT {tag}")
    } else {
        format!("HOLD-PROVIDER {tag}")
    };
    let mut cmd = mission_cmd(&dir, base, &[], &goal);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = ChildGuard(cmd.spawn().unwrap());
    match kind {
        // Inside the model call: the user message is durable, the mock holds.
        0 => {
            let needle = format!("MISSION GOAL: {goal}");
            common::wait_until(common::WAIT, "the goal to be durable", || {
                session_file(&dir)
                    .and_then(|f| std::fs::read_to_string(f).ok())
                    .is_some_and(|t| t.contains(&needle))
            });
            std::thread::sleep(Duration::from_millis(rng.below(1200)));
        }
        // Inside the tool effect.
        1 => {
            let effect = dir.join("w/effect.log");
            common::wait_until(common::WAIT, "the effect to start", || {
                effect_runs(&effect) >= 1
            });
            std::thread::sleep(Duration::from_millis(rng.below(1300)));
        }
        // Anywhere since the spawn.
        _ => std::thread::sleep(Duration::from_millis(rng.below(1600))),
    }
    let _ = child.kill();
    let _ = child.wait();
    let file = session_file(&dir);
    (dir, file)
}

#[test]
fn a_mission_killed_at_a_random_point_resumes_in_two_steps_and_completes() {
    let (seed, rounds) = (seed(), rounds(3));
    eprintln!(
        "mission loop: seed={seed} rounds={rounds} (TOLE_CRASH_LOOP_SEED / TOLE_CRASH_LOOP_N)"
    );
    let mut rng = Rng(seed);
    let base = mock::spawn(1500);
    let (mut cut_off, mut settled, mut none) = (0, 0, 0);
    for round in 0..rounds {
        let kind = round % 3;
        let allow = round % 2 == 1;
        let tag = format!("m{seed}-{round}");
        let ctx = format!("round {round}, kind {kind}, allow {allow}, seed {seed}");
        let (dir, file) = killed_mission(&base, kind, &tag, &mut rng);
        let effect = dir.join("w/effect.log");
        let Some(file) = file else {
            // Killed before the session file existed: nothing was written.
            none += 1;
            assert!(std::fs::read_dir(dir.join("sessions")).map_or(true, |d| d.count() == 0));
            continue;
        };
        let id = id_of(&file);
        // Killed mid-way through creating the file (an empty or headerless
        // file): there is no session to resume, and nothing trusts it.
        let text = std::fs::read(&file).unwrap();
        if !text.contains(&b'\n') {
            none += 1;
            continue;
        }
        let log = read_log(&file, &ctx);
        let sent = user_messages_with(&log.items, &tag);
        assert!(sent <= 1, "{ctx}: the goal is in the log {sent} times");
        let pc = last_pc(&log.items);
        let mid = mid_flight(pc.as_deref()) && sent == 1;
        let pending_effect = intents(&log.items, "run_command").len() == 1
            && settles(&log.items, "run_command").is_empty();
        let runs = effect_runs(&effect);

        // Step 1: `tole mission --resume` on a cut-off turn is refused and
        // leaves the transcript as it was (PINNED, see the module doc).
        let before = entry_seqs(&log.items);
        let first = run(mission_cmd(&dir, &base, &["--resume", &id], "resume"));
        if mid {
            cut_off += 1;
            assert_eq!(first.code, Some(1), "{ctx}: {}", first.stderr);
            assert!(
                first.stdout.contains("status: error"),
                "{ctx}: {}",
                first.stdout
            );
            assert!(
                first.stderr.contains("run_turn requires pc Idle or Final"),
                "{ctx}: {}",
                first.stderr
            );
            let now = read_log(&file, &ctx);
            assert_eq!(
                entry_seqs(&now.items),
                before,
                "{ctx}: the refusal wrote entries"
            );
            assert_eq!(
                last_pc(&now.items),
                pc,
                "{ctx}: the refusal moved the register"
            );
            assert_eq!(
                effect_runs(&effect),
                runs,
                "{ctx}: the refusal ran the effect"
            );

            // Step 2a: `tole resume` finishes the turn, the effect as the
            // contract says.
            let grant: &[&str] = if allow {
                &["--allow", "run_command"]
            } else {
                &[]
            };
            let mut args = vec!["resume"];
            args.extend_from_slice(grant);
            args.push(&id);
            let second = run(tole(&dir, &base, &args));
            let expect_runs = runs + usize::from(pending_effect && allow);
            if pending_effect && !allow {
                // Refused replay: parked, exit 2, nothing runs again.
                assert_eq!(second.code, Some(2), "{ctx}: {}", second.stderr);
            } else {
                assert_eq!(second.code, Some(0), "{ctx}: {}", second.stderr);
            }
            assert_eq!(effect_runs(&effect), expect_runs, "{ctx}: replays");
        } else if sent == 1 {
            settled += 1;
        }

        // Step 2b: `tole mission --resume` continues the mission and it ends.
        let ended = run(mission_cmd(&dir, &base, &["--resume", &id], "resume"));
        if sent == 1 || mid {
            assert_eq!(
                ended.code,
                Some(0),
                "{ctx}: {}{}",
                ended.stdout,
                ended.stderr
            );
            assert!(
                ended.stdout.contains("status: complete"),
                "{ctx}: {}",
                ended.stdout
            );
            assert_eq!(mission_status(&file), "complete", "{ctx}: the register");
        }
        let end = read_log(&file, &ctx);
        assert!(!end.torn, "{ctx}: the file ends in a torn line");
        assert_eq!(
            user_messages_with(&end.items, &tag),
            sent,
            "{ctx}: the goal is sent once"
        );
        assert!(
            effect_runs(&effect) <= runs + usize::from(pending_effect && allow),
            "{ctx}"
        );
        assert!(
            settles(&end.items, "run_command").len() <= 1,
            "{ctx}: settled once"
        );
        if sent == 1 {
            assert_eq!(last_pc(&end.items).as_deref(), Some("final"), "{ctx}");
            assert!(
                answers_with(&end.items, ANSWER) >= 1,
                "{ctx}: the mission answered"
            );
        }
    }
    eprintln!("mission loop: {cut_off} cut off mid-turn, {settled} already settled, {none} before the session existed");
}

/// `tole <args>` over a serve's directory (its `.tole/sessions`), the way the
/// operator recovers a mission that `tole serve` was running.
fn cli_over(serve_dir: &Path, base: &str, args: &[&str]) -> Command {
    let mut cmd = common::hermetic_command(
        env!("CARGO_BIN_EXE_tole"),
        &home_of(serve_dir),
        &[
            ("TOLE_BASE_URL", base),
            ("TOLE_MODEL", "mock-model"),
            ("TOLE_API_KEY", "sk-test"),
            ("TOLE_NO_UPDATE_CHECK", "1"),
        ],
    );
    cmd.arg("-s")
        .arg(serve_dir.join(".tole/sessions"))
        .args(args)
        .current_dir(serve_dir)
        .stdin(Stdio::null());
    cmd
}

/// Start a mission over serve and wait until its `run_command` is a pending
/// row of the approvals queue: `(serve, mission id, tag)`.
fn serve_mission_at_approval(base: &str, tag: &str, ctx: &str) -> (Serve, String) {
    let serve = Serve::spawn(base);
    let goal = json!({"goal": format!("RUN-EFFECT {tag}")}).to_string();
    let (code, v) = serve.http("POST", "/missions", Some(&goal));
    assert_eq!(code, 202, "{ctx}: {v}");
    let id = v["id"].as_str().unwrap().to_string();
    common::wait_until(common::WAIT, "the approval to be queued", || {
        let (code, v) = serve.http("GET", &format!("/approvals?session={id}"), None);
        assert_eq!(code, 200, "{ctx}: {v}");
        v["approvals"].as_array().unwrap().len() == 1
    });
    (serve, id)
}

/// The turn is parked after its denial (`parked_after_denial` in tole-core):
/// the last entry of the log is the denial note.
fn parked(items: &[Value]) -> bool {
    crash_loop::last_entry(items).is_some_and(|e| e["payload"]["denial_note"] == json!(true))
}

/// What a killed serve mission leaves, and the recovery of each state. `tag`
/// is the goal's marker, `ctx` names the round.
///
/// - Parked (the denial note is the last entry): `tole mission --resume`
///   continues it directly.
/// - Cut off before the note (the register still says `planning`, the window
///   between the queued row and the denial's commit): the same refusal as any
///   cut-off turn, so `tole resume` first, then the mission.
///
/// Either way the lost row does not run the effect, the log parses and the
/// goal is in it once. Returns whether the log was parked at the kill.
fn recover_serve_mission(base: &str, dir: &Path, file: &Path, tag: &str, ctx: &str) -> bool {
    let effect = dir.join("effect.log");
    let id = id_of(file);
    let log = read_log(file, ctx);
    assert_eq!(user_messages_with(&log.items, tag), 1, "{ctx}");
    assert_eq!(effect_runs(&effect), 0, "{ctx}: nothing ran");
    assert!(settles(&log.items, "run_command").is_empty(), "{ctx}");
    let was_parked = parked(&log.items);
    if !was_parked {
        // Cut off: nothing but `Planning` and no denial note.
        assert_eq!(last_pc(&log.items).as_deref(), Some("planning"), "{ctx}");
        let before = entry_seqs(&log.items);
        let refused = run(cli_over(dir, base, &["mission", "--resume", &id, "resume"]));
        assert_eq!(refused.code, Some(1), "{ctx}: {}", refused.stderr);
        assert!(
            refused
                .stderr
                .contains("run_turn requires pc Idle or Final"),
            "{ctx}: {}",
            refused.stderr
        );
        assert_eq!(
            entry_seqs(&read_log(file, ctx).items),
            before,
            "{ctx}: the refusal wrote entries"
        );
        // `tole resume` replans; the model asks again, nothing is allowed.
        let second = run(cli_over(dir, base, &["resume", &id]));
        assert_eq!(second.code, Some(2), "{ctx}: {}", second.stderr);
        assert_eq!(effect_runs(&effect), 0, "{ctx}: resume without an allow");
    }
    let ended = run(cli_over(dir, base, &["mission", "--resume", &id, "resume"]));
    assert_eq!(
        ended.code,
        Some(0),
        "{ctx}: {}{}",
        ended.stdout,
        ended.stderr
    );
    assert!(
        ended.stdout.contains("status: complete"),
        "{ctx}: {}",
        ended.stdout
    );
    assert_eq!(mission_status(file), "complete", "{ctx}");
    assert_eq!(
        effect_runs(&effect),
        0,
        "{ctx}: a resume does not run the lost approval"
    );
    let end = read_log(file, ctx);
    assert!(!end.torn, "{ctx}: the file ends in a torn line");
    assert_eq!(user_messages_with(&end.items, tag), 1, "{ctx}");
    was_parked
}

/// A mission started over serve (`POST /missions`) stops at a `run_command`
/// that waits in the approvals queue. Kill the serve at a random moment after
/// the row is visible: the row is gone with the process, the effect has not
/// run, the session log parses, and the mission is recovered (see
/// [`recover_serve_mission`]). The kill may land before the denial is written,
/// which leaves a cut-off turn instead of a parked one; the round takes the
/// recovery its log needs and neither is skipped.
#[test]
fn a_serve_mission_killed_at_a_pending_approval_loses_the_row_not_the_session() {
    let (seed, rounds) = (seed(), rounds(2));
    eprintln!("mission approval loop: seed={seed} rounds={rounds}");
    let mut rng = Rng(seed);
    let base = mock::spawn(0);
    let mut parked_rounds = 0;
    for round in 0..rounds {
        let tag = format!("a{seed}-{round}");
        let ctx = format!("round {round}, seed {seed}");
        let (mut serve, id) = serve_mission_at_approval(&base, &tag, &ctx);
        std::thread::sleep(Duration::from_millis(rng.below(200)));
        serve.kill();
        let file = serve.session_file(&id);
        let rows_after = {
            let second = Serve::spawn_in(&base, serve.dir.clone(), false);
            let (_, v) = second.http("GET", &format!("/approvals?session={id}"), None);
            v["approvals"].as_array().unwrap().len()
        };
        assert_eq!(rows_after, 0, "{ctx}: the row died with the process");
        if recover_serve_mission(&base, &serve.dir, &file, &tag, &ctx) {
            parked_rounds += 1;
        }
    }
    eprintln!("mission approval loop: {parked_rounds} of {rounds} parked at the kill");
}

/// The kill that lands before the denial is written, made certain: the serve
/// is killed once the turn is parked, and the log is then cut back to what a
/// kill earlier would have left (`cut` trailing lines gone: the mission
/// summary and the denial note, and for 3 the `error` entry too). The log is
/// append-only and written line by line, so a prefix of it is exactly what the
/// kill leaves. `a_serve_mission_killed_at_a_pending_approval_...` meets this
/// state only when the kill is slow to land (a busy CI machine, #1018).
#[test]
fn a_serve_mission_killed_before_its_denial_is_written_needs_two_steps() {
    let base = mock::spawn(0);
    for (round, cut) in [2usize, 3].into_iter().enumerate() {
        let tag = format!("c{seed}-{round}", seed = seed());
        let ctx = format!("cut {cut}, tag {tag}");
        let (mut serve, id) = serve_mission_at_approval(&base, &tag, &ctx);
        let file = serve.session_file(&id);
        // The turn is parked once the mission summary is the file's last line.
        common::wait_until(common::WAIT, "the turn to park", || {
            std::fs::read_to_string(&file)
                .is_ok_and(|t| t.lines().last().is_some_and(|l| l.contains("\"fact\"")))
        });
        serve.kill();
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let keep = lines[..lines.len() - cut].join("\n") + "\n";
        std::fs::write(&file, keep).unwrap();
        let was_parked = recover_serve_mission(&base, &serve.dir, &file, &tag, &ctx);
        assert!(!was_parked, "{ctx}: the cut log is not parked");
    }
}
