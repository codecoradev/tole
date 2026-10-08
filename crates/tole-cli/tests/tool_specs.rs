//! Issue #320: provider-facing tool descriptions must come from the
//! static `Tool::summary()`, never from `describe(Null)` (the per-call
//! approval line, which validates input and returns error strings such
//! as "git: 'op' must be one of ..." for a `Null` input).

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tole_cli::session_host::UpdatePlanTool;
use tole_cli::session_tools::SessionToolState;
use tole_cli::tools::WriteFileTool;
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::tool::{Tool, ToolRegistry};

struct DenyAllInteractive;

impl Approver for DenyAllInteractive {
    fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
        Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// Every in-repo tool (all shell-tools), built directly so the test does
/// not depend on which binaries (`uteke`, `cora`, ...) the host has.
fn all_tools() -> Vec<Box<dyn Tool>> {
    let root = PathBuf::from(".");
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(tole_core::git::GitTool::new()),
        Box::new(tole_core::gh::GhTool::new("owner/name")),
        Box::new(tole_core::gitea::GiteaTool::new(
            "https://gitea.example",
            "tok",
            "owner/name",
        )),
        Box::new(tole_core::systemone::SystemOneTool::new(
            "https://systemone.example/v1/systemone",
            "key",
        )),
        Box::new(tole_core::cora_search::CoraSearchTool::new()),
        Box::new(tole_core::uteke::UtekeRecallTool::new()),
        Box::new(tole_core::uteke::UtekeDocumentTool::new(None)),
        Box::new(tole_core::verify_package::VerifyPackageTool::new()),
        Box::new(tole_core::run_command::RunCommandTool::new(root.clone())),
        Box::new(tole_core::jobs::JobStartTool::new(root.clone())),
        Box::new(tole_core::jobs::JobPollTool::new(root.clone())),
        Box::new(tole_core::agents::AgentStartTool::new("tole", root.clone())),
        Box::new(tole_core::agents::AgentPollTool::new(root.clone())),
        Box::new(tole_core::read_file::ReadFileTool::new(root.clone())),
        Box::new(tole_core::file_tools::EditFileTool::new(root.clone())),
        Box::new(tole_core::file_tools::DeleteFileTool::new(root.clone())),
        Box::new(tole_core::web::WebFetchTool),
        Box::new(tole_core::skills::LoadSkillTool::new(None)),
        Box::new(WriteFileTool::new(root.clone())),
        Box::new(UpdatePlanTool::new(Arc::new(|_| {}))),
    ];
    let todo = tole_core::todo::TodoState::new();
    tools.push(Box::new(tole_core::todo::TodoWriteTool::new(todo.clone())));
    tools.push(Box::new(tole_core::todo::TodoReadTool::new(todo)));
    if let Some(t) = tole_core::web::WebSearchTool::from_env() {
        tools.push(Box::new(t));
    }
    let state = SessionToolState::new(vec![], false, None, root, None, vec![]);
    tools.extend(state.tools());
    tools
}

fn full_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::with_approver(DenyAllInteractive);
    for t in all_tools() {
        let name = t.name().to_string();
        reg.register(t)
            .unwrap_or_else(|e| panic!("registering {name}: {e}"));
    }
    reg
}

fn spec_description<'a>(specs: &'a [Value], name: &str) -> &'a str {
    specs
        .iter()
        .find(|s| s["function"]["name"] == name)
        .unwrap_or_else(|| panic!("spec for {name} missing"))["function"]["description"]
        .as_str()
        .expect("description is a string")
}

#[test]
fn specs_never_advertise_describe_null_error_strings() {
    let reg = full_registry();
    let specs = reg.specs();
    assert!(specs.len() >= 20, "expected the full registry");
    for t in all_tools() {
        let desc = spec_description(&specs, t.name());
        assert!(
            !desc.trim().is_empty(),
            "{}: empty spec description",
            t.name()
        );
        // The approval line for a Null input is where validation errors
        // surface; none of it may leak into the provider-facing text.
        let approval_null = t.describe(&Value::Null);
        assert_ne!(
            desc,
            approval_null,
            "{}: spec description must not be describe(Null) ({approval_null:?})",
            t.name()
        );
        for marker in ["must be one of", "missing '", "<missing", "unparsable"] {
            assert!(
                !desc.contains(marker),
                "{}: spec description looks like an error string: {desc:?}",
                t.name()
            );
        }
    }
}

#[test]
fn git_and_systemone_summaries_are_static() {
    let reg = full_registry();
    let specs = reg.specs();
    assert_eq!(
        spec_description(&specs, "git"),
        "Light version control in the workspace: status, diff, add, commit. No push; a human pushes."
    );
    assert_eq!(
        spec_description(&specs, "systemone_decide"),
        "Typed decisions (choice, score or noul) with calibrated confidence from a System One backend. Pass filtered state and literal criteria."
    );
    // describe() is untouched: still the approval/validation line.
    let git = tole_core::git::GitTool::new();
    assert!(git.describe(&Value::Null).contains("'op' must be one of"));
    assert!(git.describe(&json!({"op": "status"})).contains("status"));
}
