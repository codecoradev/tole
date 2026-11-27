//! The plain-terminal approval prompt (#1041 follow-up): it shows the action
//! summary the way the TUI prompt does, falls back to the tool's own text
//! without one, neutralizes every printed value and keeps the y/N semantics.

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use tole_cli::approver::{InteractiveApprover, PromptFn};
use tole_cli::stdio_prompt::{ask, render};
use tole_cli::tools::WriteFileTool;
use tole_core::action_summary::{ActionEffect, ActionKind, ActionSummary};
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::tool::{Risk, ToolRegistry};

/// The real prompt code over a scripted stdin and a captured stdout.
struct Scripted {
    answer: &'static str,
    shown: Arc<Mutex<Vec<String>>>,
}

impl PromptFn for Scripted {
    fn prompt(&self, req: &ToolRequest<'_>) -> Verdict {
        let mut out = Vec::new();
        let verdict = ask(req, &mut Cursor::new(self.answer.as_bytes()), &mut out);
        self.shown
            .lock()
            .unwrap()
            .push(String::from_utf8(out).unwrap());
        verdict
    }
}

fn scripted(answer: &'static str) -> (Scripted, Arc<Mutex<Vec<String>>>) {
    let shown = Arc::new(Mutex::new(Vec::new()));
    (
        Scripted {
            answer,
            shown: shown.clone(),
        },
        shown,
    )
}

fn request<'a>(
    tool: &'a str,
    risk: Risk,
    input: &'a Value,
    description: &str,
    summary: Option<ActionSummary>,
) -> ToolRequest<'a> {
    ToolRequest {
        tool,
        risk,
        input,
        description: description.into(),
        target: None,
        summary,
    }
}

