//! #597: the shared shape detector (`uteke-privacy`) runs after the
//! known-value pass at every sink `SecretSet` covers, so a secret tole never
//! knew (pasted by the owner, printed by a command) is masked too, as
//! `prefix[REDACTED:<kind>]`.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use std::sync::Mutex;
use test_dir::TestDir;
use tole_core::entry::{Entry, EntryType, NewEntry};
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::secrets::SecretSet;
use tole_core::storage::{Commit, JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};

/// Fake secrets of two shapes, assembled at run time (no scanner bait).
fn gh() -> String {
    ["ghp", "_aB3dE5gH7jK9mN1pQ3rS5tU7vW9xY1zA3bC5"].concat()
}
fn aws() -> String {
    ["AKIA", "QWERTYUIOP234567"].concat()
}

/// An empty set and a set holding an unrelated known secret.
fn sets() -> Vec<SecretSet> {
    vec![
        SecretSet::default(),
        SecretSet::default().with("OTHER_TOKEN", "other-known-value-0597"),
    ]
}

fn assert_masked(what: &str, text: &str) {
    for b in [gh(), aws()] {
        assert!(!text.contains(&b), "{what} leaks {b}:\n{text}");
    }
    assert!(text.contains("[REDACTED:"), "{what} has no mask:\n{text}");
}

#[test]
fn the_mask_is_prefix_kind_and_the_pass_is_idempotent() {
    for s in sets() {
        let once = s.redact(&format!("token {}", gh())).into_owned();
        assert_eq!(once, "token ghp_[REDACTED:github-token]");
        assert_eq!(s.redact(&once), once, "second pass changes nothing");
        let r = s.redact_value(&json!({"a": [format!("k {}", aws())]}));
        assert_eq!(s.redact_value(&r), r);
    }
    // Text with no shape is returned borrowed: no copy on the hot path.
    assert!(matches!(
        SecretSet::default().redact("plain words only"),
        std::borrow::Cow::Borrowed(_)
    ));
}

#[test]
fn known_values_keep_their_name_the_detector_only_sees_the_rest() {
    // A KNOWN value that is also token-shaped must become «NAME», not the
    // detector's mask: the known pass runs first.
    let s = SecretSet::default().with("GITHUB_TOKEN", &gh());
    let out = s
        .redact(&format!("known {} unknown {}", gh(), aws()))
        .into_owned();
    assert!(out.contains("known «GITHUB_TOKEN» unknown "), "{out}");
    assert!(out.contains("[REDACTED:aws-access-key]"), "{out}");
    assert!(!out.contains(&gh()) && !out.contains(&aws()));
    // The marker itself survives where it sits inside a secret shape.
    let s = SecretSet::default()
        .with_url_password("MCP_URL", "https://bob:hunter2-long@example.com/mcp");
    assert_eq!(
        s.redact("https://bob:hunter2-long@example.com/mcp"),
        "https://bob:«MCP_URL»@example.com/mcp"
    );
}

// ---- the no-leak sweep: every sink the redactor covers ----

/// Sinks that call `SecretSet::redact` / `redact_value` directly (the web
/// query and URL checks, MCP arguments, system-one bodies, router records,
/// advisor and memory text) reduce to these entry points.
#[test]
fn entry_points_mask_every_string_key_and_nesting() {
    for s in sets() {
        assert_masked("redact", &s.redact(&format!("x {} y {}", gh(), aws())));
        let v = json!({gh(): [format!("k {}", aws())], "n": {"deep": [gh()]}});
        assert_masked("redact_value", &s.redact_value(&v).to_string());
        let capped = tole_core::router::redact_cap(&format!("why {}", gh()), &s, 200);
        assert_masked("router::redact_cap", &capped);
    }
}

fn payloads() -> Vec<(&'static str, Value)> {
    vec![
        (
            "message",
            json!({"role": "user", "text": format!("my key {}", gh())}),
        ),
        (
            "intent",
            json!({"tool": "run_command", "input": {"command": format!("curl -H 'X: {}'", aws())},
                   "replay": "unsafe"}),
        ),
        (
            "tool_result",
            json!({"ok": true, "output": {"stdout": format!("AWS {}", aws()), "n": gh()}}),
        ),
        (
            "error",
            json!({"ok": false, "error": format!("auth failed {}", gh())}),
        ),
    ]
}

