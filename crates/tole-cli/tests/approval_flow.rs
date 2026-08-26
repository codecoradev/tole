//! E6 / W4 checkpoint — full-loop proof of the approval flow, Tier A:
//! write request → (scripted) y/N prompt → denied recorded / allowed
//! executed, over the real turn loop and JSONL storage.

use serde_json::json;
use std::path::{Path, PathBuf};

use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::mock::MockProvider;
use tole_core::provider::ProviderOutput;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{run_turn, TurnOutcome};

use approver::InteractiveApprover;

// The bin's modules are not importable from an integration test, so the
// prompt + tool under test are re-declared here in the exact same shape
// as `src/approver.rs` / `src/tools.rs` (they are host-side, small, and
// unit-tested there; this file exercises the *loop* semantics).
mod approver {
    use tole_core::approval::{glob_match, Approver, ToolRequest, Verdict};
    use tole_core::tool::Risk;

    pub trait PromptFn: Send + Sync {
        fn prompt(&self, req: &ToolRequest<'_>) -> Verdict;
    }

    /// Prompt that always answers with the canned verdict.
    pub struct CannedPrompt(pub Verdict);

    impl PromptFn for CannedPrompt {
        fn prompt(&self, _req: &ToolRequest<'_>) -> Verdict {
            self.0
        }
    }

    pub struct InteractiveApprover<P: PromptFn> {
        patterns: Vec<String>,
        auto_write: bool,
        prompter: P,
    }

    impl InteractiveApprover<CannedPrompt> {
        pub fn answering(v: Verdict) -> Self {
            Self::new(CannedPrompt(v))
        }
    }

    impl<P: PromptFn> InteractiveApprover<P> {
        pub fn new(prompter: P) -> Self {
            Self {
                patterns: Vec::new(),
                auto_write: false,
                prompter,
            }
        }

        pub fn with_allow_patterns(mut self, patterns: Vec<String>) -> Self {
            self.patterns = patterns;
            self
        }

        pub fn with_auto_write(mut self, yes: bool) -> Self {
            self.auto_write = yes;
            self
        }
    }

    impl<P: PromptFn> Approver for InteractiveApprover<P> {
        fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
            if req.risk == Risk::Destructive {
                return self.prompter.prompt(req);
            }
            if self.auto_write || self.patterns.iter().any(|p| glob_match(p, req.tool)) {
                return Verdict::Allow;
            }
            self.prompter.prompt(req)
        }

        fn interactive(&self) -> bool {
            true
        }
    }
}

mod tools {
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use tole_core::tool::{Risk, Tool};

    pub struct WriteFileTool {
        pub root: PathBuf,
    }

    impl Tool for WriteFileTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn risk(&self) -> Risk {
            Risk::Write
        }
        fn describe(&self, input: &Value) -> String {
            format!(
                "write file {}",
                input.get("path").and_then(Value::as_str).unwrap_or("?")
            )
        }
        fn execute(&self, input: Value) -> Result<Value, String> {
            let path = input
                .get("path")
                .and_then(Value::as_str)
                .ok_or("missing path")?;
            let content = input
                .get("content")
                .and_then(Value::as_str)
                .ok_or("missing content")?;
            std::fs::write(self.root.join(path), content).map_err(|e| e.to_string())?;
            Ok(json!({ "path": path, "bytes": content.len() }))
        }
    }

    pub struct Bomb;
    impl Tool for Bomb {
        fn name(&self) -> &str {
            "bomb"
        }
        fn risk(&self) -> Risk {
            Risk::Destructive
        }
        fn execute(&self, _input: Value) -> Result<Value, String> {
            Ok(json!({ "boom": true }))
        }
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-cli-e2e-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn registry_with(dir: &Path, approver: impl Approver + 'static, with_bomb: bool) -> ToolRegistry {
    let mut reg = ToolRegistry::with_approver(approver);
    reg.register(Box::new(tools::WriteFileTool {
        root: dir.to_path_buf(),
    }))
    .unwrap();
    if with_bomb {
        reg.register(Box::new(tools::Bomb)).unwrap();
    }
    reg
}

/// The W4 demo, denied leg: write request → prompt (N) → tool NOT
/// executed, `approval required` durably recorded, turn aborted.
#[test]
fn write_request_prompted_and_denied_is_recorded() {
    let dir = tmpdir("denied");
    let mut s = JsonlStorage::create(&dir, "denied", None).unwrap();
    let reg = registry_with(&dir, InteractiveApprover::answering(Verdict::Deny), false);
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "write_file".into(),
            input: json!({ "path": "out.txt", "content": "x" }),
        },
        ProviderOutput::Final {
            text: "unreachable".into(),
        },
    ]);
    match run_turn(&mut s, &mut p, &reg, "please write out.txt").unwrap() {
        TurnOutcome::ApprovalRequired { name } => assert_eq!(name, "write_file"),
        other => panic!("expected ApprovalRequired, got {other:?}"),
    }
    // Durable record: an ERROR entry under the user message.
    let errs: Vec<_> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error" && e.parent_id.is_some())
        .collect();
    assert_eq!(errs.len(), 1, "exactly one durable denial record");
    assert_eq!(errs[0].payload["error"], json!("approval required"));
    // The world was NOT touched.
    assert!(
        !dir.join("out.txt").exists(),
        "denied write must not execute"
    );
}

