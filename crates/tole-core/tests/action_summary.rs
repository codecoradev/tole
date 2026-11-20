//! The plain-language approval summary (#1041), core part: a tool without a
//! summary of its own is neutral and keeps its own description, with the raw
//! input redacted; the tier is never changed by it.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use test_dir::TestDir;
use tole_core::action_summary::{ActionEffect, ActionKind, ActionSummary};
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::tool::{Risk, Tool, ToolRegistry};

/// Denies everything and records the whole request it was asked about.
struct Spy(Arc<Mutex<Vec<(Risk, String, ActionSummary)>>>);

impl Approver for Spy {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        self.0
            .lock()
            .unwrap()
            .push((req.risk, req.description.clone(), req.action_summary()));
        Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

fn summary_of(tool: Box<dyn Tool>, input: Value) -> (Risk, ActionSummary) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::with_approver(Spy(seen.clone()));
    let name = tool.name().to_string();
    reg.register(tool).unwrap();
    reg.decide(&name, &input).unwrap();
    let (risk, _, s) = seen.lock().unwrap().remove(0);
    (risk, s)
}

fn workspace() -> TestDir {
    let dir = TestDir::new("tole-summary");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}\n").unwrap();
    dir
}

struct Plain(&'static str, Risk);
impl Tool for Plain {
    fn name(&self) -> &str {
        self.0
    }
    fn risk(&self) -> Risk {
        self.1
    }
    fn describe(&self, _: &Value) -> String {
        "completely safe, nothing to see".into()
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }
}

/// A tool with no summary of its own is "use the tool <name>" with the raw
/// input, at whatever tier it has; the tier is read from the tool, not the
/// summary.
#[test]
fn an_unknown_tool_is_neutral_with_raw_details_and_keeps_its_tier() {
    for risk in [Risk::Write, Risk::Destructive] {
        let (got, s) = summary_of(Box::new(Plain("frobnicate", risk)), json!({"mode": "fast"}));
        assert_eq!(got, risk);
        assert_eq!(s.kind, ActionKind::Unknown);
        assert_eq!(s.effect, ActionEffect::Unknown);
        assert_eq!(s.headline(), "use the tool frobnicate");
        // The tool's own description first, then the raw input.
        assert_eq!(
            s.preview,
            [
                "the tool's own description: completely safe, nothing to see",
                r#"{"mode":"fast"}"#
            ]
        );
        // The reassuring words are the tool's own text, shown as data; the
        // sentence code builds says nothing reassuring.
        assert!(!s.sentence().contains("safe"), "{}", s.sentence());
    }
}

/// A tool without a summary of its own keeps what its `describe` said (the
/// resolved destination, the exact argv): the summary never shows less.
/// Tools with their own summary are covered by their own tests above (the
/// documented equivalent: kind, target, effect and a preview).
#[cfg(feature = "shell-tools")]
#[test]
fn a_tool_without_a_summary_keeps_its_describe_text() {
    use tole_core::children::handoff::{HandoffSubmitTool, Role};
    let dir = workspace();
    let root = dir.path().to_path_buf();
    let cases: Vec<(Box<dyn Tool>, Value)> = vec![
        // The model omitted `repo`: the destination is only in `describe`.
        (
            Box::new(tole_core::gh::GhTool::new("acme/widgets")),
            json!({"op": "pr_create", "title": "t", "body": "b", "head": "feat", "base": "main"}),
        ),
        (
            Box::new(tole_core::gh::GhTool::new("acme/widgets")),
            json!({"op": "issue_comment", "number": 7, "body": "hi"}),
        ),
        (
            Box::new(tole_core::gh::GhTool::new("acme/widgets")),
            json!({"op": "issue_create", "title": "t", "body": "b"}),
        ),
        (
            Box::new(tole_core::git::GitTool::new()),
            json!({"op": "status"}),
        ),
        (
            Box::new(tole_core::agents::AgentPollTool::new(root.clone())),
            json!({"agent": "a1"}),
        ),
        (
            Box::new(tole_core::todo::TodoWriteTool::new(
                tole_core::todo::TodoState::new(),
            )),
            json!({"todos": []}),
        ),
        (
            Box::new(HandoffSubmitTool::new(root.join("h.json"), Role::Explore)),
            json!({"summary": "done"}),
        ),
    ];
    for (tool, input) in cases {
        let name = tool.name().to_string();
        let described = tool.describe(&input);
        let own = tool.action_summary(&input).is_some();
        let (risk, s) = summary_of(tool, input);
        assert_ne!(risk, Risk::ReadOnly, "{name}");
        if own {
            continue;
        }
        assert_eq!(s.kind, ActionKind::Unknown, "{name}");
        assert_eq!(s.headline(), format!("use the tool {name}"), "{name}");
        assert_eq!(
            s.preview[0],
            format!("the tool's own description: {described}"),
            "{name}"
        );
    }
    // The gh destination the model never sent is in the summary.
    let (_, s) = summary_of(
        Box::new(tole_core::gh::GhTool::new("acme/widgets")),
        json!({"op": "pr_create", "title": "t", "body": "b", "head": "feat", "base": "main"}),
    );
    assert!(
        s.preview[0].contains("--repo acme/widgets"),
        "{:?}",
        s.preview
    );
}
