//! #677: the live observer events (serve SSE `tool_started` /
//! `tool_finished`, ACP `tool_call_update`, the TUI) carried the RAW tool
//! input, output, error and reasoning while the session log of the same
//! turn was redacted (#596). The turn loop now redacts every observer event
//! with the session's own secret set, before the preview is cut.

use serde_json::{json, Value};
use std::sync::Mutex;
use tole_core::entry::Entry;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::secrets::SecretSet;
use tole_core::storage::JsonlStorage;

const GH: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB";

fn set() -> SecretSet {
    SecretSet::collect([("GITHUB_TOKEN".to_string(), GH.to_string())], [])
}

/// Returns the secret in its output, or fails with it in the error.
struct LeakyTool {
    fail: bool,
}

impl tole_core::tool::Tool for LeakyTool {
    fn name(&self) -> &str {
        if self.fail {
            "leaky_err"
        } else {
            "leaky"
        }
    }
    fn risk(&self) -> tole_core::tool::Risk {
        tole_core::tool::Risk::ReadOnly
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        if self.fail {
            Err(format!("auth failed for {GH}"))
        } else {
            // The secret straddles the 200-character preview cut: a cut
            // before redaction would leave a partial, unmatchable value.
            Ok(json!({ "stdout": format!("{}{GH}", "x".repeat(180)) }))
        }
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

/// A scripted provider whose every step reports reasoning holding the secret.
struct Reasoning(tole_core::mock::MockProvider);

impl Provider for Reasoning {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.0.complete(transcript)
    }
    fn last_reasoning(&self) -> Option<String> {
        Some(format!("the token is {GH}"))
    }
}

#[test]
fn every_live_observer_event_is_redacted_like_the_log() {
    let dir = std::env::temp_dir().join(format!("tole-live-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut st = JsonlStorage::create(&dir, "s", None).unwrap();
    st.set_secrets(set());
    let mut p = Reasoning(tole_core::mock::MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "leaky".into(),
            input: json!({ "token": GH }),
        },
        ProviderOutput::ToolCall {
            tool: "leaky_err".into(),
            input: json!({}),
        },
        ProviderOutput::Final { text: "ok".into() },
    ]));
    let mut reg = tole_core::tool::ToolRegistry::new();
    reg.register(Box::new(LeakyTool { fail: false })).unwrap();
    reg.register(Box::new(LeakyTool { fail: true })).unwrap();
    let rec = Rec::default();
    tole_core::turn::run_turn_with_observer(
        &mut st,
        &mut p,
        &reg,
        "go",
        &tole_core::cancel::CancelToken::default(),
        Some(&rec),
    )
    .unwrap();

    let log = std::fs::read_to_string(dir.join("s.jsonl")).unwrap();
    assert!(!log.contains(GH), "the durable log is redacted (#596)");
    let events = rec.0.lock().unwrap().join("\n");
    for kind in [
        "started leaky",
        "finished leaky ",
        "finished leaky_err",
        "reasoning",
    ] {
        assert!(events.contains(kind), "no {kind:?} event: {events}");
    }
    assert!(
        !events.contains(GH),
        "a live event leaks the secret: {events}"
    );
    // Not even the part before the cut.
    assert!(
        !events.contains(&GH[..6]),
        "a partial secret leaks: {events}"
    );
    assert!(events.contains("«GITHUB_TOKEN»"), "{events}");
    let _ = std::fs::remove_dir_all(&dir);
}
