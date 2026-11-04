//! #381: importing a Claude Code session transcript (JSONL) into a tole
//! session that `resume` can continue.

use serde_json::json;
use std::path::PathBuf;

use tole_core::ccimport::{import_file, import_str};
use tole_core::state::Pc;
use tole_core::storage::{JsonlStorage, Storage};

const FIXTURE: &str = include_str!("fixtures/cc_session.jsonl");

fn tmpdir(tag: &str) -> PathBuf {
    // pid + nanos: a recycled pid must not collide with a live dir (#493).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let d = std::env::temp_dir().join(format!(
        "tole-ccimport-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A transcript where a tool call was interrupted (tool_use with no
/// tool_result — the user pressed Esc) must not poison every later cut
/// point: at the next prompt the call is abandoned with a synthetic error
/// entry, so the session settles again and the rest of the conversation
/// survives the import.
#[test]
fn an_interrupted_tool_call_does_not_poison_later_cut_points() {
    let interrupted = concat!(
        r#"{"type":"assistant","sessionId":"aaaa2222-3333-4444-5555-666666666666","timestamp":"2026-11-03T10:00:00.000Z","message":{"content":[{"type":"text","text":"let me check"}]}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"aaaa2222-3333-4444-5555-666666666666","timestamp":"2026-11-03T10:00:01.000Z","message":{"content":[{"type":"tool_use","id":"toolu_01","name":"Read","input":{"path":"x"}}]}}"#,
        "\n",
        // Interruption: no tool_result for toolu_01; a new prompt follows.
        r#"{"type":"user","sessionId":"aaaa2222-3333-4444-5555-666666666666","timestamp":"2026-11-03T10:00:05.000Z","message":{"content":[{"type":"text","text":"stop, never mind"}]}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"aaaa2222-3333-4444-5555-666666666666","timestamp":"2026-11-03T10:00:06.000Z","message":{"content":[{"type":"text","text":"understood"}]}}"#,
        "\n",
    );
    let dir = tmpdir("interrupt");
    let r = import_str(interrupted, &dir).unwrap();
    assert_eq!(r.session_id, "cc-aaaa2222-3333-4444-5555-666666666666");
    assert!(
        r.warnings.iter().any(|w| w.contains("abandoned tool call")),
        "{:?}",
        r.warnings
    );
    let s = JsonlStorage::open_read_only(dir.join(format!("{}.jsonl", r.session_id))).unwrap();
    let e = s.entries();
    // The interrupted intent, its synthetic error, BOTH user texts and the
    // final answer survive — nothing beyond the pre-call point was dropped.
    assert!(e.iter().any(|x| x.id == "cc_toolu_01"));
    assert_eq!(e.iter().filter(|x| x.kind.as_str() == "error").count(), 1);
    let texts: Vec<&str> = e
        .iter()
        .filter(|x| x.kind.as_str() == "message")
        .filter_map(|x| x.payload["text"].as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["let me check", "stop, never mind", "understood"]
    );
    assert_eq!(r.dropped, 0, "the whole tail must not be dropped");
    assert_eq!(s.state().pc, Pc::Final);
}

#[test]
fn fixture_maps_messages_tool_calls_and_results() {
    let dir = tmpdir("map");
    let r = import_str(FIXTURE, &dir).unwrap();
    assert_eq!(r.session_id, "cc-11111111-2222-3333-4444-555555555555");
    let s = JsonlStorage::open_read_only(dir.join(format!("{}.jsonl", r.session_id))).unwrap();
    let e = s.entries();
    let shape: Vec<(String, String)> = e
        .iter()
        .map(|e| {
            let tag = match e.kind.as_str() {
                "message" => e.payload["role"].as_str().unwrap().to_string(),
                "intent" => e.payload["tool"].as_str().unwrap().to_string(),
                other => other.to_string(),
            };
            (e.kind.as_str().to_string(), tag)
        })
        .collect();
    let want = [
        ("message", "user"),
        ("message", "assistant"),
        ("intent", "Bash"),
        ("intent", "Read"),
        ("tool_result", "tool_result"),
        ("error", "error"),
        ("message", "assistant"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(shape, want, "dangling prompt + open call are dropped");
    // Intent ids double as wire tool_call_ids; results hang under them.
    assert_eq!(e[2].id, "cc_toolu_A");
    assert_eq!(e[2].payload["input"], json!({"command": "ls"}));
    assert_eq!(
        e[3].payload["batch"],
        json!(true),
        "2nd call of one response"
    );
    assert_eq!(e[4].parent_id.as_deref(), Some("cc_toolu_A"));
    assert_eq!(e[4].payload["output"], json!("a.txt\nb.txt"));
    assert_eq!(e[5].parent_id.as_deref(), Some("cc_toolu_B"));
    assert_eq!(e[5].payload["error"], json!("file not found"));
    // CC timestamps carried over (ms since epoch).
    assert_eq!(e[0].timestamp, 1_793_527_201_000);
    // Resumable: rests at Final, like a finished tole turn.
    assert_eq!(s.state().pc, Pc::Final);
}

#[test]
fn metadata_lands_in_a_register_and_the_header_cwd() {
    let dir = tmpdir("meta");
    let r = import_str(FIXTURE, &dir).unwrap();
    let s = JsonlStorage::open_read_only(dir.join(format!("{}.jsonl", r.session_id))).unwrap();
    assert_eq!(s.cwd(), "/work/demo");
    let m = s.get_register("fact", "cc_import").unwrap();
    assert_eq!(m["sessionId"], "11111111-2222-3333-4444-555555555555");
    assert_eq!(m["model"], "claude-opus-4-1");
    assert_eq!(m["version"], "2.1.0");
    assert_eq!(m["gitBranch"], "main");
    assert_eq!(m["startedAt"], 1_793_527_201_000u64);
}

#[test]
fn malformed_and_sidechain_lines_are_skipped_with_warnings() {
    let dir = tmpdir("warn");
    let r = import_str(FIXTURE, &dir).unwrap();
    assert!(
        r.warnings.iter().any(|w| w.contains("line 9")),
        "{:?}",
        r.warnings
    );
    assert!(r.dropped > 0, "the unsettled tail is reported");
}

#[test]
fn a_second_import_of_the_same_session_is_refused_and_touches_nothing() {
    let dir = tmpdir("twice");
    let r = import_str(FIXTURE, &dir).unwrap();
    let path = dir.join(format!("{}.jsonl", r.session_id));
    let before = std::fs::read(&path).unwrap();
    let err = import_str(FIXTURE, &dir).unwrap_err().to_string();
    assert!(err.contains("already imported"), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_transcript_with_nothing_settled_is_an_error_not_a_crash() {
    let dir = tmpdir("empty");
    let err = import_str("garbage\n{\"type\":\"summary\"}\n", &dir).unwrap_err();
    assert!(err.to_string().contains("session id"), "{err}");
    let only_prompt =
        r#"{"type":"user","sessionId":"abc","message":{"role":"user","content":"hi"}}"#;
    let err = import_str(only_prompt, &dir).unwrap_err();
    assert!(err.to_string().contains("nothing to resume"), "{err}");
}

#[test]
fn stream_json_output_of_claude_p_imports_too() {
    // `claude -p --output-format stream-json`: init line with `session_id`,
    // then assistant/user lines, then a `result` line (ignored).
    let dir = tmpdir("stream");
    let t = [
        r#"{"type":"system","subtype":"init","session_id":"sj-1","model":"claude-sonnet-4","cwd":"/w"}"#,
        r#"{"type":"user","session_id":"sj-1","message":{"role":"user","content":"hi"}}"#,
        r#"{"type":"assistant","session_id":"sj-1","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        r#"{"type":"result","subtype":"success","session_id":"sj-1","result":"hello"}"#,
    ]
    .join("\n");
    let r = import_str(&t, &dir).unwrap();
    assert_eq!(r.session_id, "cc-sj-1");
    let s = JsonlStorage::open_read_only(dir.join("cc-sj-1.jsonl")).unwrap();
    assert_eq!(s.entries().len(), 2);
    assert_eq!(s.cwd(), "/w");
    assert_eq!(
        s.get_register("fact", "cc_import").unwrap()["model"],
        "claude-sonnet-4"
    );
}

#[test]
fn import_file_reads_a_path() {
    let dir = tmpdir("file");
    let src = dir.join("cc.jsonl");
    std::fs::write(&src, FIXTURE).unwrap();
    let out = tmpdir("file-out");
    assert!(import_file(&src, &out).is_ok());
    assert!(import_file(&dir.join("missing.jsonl"), &out).is_err());
}

#[test]
fn an_imported_session_continues_with_a_new_turn() {
    use tole_core::mock::MockProvider;
    use tole_core::provider::ProviderOutput;
    use tole_core::tool::ToolRegistry;
    use tole_core::turn::{run_turn, TurnOutcome};

    let dir = tmpdir("continue");
    let r = import_str(FIXTURE, &dir).unwrap();
    let mut s = JsonlStorage::open(dir.join(format!("{}.jsonl", r.session_id))).unwrap();
    let before = s.entries().len();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "continued".into(),
    }]);
    let out = run_turn(&mut s, &mut p, &ToolRegistry::default(), "and now?").unwrap();
    assert!(matches!(out, TurnOutcome::Final { ref text, .. } if text == "continued"));
    // Append-only: the imported prefix is untouched, the new turn follows it.
    assert_eq!(s.entries().len(), before + 2);
    assert_eq!(s.state().pc, Pc::Final);
}