/// The W4 demo, allowed leg: y → file written, loop continues to Final.
#[test]
fn write_request_prompted_and_allowed_executes() {
    let dir = tmpdir("allowed");
    let mut s = JsonlStorage::create(&dir, "allowed", None).unwrap();
    let reg = registry_with(&dir, InteractiveApprover::answering(Verdict::Allow), false);
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "write_file".into(),
            input: json!({ "path": "out.txt", "content": "hello" }),
        },
        ProviderOutput::Final {
            text: "written!".into(),
        },
    ]);
    match run_turn(&mut s, &mut p, &reg, "please write out.txt").unwrap() {
        TurnOutcome::Final { text } => assert_eq!(text, "written!"),
        other => panic!("expected Final, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "hello"
    );
}

/// `--allow write_*` semantics: no prompt needed, write executes.
#[test]
fn allow_pattern_skips_prompt_entirely() {
    let dir = tmpdir("pattern");
    let mut s = JsonlStorage::create(&dir, "pattern", None).unwrap();
    let reg = registry_with(
        &dir,
        InteractiveApprover::answering(Verdict::Deny) // human says N — must not be asked
            .with_allow_patterns(vec!["write_*".into()]),
        false,
    );
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "write_file".into(),
            input: json!({ "path": "out.txt", "content": "auto" }),
        },
        ProviderOutput::Final {
            text: "done".into(),
        },
    ]);
    assert!(matches!(
        run_turn(&mut s, &mut p, &reg, "write").unwrap(),
        TurnOutcome::Final { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "auto"
    );
}

/// Destructive tools register behind an interactive approver, and even
/// `--yes` + `--allow '*'` cannot skip their prompt (PRD).
#[test]
fn destructive_always_prompts_despite_allow_all() {
    let dir = tmpdir("bomb");
    let mut s = JsonlStorage::create(&dir, "bomb", None).unwrap();
    let reg = registry_with(
        &dir,
        InteractiveApprover::answering(Verdict::Deny)
            .with_allow_patterns(vec!["*".into()])
            .with_auto_write(true),
        true,
    );
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "bomb".into(),
            input: json!({ "n": 1 }),
        },
        ProviderOutput::Final {
            text: "unreachable".into(),
        },
    ]);
    match run_turn(&mut s, &mut p, &reg, "detonate").unwrap() {
        TurnOutcome::ApprovalRequired { name } => assert_eq!(name, "bomb"),
        other => panic!("expected ApprovalRequired, got {other:?}"),
    }
    // Prompt WAS shown (the CannedPrompt denies), i.e. allow-all did
    // not bypass the human: proven by the outcome + durable record.
    let errs: Vec<_> = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "error" && e.parent_id.is_some())
        .collect();
    assert_eq!(errs.len(), 1);
}

/// Registry structural rule: Destructive behind a non-interactive
/// approver is refused at wiring time.
#[test]
fn registry_refuses_destructive_behind_allowlist() {
    use tole_core::approval::AllowlistApprover;
    let mut reg = ToolRegistry::with_approver(AllowlistApprover::default());
    let err = reg.register(Box::new(tools::Bomb)).unwrap_err();
    assert!(err.contains("Destructive"), "got: {err}");
}

/// The request the prompt renders carries everything a human needs.
#[test]
fn tool_request_carries_prompt_fields() {
    let a = InteractiveApprover::answering(Verdict::Allow);
    let input = json!({ "path": "notes.md", "content": "hi" });
    // Build a request the way the registry does and inspect it.
    let t = tools::WriteFileTool {
        root: PathBuf::from("."),
    };
    let req = ToolRequest {
        tool: t.name(),
        risk: t.risk(),
        input: &input,
        description: t.describe(&input),
    };
    assert_eq!(req.tool, "write_file");
    assert_eq!(req.risk, Risk::Write);
    assert_eq!(req.description, "write file notes.md");
    assert_eq!(a.decide(&req), Verdict::Allow);
}
