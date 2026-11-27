//! #460 acceptance: stream, approve, cancel, hooks, escapes, model picker, steering.

use crate::common::{self};
use crate::mock::{spawn_mock, spawn_mock_recording, Bodies};
use crate::pty::{workdir, Pty};
#[test]
fn tui_streams_approves_cancels_and_leaves_an_ordinary_session() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:"); // footer rendered

    // 1. streamed answer
    t.send("hello\r");
    t.expect("STREAM-ANSWER");
    // The answer text lands before the turn is over: a prompt typed now
    // would steer the still-running turn instead of starting a new one
    // (#877), so wait for the idle hint before every next prompt.
    t.idle();

    // 2. a write asks inline; `y` lets it run
    t.send("write it\r");
    t.expect("create the file out.txt"); // the inline approval prompt
    assert!(
        !dir.join("out.txt").exists(),
        "must not write before approval"
    );
    t.send("y");
    t.expect("WROTE-OK");
    t.idle();
    assert!(dir.join("out.txt").exists(), "approved write must run");

    // 3. Shift+Enter (kitty CSI-u) makes a newline instead of submitting
    t.send("multi1\x1b[13;2umulti2\r");
    t.expect("STREAM-ANSWER");
    // If "slow" were typed while this turn still runs it would steer it, and
    // the Esc below would cancel the already-finished turn (#877).
    t.idle();

    // 4. Esc cancels a running turn
    t.send("slow\r");
    t.expect("working");
    t.send("\x1b");
    t.expect("(turn cancelled)");

    // 5. quit; the session is an ordinary one
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));

    let sess_dir = dir.join(".tole/sessions");
    let file = std::fs::read_dir(&sess_dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .expect("session file");
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert!(
        log.contains("multi1\\nmulti2"),
        "multi-line prompt recorded: {log}"
    );
    let id = file
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".jsonl")
        .to_string();
    let listed =
        common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &dir.with_extension("home"), &[])
            .arg("sessions")
            .current_dir(&dir)
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(&id),
        "sessions lists {id}: {}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// The TUI face must wire `--on-pretool` like every other command: a deny
/// (exit 2) stops the write even under `--yes`, and the denial is shown.
#[cfg(feature = "shell-tools")]
#[test]
fn tui_enforces_a_deny_only_pretool_hook() {
    let (url, dir) = (spawn_mock(), workdir());
    let script = dir.join("deny.sh");
    std::fs::write(&script, "#!/bin/sh\necho policy says no\nexit 2\n").unwrap();
    let args = [
        "--on-pretool".to_string(),
        format!("/bin/sh {}", script.display()),
        "tui".into(),
        "--yes".into(),
    ];
    let mut t = Pty::spawn_with(&dir, &url, &args);
    t.expect("effort:");
    t.send("write it\r");
    t.expect("approval denied for 'write_file'");
    assert!(!dir.join("out.txt").exists(), "denied write must not run");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
}

/// Provider text with ESC/CR must reach the terminal only as visible
/// `\u{..}` escapes, never as the raw sequence (terminal injection).
#[test]
fn tui_never_passes_escape_sequences_from_provider_text() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("evil\r");
    t.expect("EV\\u{1b}[2J\\u{d}IL");
    // Slash commands are ignored while a turn runs: `/quit` sent before the
    // turn settles is dropped and the process never exits (a CI flake).
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    let seen = String::from_utf8_lossy(&o);
    assert!(
        !seen.contains("\x1b[2J\rIL"),
        "raw injection reached the pty"
    );
    assert!(!seen.contains("[2J\rIL"), "raw injection reached the pty");
}

