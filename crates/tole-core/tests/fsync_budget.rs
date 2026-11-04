//! #483 measurement harness: counts the real `sync_data` calls
//! `JsonlStorage` issues per MODEL step through the real turn loop, and
//! asserts the budget. Durability is the invariant: the file on disk must
//! hold the full step whenever a step completes (checked by re-opening it).

use serde_json::{json, Value};
use tole_core::approval::AllowlistApprover;
use tole_core::entry::Entry;
use tole_core::mock::MockProvider;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{run_turn, TurnOutcome};

/// Reports usage like a real provider, so the per-step usage record is
/// part of the measurement (the mock alone reports none).
struct WithUsage(MockProvider);
impl Provider for WithUsage {
    fn complete(&mut self, t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.0.complete(t)
    }
    fn last_usage(&self) -> Option<Value> {
        Some(json!({ "prompt_tokens": 1, "completion_tokens": 1 }))
    }
}

struct Echo(Risk);
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn risk(&self) -> Risk {
        self.0
    }
    fn describe(&self, _input: &Value) -> String {
        "echo".into()
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }
}

const TOOL_STEPS: usize = 10;

fn run(tag: &str, risk: Risk) -> (f64, std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("tole-fsync-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = JsonlStorage::create(&dir, tag, None).unwrap();
    let mut reg = ToolRegistry::with_approver(AllowlistApprover::allow_only(vec!["echo".into()]));
    reg.register(Box::new(Echo(risk))).unwrap();
    let mut script: Vec<ProviderOutput> = (0..TOOL_STEPS)
        .map(|i| ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({ "n": i }),
        })
        .collect();
    script.push(ProviderOutput::Final {
        text: "done".into(),
    });
    let mut p = WithUsage(MockProvider::scripted(script));
    let before = s.sync_count();
    let out = run_turn(&mut s, &mut p, &reg, "go").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));
    let per_step = (s.sync_count() - before) as f64 / (TOOL_STEPS + 1) as f64;
    // Durability: the on-disk file alone reflects the whole turn.
    let disk = std::fs::read_to_string(dir.join(format!("{tag}.jsonl"))).unwrap();
    let reopened = JsonlStorage::open_read_only(dir.join(format!("{tag}.jsonl"))).unwrap();
    assert_eq!(reopened.entries().len(), s.entries().len());
    assert_eq!(reopened.state(), s.state());
    assert_eq!(reopened.usages().len(), TOOL_STEPS + 1);
    (per_step, dir, disk)
}

#[test]
fn read_only_step_syncs_at_most_twice() {
    let (per_step, dir, _) = run("ro", Risk::ReadOnly);
    eprintln!("read-only: {per_step:.2} syncs/model step");
    assert!(per_step <= 2.0, "{per_step:.2} syncs/step (budget 2.0)");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn write_step_syncs_at_most_twice() {
    let (per_step, dir, _) = run("wr", Risk::Write);
    eprintln!("write: {per_step:.2} syncs/model step");
    assert!(per_step <= 2.0, "{per_step:.2} syncs/step (budget 2.0)");
    let _ = std::fs::remove_dir_all(dir);
}