/// Everything unsafe is gone and the prompt is the same fixed shape: six line
/// breaks of its own, the question last without one.
fn assert_neutral(text: &str) {
    assert_eq!(text.matches('\n').count(), 6, "{text:?}");
    assert!(text.ends_with("allow? [y/N] "), "{text:?}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(!text.contains('\r'), "{text:?}");
    assert!(
        !text.chars().any(|c| ('\u{202a}'..='\u{202e}').contains(&c)),
        "{text:?}"
    );
}

#[test]
fn the_summary_is_shown_before_the_tools_own_text() {
    let dir = TestDir::new("tole-stdio-summary");
    let (prompt, shown) = scripted("y\n");
    let mut reg = ToolRegistry::with_approver(InteractiveApprover::new(prompt));
    reg.register(Box::new(WriteFileTool::new(dir.to_path_buf())))
        .unwrap();
    let input = json!({"path": "Weekly report.txt", "content": "Done\n"});
    assert_eq!(reg.decide("write_file", &input), Some(Verdict::Allow));

    let text = shown.lock().unwrap()[0].clone();
    let does = text
        .find("does:  create the file Weekly report.txt — Adds a new file: it did not exist when asked. Nothing is replaced.\n")
        .unwrap_or_else(|| panic!("{text}"));
    let what = text.find("what:  ").expect("the tool's own text stays");
    assert!(does < what, "{text}");
    assert!(text.contains("tool:  write_file [Write]\n"), "{text}");
    assert!(text.contains("input: {"), "{text}");
    assert_neutral(&text);
}

#[test]
fn a_request_without_a_summary_falls_back_to_the_tools_text() {
    let input = json!({"q": 1});
    let req = request("probe", Risk::Write, &input, "probe the thing", None);
    let text = render(&req);
    assert!(!text.contains("does:"), "{text}");
    assert!(text.contains("\nwhat:  probe the thing\n"), "{text}");
    assert!(text.contains("tool:  probe [Write]\n"), "{text}");
}

#[test]
fn a_tool_without_its_own_summary_says_the_effect_is_unknown() {
    let input = json!({"q": 1});
    let summary = ActionSummary::unknown_with_description("probe", &input, "probe the thing");
    let req = request(
        "probe",
        Risk::Write,
        &input,
        "probe the thing",
        Some(summary),
    );
    let text = render(&req);
    assert!(
        text.contains("does:  use the tool probe — Effect unknown: read the details.\n"),
        "{text}"
    );
    assert!(text.contains("\nwhat:  probe the thing\n"), "{text}");
}

#[test]
fn control_and_bidi_characters_in_a_path_are_shown_as_escapes() {
    let dir = TestDir::new("tole-stdio-sanitize");
    let (prompt, shown) = scripted("n\n");
    let mut reg = ToolRegistry::with_approver(InteractiveApprover::new(prompt));
    reg.register(Box::new(WriteFileTool::new(dir.to_path_buf())))
        .unwrap();
    let input = json!({"path": "a\u{1b}[2J\nb\u{202e}txt\r.md", "content": "x"});
    assert_eq!(reg.decide("write_file", &input), Some(Verdict::Deny));

    let text = shown.lock().unwrap()[0].clone();
    assert_neutral(&text);
    assert!(
        text.contains("\\u{1b}[2J\\u{a}b\\u{202e}txt\\u{d}.md"),
        "{text}"
    );
}

#[test]
fn a_request_built_by_hand_is_neutralized_too() {
    // No registry in between: the prompt does not trust the text it is given.
    let input = json!({"path": "p\u{202e}q"});
    let summary = ActionSummary::new(
        ActionKind::DeleteFile,
        "a\u{1b}b\nc",
        ActionEffect::CannotBeUndone,
        true,
    );
    let mut req = request(
        "dele\u{1b}te",
        Risk::Destructive,
        &input,
        "remove\u{1b}[2J\r\nit",
        Some(summary),
    );
    req.summary.as_mut().unwrap().target = "a\u{1b}b\nc".into();
    let text = render(&req);
    assert_neutral(&text);
    assert!(
        text.contains("tool:  dele\\u{1b}te [Destructive]\n"),
        "{text}"
    );
    assert!(text.contains("delete the file a\\u{1b}b\\u{a}c"), "{text}");
    assert!(text.contains("remove\\u{1b}[2J\\u{d}\\u{a}it"), "{text}");
    assert!(text.contains("p\\u{202e}q"), "{text}");
}

#[test]
fn a_destructive_prompt_offers_no_allow_always_and_ignores_the_allowlist() {
    let input = json!({"path": "a.txt"});
    let summary = ActionSummary::new(
        ActionKind::DeleteFile,
        "a.txt",
        ActionEffect::CannotBeUndone,
        true,
    );
    let make = || {
        request(
            "delete_file",
            Risk::Destructive,
            &input,
            "delete a.txt",
            Some(summary.clone()),
        )
    };
    let text = render(&make());
    assert!(
        text.contains("tool:  delete_file [Destructive]\n"),
        "{text}"
    );
    assert!(
        text.contains("does:  delete the file a.txt — Cannot be undone.\n"),
        "{text}"
    );
    assert!(text.ends_with("allow? [y/N] "), "{text}");
    assert!(!text.to_lowercase().contains("always"), "{text}");

    // The allowlist and --yes never reach it: the human is asked, and only a
    // plain y / yes allows.
    for (answer, want) in [("y\n", Verdict::Allow), ("always\n", Verdict::Deny)] {
        let (prompt, shown) = scripted(answer);
        let a = InteractiveApprover::new(prompt)
            .with_allow_patterns(vec!["*".into()])
            .with_auto_write(true);
        assert_eq!(a.decide(&make()), want, "{answer:?}");
        assert_eq!(shown.lock().unwrap().len(), 1, "it asked");
    }
}

#[test]
fn the_answer_keys_and_the_fail_closed_default_are_unchanged() {
    let input = Value::Null;
    let req = request("t", Risk::Write, &input, "d", None);
    let verdict = |stdin: &str| {
        ask(
            &req,
            &mut Cursor::new(stdin.as_bytes().to_vec()),
            &mut Vec::new(),
        )
    };
    assert_eq!(verdict("y\n"), Verdict::Allow);
    assert_eq!(verdict("YES\n"), Verdict::Allow);
    assert_eq!(verdict("  y  \n"), Verdict::Allow);
    assert_eq!(verdict("n\n"), Verdict::Deny);
    assert_eq!(verdict("\n"), Verdict::Deny);
    assert_eq!(verdict("maybe\n"), Verdict::Deny);
    // No terminal: stdin is at end of file, nothing can answer, so Deny.
    assert_eq!(verdict(""), Verdict::Deny);
    assert!(InteractiveApprover::stdio().interactive());
}

#[test]
fn a_long_input_keeps_its_head_and_tail() {
    let render_of = |v: Value| render(&request("t", Risk::Write, &v, "d", None));
    let long = render_of(Value::String("x".repeat(1998)));
    assert!(long.contains("… (+1200 more chars)…"), "{long}");
    // Multi-byte text is cut on character boundaries.
    let multibyte = render_of(Value::String("é".repeat(1000)));
    assert!(multibyte.contains("… (+202 more chars)…"), "{multibyte}");
    // A short security-relevant value stays fully visible.
    let edge = "p".repeat(797);
    let short = render_of(Value::String(edge.clone()));
    assert!(short.contains(&format!("input: \"{edge}\"\n")), "{short}");
}