/// #461 acceptance: switch the model through the picker (Ctrl+L) and the
/// thinking level through `/thinking`; the NEXT provider request carries
/// both, the status shows them, and a later `tole resume` still sends them
/// (they are session registers, not process state).
#[test]
fn tui_model_picker_and_thinking_reach_the_next_request_and_survive_resume() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("hello\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // Ctrl+L: picker over /models, filter as you type, Enter picks
    t.send("\x0c");
    t.expect("model>");
    t.send("alt");
    t.expect("alt-model");
    t.send("\r");
    t.expect("model → alt-model");

    // /thinking low (the editor border shows the level)
    t.send("/thinking low\r");
    t.expect("thinking → low");
    t.expect("thinking: low");

    // unknown commands are refused with a hint and nothing is sent
    t.send("/mod\r");
    t.expect("did you mean /model");

    t.send("second\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    {
        let b = bodies.lock().unwrap();
        assert_eq!(b.len(), 2, "only the two prompts reached the provider");
        assert_eq!(b[0]["model"], "m");
        assert!(b[0].get("reasoning_effort").is_none(), "{}", b[0]);
        assert_eq!(
            b[1]["model"], "alt-model",
            "next call carries the new model"
        );
        assert_eq!(b[1]["reasoning_effort"], "low", "and the new effort");
    }

    t.send("/name e2e-run\r");
    t.expect("named e2e-run");
    t.send("/session\r");
    t.expect("alt-model");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));

    let file = std::fs::read_dir(dir.join(".tole/sessions"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .expect("session file");
    let id = file
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".jsonl")
        .to_string();
    let tole = |args: &[&str]| {
        common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &dir.with_extension("home"), &[])
            .args(args)
            .current_dir(&dir)
            .env("TOLE_BASE_URL", &url)
            .env("TOLE_MODEL", "m")
            .env("TOLE_API_KEY", "k")
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap()
    };
    let out = tole(&["resume", &id, "third"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let b = bodies.lock().unwrap();
    let last = b.last().unwrap();
    assert_eq!(last["model"], "alt-model", "model register survives resume");
    assert_eq!(
        last["reasoning_effort"], "low",
        "effort register survives resume"
    );
    drop(b);
    let listed = tole(&["sessions", "--all"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("e2e-run"),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// #462 acceptance: steer a running turn (soft interrupt, #377), queue
/// follow-ups that run one by one after it settles, and get the queue back in
/// the editor on cancel: all on a pty against the mock provider.
#[test]
fn tui_steers_queues_and_returns_the_queue_on_cancel() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");

    // 1. Enter while the turn runs steers: the first answer is withdrawn at
    //    the safe point and the next provider call carries the steering text.
    t.send("hold-a\r");
    t.expect("working");
    t.send("go-left\r");
    t.expect("[steer]");
    t.expect("STREAM-ANSWER");
    t.idle();

    // 2. Alt+Enter (ESC CR) queues; the rows show; after the turn settles the
    //    head runs, then the next, one at a time.
    t.send("hold-b\r");
    t.expect("working");
    t.send("qa\x1b\r");
    t.send("qb\x1b\r");
    t.expect("queued: qb");
    t.expect("HELD-ANSWER");
    t.expect("> qa");
    t.expect("> qb");
    t.idle();

    // 3. Esc cancels and puts the queue back in the editor (not lost).
    t.send("slow\r");
    t.expect("working");
    t.send("qc\x1b\r");
    t.expect("queued: qc");
    t.send("\x1b");
    t.expect("(turn cancelled)");
    t.send("\r"); // the editor holds "qc" again: sending it runs it
    t.expect("STREAM-ANSWER");
    // `/quit` is ignored while a turn runs (#877): wait for the turn to end.
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));

    let all = bodies.lock().unwrap();
    // the steering text reached a provider call as the last (user) message
    assert!(
        all.iter().any(|b| {
            let last = b["messages"].as_array().and_then(|m| m.last()).unwrap();
            last["role"] == "user"
                && last["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("owner interrupt") && c.contains("go-left"))
        }),
        "steering text never reached the provider: {all:?}"
    );
    // the last request carries every prompt, in the order they ran
    let last = all.last().unwrap()["messages"].as_array().unwrap();
    let users: Vec<&str> = last
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    let mut at = 0;
    for want in ["hold-a", "go-left", "hold-b", "qa", "qb", "slow", "qc"] {
        let i = users[at..]
            .iter()
            .position(|u| u.contains(want))
            .unwrap_or_else(|| panic!("{want} missing/out of order in {users:?}"));
        at += i + 1;
    }
}