/// The session log: every entry kind, memory equals disk, reopen equals both.
#[test]
fn the_session_log_stores_only_masked_text() {
    for (i, s) in sets().into_iter().enumerate() {
        let dir = TestDir::new(&format!("tole-597-log{i}"));
        let mut st = JsonlStorage::create(&dir, "l", None).unwrap();
        st.set_secrets(s);
        let mut parent: Option<String> = None;
        for (kind, payload) in payloads() {
            let c = st
                .commit(Commit::new().entry(NewEntry {
                    id: None,
                    parent_id: parent.clone(),
                    kind: EntryType::from_string(kind),
                    payload,
                    timestamp: 1,
                }))
                .unwrap();
            parent = Some(c[0].id.clone());
        }
        let file = std::fs::read_to_string(dir.join("l.jsonl")).unwrap();
        assert_masked("session log", &file);
        assert_masked("memory", &serde_json::to_string(st.entries()).unwrap());
        let mem = st.entries().to_vec();
        drop(st);
        let back = JsonlStorage::open(dir.join("l.jsonl")).unwrap();
        assert_eq!(back.entries(), &mem[..]);
    }
}

/// The provider request: system prompt, every message role, tool arguments
/// and tool results, built from the masked log.
#[test]
fn the_provider_request_body_is_masked() {
    let dir = TestDir::new("tole-597-body");
    let mut st = JsonlStorage::create(&dir, "b", None).unwrap();
    let mut parent: Option<String> = None;
    for (kind, payload) in payloads() {
        let c = st
            .commit(Commit::new().entry(NewEntry {
                id: None,
                parent_id: parent.clone(),
                kind: EntryType::from_string(kind),
                payload,
                timestamp: 1,
            }))
            .unwrap();
        parent = Some(c[0].id.clone());
    }
    // Raw entries (an old, unredacted log) must still be masked on the way out.
    let raw: Vec<Entry> = st
        .entries()
        .iter()
        .zip(payloads())
        .map(|(e, (_, payload))| Entry {
            payload,
            ..e.clone()
        })
        .collect();
    let p = OpenAiProvider::new(OpenAiConfig::new(
        "https://x.example/v1",
        "m",
        "sk-test-0597-key",
    ));
    let body = serde_json::to_string(&p.request_body(&raw)).unwrap();
    assert_masked("request_body", &body);
}

#[test]
fn the_pinned_system_prompt_header_is_masked() {
    // The header is written with the process set (nothing known installed
    // here: only the detector can mask it).
    let dir = TestDir::new("tole-597-header");
    let st =
        JsonlStorage::create_with(&dir, "h", None, Some(&format!("you hold {}", gh()))).unwrap();
    assert!(!st.system_prompt().unwrap().contains(&gh()));
    assert_masked(
        "header",
        &std::fs::read_to_string(dir.join("h.jsonl")).unwrap(),
    );
}

struct Echo;
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        if input["fail"] == true {
            return Err(format!("denied for {}", aws()));
        }
        Ok(json!({"stdout": format!("found {}", gh())}))
    }
}

struct Seer(Vec<String>, Vec<ProviderOutput>);
impl Provider for Seer {
    fn complete(&mut self, t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.0.push(serde_json::to_string(t).unwrap());
        Ok(self.1.remove(0))
    }
    fn last_reasoning(&self) -> Option<String> {
        Some(format!("thinking about {}", gh()))
    }
}

#[derive(Default)]
struct Rec(Mutex<Vec<String>>);
impl tole_core::turn::TurnObserver for Rec {
    fn tool_started(&self, tool: &str, input: &Value) {
        self.0
            .lock()
            .unwrap()
            .push(format!("started {tool} {input}"));
    }
    fn tool_finished(&self, tool: &str, _ok: bool, preview: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("finished {tool} {preview}"));
    }
    fn reasoning(&self, text: &str) {
        self.0.lock().unwrap().push(format!("reasoning {text}"));
    }
}

/// A whole turn: the transcript the provider is handed next, the live
/// observer events (started, finished, error, reasoning) and the log.
#[test]
fn a_whole_turn_masks_the_transcript_the_live_events_and_the_log() {
    let dir = TestDir::new("tole-597-turn");
    let mut st = JsonlStorage::create(&dir, "t", None).unwrap();
    st.set_secrets(SecretSet::default());
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(Echo)).unwrap();
    let mut p = Seer(
        vec![],
        vec![
            ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({"arg": gh()}),
            },
            ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({"fail": true}),
            },
            ProviderOutput::Final {
                text: format!("done {}", aws()),
            },
        ],
    );
    let rec = Rec::default();
    tole_core::turn::run_turn_with_observer(
        &mut st,
        &mut p,
        &reg,
        &format!("use {}", gh()),
        &tole_core::cancel::CancelToken::default(),
        Some(&rec),
    )
    .unwrap();
    assert_masked(
        "log",
        &std::fs::read_to_string(dir.join("t.jsonl")).unwrap(),
    );
    for t in &p.0 {
        assert_masked("provider transcript", t);
    }
    let events = rec.0.lock().unwrap().join("\n");
    for kind in ["started echo", "finished echo", "reasoning"] {
        assert!(events.contains(kind), "no {kind:?}: {events}");
    }
    assert_masked("observer events", &events);
}
