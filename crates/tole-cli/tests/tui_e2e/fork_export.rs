//! `/fork`, `/tree`, `/copy` and `/export` on the real binary.

use crate::common::{self};
use crate::mock::{spawn_mock, spawn_mock_recording, Bodies};
use crate::paste_image::user_contents;
use crate::pty::{workdir, Pty};
use serde_json::{json, Value};
use std::path::Path;
/// The user-message texts recorded in a session file, and its `forkedFrom`
/// header (`session`, `entry`) if it is a fork.
fn session_facts(path: &Path) -> (Vec<String>, Option<(String, String)>) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let origin = header.get("forkedFrom").map(|f| {
        (
            f["session"].as_str().unwrap().to_string(),
            f["entry"].as_str().unwrap().to_string(),
        )
    });
    let mut users = Vec::new();
    for l in lines {
        let v: Value = serde_json::from_str(l).unwrap();
        let recs = v.as_array().cloned().unwrap_or_else(|| vec![v]);
        for r in recs {
            if r["kind"] == "entry" && r["payload"]["role"] == "user" {
                users.push(r["payload"]["text"].as_str().unwrap_or("").to_string());
            }
        }
    }
    (users, origin)
}

/// #463: `/fork` cuts the open session at its last settled entry into a new
/// session and switches to it (the next message lands there, with the
/// history, and not in the original); `/tree` lists the family with the open
/// session marked and picking the parent switches back to it.
#[test]
fn tui_fork_continues_in_the_new_session_and_tree_switches_back() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let sessions = dir.join(".tole/sessions");
    let tole = |args: &[&str]| {
        let o =
            common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &dir.with_extension("home"), &[])
                .args(args)
                .current_dir(&dir)
                .env("TOLE_NO_UPDATE_CHECK", "1")
                .output()
                .unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("first\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // a bogus entry is refused with the core's error
    t.send("/fork no-such-entry\r");
    t.expect("(fork: forking");
    t.idle();

    t.send("/fork\r");
    t.expect("forked → ");
    t.idle();
    let mut all: Vec<String> = std::fs::read_dir(&sessions)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter_map(|n| n.strip_suffix(".jsonl").map(str::to_string))
        .collect();
    all.sort();
    assert_eq!(all.len(), 2, "the original and one fork: {all:?}");
    let file = |id: &str| sessions.join(format!("{id}.jsonl"));
    let (orig, fork) = if session_facts(&file(&all[0])).1.is_some() {
        (all[1].clone(), all[0].clone())
    } else {
        (all[0].clone(), all[1].clone())
    };
    let (src, _entry) = session_facts(&file(&fork)).1.expect("fork header");
    assert_eq!(src, orig, "forkedFrom names the original");

    // the next message lands in the fork, with the carried history
    t.send("second\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    {
        let b = bodies.lock().unwrap();
        assert_eq!(
            user_contents(b.last().unwrap()),
            [json!("first"), json!("second")],
            "the fork carries the history"
        );
    }
    assert_eq!(
        session_facts(&file(&orig)).0,
        ["first"],
        "original untouched"
    );
    assert_eq!(session_facts(&file(&fork)).0, ["first", "second"]);

    // /tree: both sessions, the open one marked and preselected
    t.send("/tree\r");
    t.expect("tree>");
    t.expect(&orig);
    t.expect("(current)");
    // Up to the parent, Enter switches there
    t.send("\x1b[A");
    t.send("\r");
    t.expect(&format!("resumed {orig}"));
    t.idle();
    t.send("third\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert_eq!(session_facts(&file(&orig)).0, ["first", "third"]);
    assert_eq!(
        session_facts(&file(&fork)).0,
        ["first", "second"],
        "nothing sent after switching back reached the fork"
    );

    // the fork is listed like any session (face "branch") and nests in the tree
    let listed = tole(&["sessions", "--all"]);
    assert!(listed.contains(&fork), "{listed}");
    let tree = tole(&["sessions", "--tree"]);
    assert!(tree.contains(&format!("└─ {fork}")), "{tree}");
}

/// #463 `/copy`: the last answer reaches the terminal clipboard as ONE
/// OSC 52 sequence whose payload is base64 only; hostile answer text never
/// appears raw, and the note says what was attempted, not that it worked.
#[test]
fn tui_copy_sends_the_last_answer_as_osc52_base64() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("/copy\r");
    t.expect("nothing to copy");
    t.send("hello\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/copy\r");
    t.expect("\x1b]52;c;U1RSRUFNLUFOU1dFUg==\x07");
    t.expect("characters to the terminal clipboard");
    // An answer with ESC / CR bytes: only its base64 is sent.
    t.send("evil\r");
    t.expect("EV\\u{1b}[2J\\u{d}IL");
    t.idle();
    t.send("/copy\r");
    t.expect("\x1b]52;c;RVYbWzJKDUlM\x07");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    let seen = String::from_utf8_lossy(&o);
    assert!(
        !seen.contains("[2J\rIL"),
        "raw answer bytes reached the pty"
    );
    assert!(!seen.contains("copied"), "never claims success");
}

/// #463 `/export`: the transcript is written under the workspace, escaped,
/// and a second run to the same path is refused instead of overwriting.
#[test]
fn tui_export_writes_the_transcript_and_refuses_to_overwrite() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("hello\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/export\r");
    t.expect("transcript written to");
    t.idle();
    let exports = dir.join(".tole/exports");
    let html = std::fs::read_dir(&exports)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".html"))
        .expect("exported page")
        .path();
    let page = std::fs::read_to_string(&html).unwrap();
    assert!(page.contains("STREAM-ANSWER") && page.contains("Content-Security-Policy"));
    assert!(!page.contains("<script"));
    t.send("/export\r");
    t.expect("pick another path");
    t.idle();
    t.send("/export ../escape.html\r");
    t.expect_unwrapped("outside the workspace jail");
    t.idle();
    t.send("/export t.jsonl\r");
    t.expect("transcript written to");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&html).unwrap(),
        page,
        "not overwritten"
    );
    assert!(!dir.parent().unwrap().join("escape.html").exists());
    let jsonl = std::fs::read_to_string(dir.join("t.jsonl")).unwrap();
    assert!(jsonl
        .lines()
        .all(|l| serde_json::from_str::<Value>(l).is_ok()));
    assert!(jsonl.contains("STREAM-ANSWER"));
}
