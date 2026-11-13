//! #592: the taint of a session survives a resume, because the registry
//! folds its tracker over the entries already in the log on the first turn.

use serde_json::{json, Value};
use tole_core::entry::Entry;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::skill_draft::TaintTracker;
use tole_core::storage::JsonlStorage;
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::run_turn;

struct Named(&'static str);
impl Tool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }
}

struct Script(Vec<ProviderOutput>, usize);
impl Provider for Script {
    fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        let out = self.0.get(self.1).cloned();
        self.1 += 1;
        out.ok_or_else(|| ProviderError("exhausted".into()))
    }
}

fn registry(tracker: &TaintTracker) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.set_taint_tracker(tracker.clone());
    for n in ["web_fetch", "read_file"] {
        r.register(Box::new(Named(n))).unwrap();
    }
    r
}

fn script(tool: &str) -> Script {
    Script(
        vec![
            ProviderOutput::ToolCall {
                tool: tool.into(),
                input: json!({}),
            },
            ProviderOutput::Final { text: "ok".into() },
        ],
        0,
    )
}

#[test]
fn a_resumed_session_keeps_the_taint_of_what_it_fetched_before() {
    let dir = std::env::temp_dir().join(format!("tole-taint-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Process one fetches from outside.
    let first = TaintTracker::default();
    let mut s = JsonlStorage::create(&dir, "tr", None).unwrap();
    run_turn(&mut s, &mut script("web_fetch"), &registry(&first), "go").unwrap();
    assert_eq!(first.sources(), ["web_fetch"]);
    drop(s);

    // Process two reopens the log with a fresh tracker and only reads a file.
    let second = TaintTracker::default();
    let mut s2 = JsonlStorage::open(dir.join("tr.jsonl")).unwrap();
    run_turn(
        &mut s2,
        &mut script("read_file"),
        &registry(&second),
        "more",
    )
    .unwrap();
    assert_eq!(second.sources(), ["web_fetch"], "not clean after a resume");

    // A session that never fetched stays clean across a resume.
    let clean_dir = dir.join("clean");
    std::fs::create_dir_all(&clean_dir).unwrap();
    let mut c = JsonlStorage::create(&clean_dir, "cl", None).unwrap();
    run_turn(
        &mut c,
        &mut script("read_file"),
        &registry(&TaintTracker::default()),
        "go",
    )
    .unwrap();
    drop(c);
    let third = TaintTracker::default();
    let mut c2 = JsonlStorage::open(clean_dir.join("cl.jsonl")).unwrap();
    run_turn(&mut c2, &mut script("read_file"), &registry(&third), "more").unwrap();
    assert!(third.sources().is_empty());
}

#[test]
fn one_registry_reused_across_sessions_tracks_the_session_in_front() {
    let dir = std::env::temp_dir().join(format!("tole-taint-swap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tracker = TaintTracker::default();
    tracker.mark("webhook");
    let reg = registry(&tracker);

    let mut dirty = JsonlStorage::create(&dir, "dirty", None).unwrap();
    run_turn(&mut dirty, &mut script("web_fetch"), &reg, "go").unwrap();
    assert_eq!(tracker.sources(), ["web_fetch", "webhook"]);

    // A new session in the same registry starts without the old taint, but
    // keeps what the host marked.
    let mut clean = JsonlStorage::create(&dir, "clean", None).unwrap();
    run_turn(&mut clean, &mut script("read_file"), &reg, "go").unwrap();
    assert_eq!(tracker.sources(), ["webhook"]);

    // Going back to the tainted session restores its taint.
    run_turn(&mut dirty, &mut script("read_file"), &reg, "again").unwrap();
    assert_eq!(tracker.sources(), ["web_fetch", "webhook"]);
}
