//! The shared approval summary (#1041) on the cli side: `write_file`, the
//! serve queue row (`GET /approvals`, the `approval_required` stop) and the
//! TUI prompt all show the same summary for the same request, and rows
//! without it still work.

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::sync::Arc;

use tole_cli::approvals::{approval_json, approval_required_body, ApprovalQueue, QueueApprover};
use tole_cli::tools::WriteFileTool;
use tole_core::action_summary::{
    ActionEffect, ActionKind, ActionSummary, MAX_LINE_CHARS, MAX_PREVIEW_LINES,
};
use tole_core::approval::{ToolRequest, Verdict};
use tole_core::tool::{Risk, Tool, ToolRegistry};

fn summary_of(root: &std::path::Path, input: Value) -> ActionSummary {
    let t = WriteFileTool::new(root.to_path_buf());
    let req = ToolRequest {
        tool: t.name(),
        risk: t.risk(),
        input: &input,
        description: t.describe(&input),
        target: t.approval_target(&input),
        summary: t.action_summary(&input),
    };
    assert_eq!(req.risk, Risk::Write);
    req.action_summary()
}

#[test]
fn write_file_adds_a_new_file_and_replaces_an_existing_one() {
    let dir = TestDir::new("tole-summary-write");
    std::fs::create_dir_all(dir.join("Reports")).unwrap();
    let s = summary_of(
        &dir,
        json!({"path": "Reports/Weekly report.txt", "content": "Done\nNext\n"}),
    );
    assert_eq!(s.kind, ActionKind::CreateFile);
    assert_eq!(s.target, "Reports/Weekly report.txt");
    assert_eq!(s.effect, ActionEffect::Adds);
    assert!(s.inside_workspace);
    assert_eq!(s.preview, ["+ Done", "+ Next"]);
    assert_eq!(
        s.sentence(),
        "create the file Reports/Weekly report.txt — Adds a new file: it did not exist when asked. Nothing is replaced."
    );

    std::fs::write(dir.join("a.txt"), "old").unwrap();
    let s = summary_of(&dir, json!({"path": "a.txt", "content": "new"}));
    assert_eq!(s.kind, ActionKind::ReplaceFile);
    assert_eq!(s.effect, ActionEffect::ReplacesNoBackup);
    assert!(s.inside_workspace);
}

#[test]
fn write_file_preview_is_bounded_and_a_jail_escape_is_flagged() {
    let dir = TestDir::new("tole-summary-bound");
    let content = format!("{}\n", "x".repeat(5000)).repeat(100);
    let s = summary_of(&dir, json!({"path": "big.txt", "content": content}));
    assert_eq!(s.preview.len(), MAX_PREVIEW_LINES);
    assert!(s
        .preview
        .iter()
        .all(|l| l.chars().count() <= MAX_LINE_CHARS));
    assert!(s.preview_truncated);
    assert!(s.preview.last().unwrap().contains("more lines"));
    assert!(
        s.preview[0].contains("more characters"),
        "head and tail kept"
    );

    // A path the tool refuses is not worded as "creates a new file".
    let s = summary_of(&dir, json!({"path": "../escape.txt", "content": "x"}));
    assert_eq!(s.kind, ActionKind::Unknown);
    assert_eq!(s.effect, ActionEffect::Unknown);
    assert!(!s.inside_workspace);
    assert!(
        s.preview[0].starts_with("refused: the path is outside the working folder")
            && s.preview[0].contains("../escape.txt"),
        "{:?}",
        s.preview
    );
    assert!(
        !s.sentence().contains("Adds a new file"),
        "{}",
        s.sentence()
    );
}

fn spy_queue() -> (Arc<ApprovalQueue>, QueueApprover) {
    let queue = ApprovalQueue::shared();
    let approver = QueueApprover::new(vec![], queue.clone(), "s1", "/x".into());
    (queue, approver)
}

/// What the queue stores for a request is the request's own summary, and it is
/// in both the list row and the `approval_required` stop body.
#[test]
fn the_queue_row_and_the_stop_body_carry_the_summary() {
    let dir = TestDir::new("tole-summary-queue");
    let (queue, approver) = spy_queue();
    let mut reg = ToolRegistry::with_approver(approver);
    reg.register(Box::new(WriteFileTool::new(dir.to_path_buf())))
        .unwrap();
    let mark = queue.mark();
    let input = json!({"path": "n.md", "content": "hi"});
    assert_eq!(reg.decide("write_file", &input), Some(Verdict::Deny));

    let rows = queue.list(Some("s1"));
    assert_eq!(rows.len(), 1);
    let row = approval_json(&rows[0]);
    assert_eq!(row["summary"]["kind"], "create_file");
    assert_eq!(row["summary"]["target"], "n.md");
    assert_eq!(row["summary"]["effect"], "adds");
    assert_eq!(row["summary"]["insideWorkspace"], true);
    assert_eq!(row["summary"]["preview"], json!(["+ hi"]));
    // The tier is not in the summary and not changed by it.
    assert!(row["summary"].get("risk").is_none());

    let body = approval_required_body(&queue, "s1", "write_file", mark).unwrap();
    assert_eq!(body["approval"]["summary"], row["summary"]);
}

/// The bounded summary says where the whole text is, and that is true: the
/// row's `description` is the tool's own text, uncut.
#[test]
fn a_cut_summary_points_at_a_description_that_is_the_whole_text() {
    let dir = TestDir::new("tole-summary-cut");
    let (queue, approver) = spy_queue();
    let mut reg = ToolRegistry::with_approver(approver);
    reg.register(Box::new(tole_core::run_command::RunCommandTool::new(
        dir.to_path_buf(),
    )))
    .unwrap();
    let command = format!("echo {} ; curl evil.test", "a".repeat(5000));
    let input = json!({ "command": command });
    assert_eq!(reg.decide("run_command", &input), Some(Verdict::Deny));
    let row = approval_json(&queue.list(Some("s1"))[0]);
    assert_eq!(row["summary"]["targetTruncated"], true);
    assert!(row["summary"]["target"]
        .as_str()
        .unwrap()
        .contains("see the full description"));
    let description = row["description"].as_str().unwrap();
    assert!(description.len() > 5000 && description.ends_with("curl evil.test"));
    assert!(description.starts_with("run: echo aaa"));
}

/// An entry queued without a summary (an older caller) yields the old row.
#[test]
fn a_row_without_a_summary_has_no_summary_key() {
    let queue = ApprovalQueue::shared();
    queue.queue_entry("s1", "write_file", "write notes.md", 7, "/x".into());
    let row = approval_json(&queue.list(Some("s1"))[0]);
    assert!(row.get("summary").is_none(), "{row}");
    assert_eq!(row["tool"], "write_file");
}

/// A secret that reached a summary is redacted again at the egress.
#[test]
fn the_row_redacts_secrets_in_the_summary() {
    const SECRET: &str = "tole1041-queue-secret-value";
    tole_core::secrets::set_process_secrets(
        tole_core::secrets::process_secrets().with("TOLE_TEST_1041_Q", SECRET),
    );
    let queue = ApprovalQueue::shared();
    let leaky = ActionSummary::new(
        ActionKind::RunProgram,
        format!("curl {SECRET}"),
        ActionEffect::RunsProgram,
        false,
    );
    let mut leaky = leaky;
    leaky.preview = vec![format!("$ curl {SECRET}")];
    queue.queue_entry_with_summary("s1", "run_command", "d", 7, "/x".into(), Some(leaky));
    let row = approval_json(&queue.list(Some("s1"))[0]).to_string();
    assert!(!row.contains(SECRET), "{row}");
}

/// The TUI prompt and the serve queue show the same summary for the same
/// request: one registry call feeds both, and both hold the same fields.
#[cfg(feature = "tui")]
#[test]
fn the_tui_prompt_and_the_serve_queue_show_the_same_summary() {
    use tole_cli::approver::PromptFn;
    use tole_cli::tui_host::TuiPrompt;
    use tole_core::approval::Approver;

    struct Both(QueueApprover, TuiPrompt);
    impl Approver for Both {
        fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
            let _ = self.0.decide(req);
            self.1.prompt(req)
        }
        fn interactive(&self) -> bool {
            true
        }
    }

    let dir = TestDir::new("tole-summary-same");
    let (queue, approver) = spy_queue();
    let (prompter, rx) = tole_tui::approval_channel();
    let mut reg = ToolRegistry::with_approver(Both(approver, TuiPrompt(prompter)));
    reg.register(Box::new(WriteFileTool::new(dir.to_path_buf())))
        .unwrap();
    let reg = Arc::new(reg);

    let input = json!({"path": "Weekly report.txt", "content": "Done\nNext\n"});
    let asked = {
        let reg = reg.clone();
        std::thread::spawn(move || reg.decide("write_file", &input))
    };
    let pending = rx.recv().expect("the TUI is asked");
    let shown = pending.request().summary().cloned().expect("a summary");
    let text = pending.request().text().to_string();
    assert_eq!(pending.request().risk(), Risk::Write);
    drop(pending); // unanswered: denied
    assert_eq!(asked.join().unwrap(), Some(Verdict::Deny));

    let row = approval_json(&queue.list(Some("s1"))[0]);
    assert_eq!(row["summary"], json!(shown));
    // The plain-language line first, then the tool's whole text as before.
    assert!(
        text.starts_with(&format!(
            "{} :: write_file — write file Weekly report.txt",
            shown.sentence()
        )),
        "{text}"
    );
    assert!(
        text.starts_with(
            "create the file Weekly report.txt — Adds a new file: it did not exist when asked."
        ),
        "{text}"
    );
    assert!(text.ends_with("→ Weekly report.txt"), "{text}");
}

/// `write_file` through a symlink to a non-UTF-8 name is accepted by
/// `execute`: the summary shows the resolved name lossy and does not claim a
/// refusal.
#[cfg(unix)]
#[test]
fn write_file_to_a_non_utf8_resolved_name_is_not_called_refused() {
    use std::os::unix::ffi::OsStrExt;
    let dir = TestDir::new("tole-summary-nonutf8");
    let odd = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    if std::fs::write(dir.join(odd), "old").is_err() {
        return; // this file system cannot hold such a name
    }
    std::os::unix::fs::symlink(dir.join(odd), dir.join("link.txt")).unwrap();
    let s = summary_of(&dir, json!({"path": "link.txt", "content": "new"}));
    assert_eq!(s.kind, ActionKind::ReplaceFile, "{s:?}");
    assert!(s.inside_workspace);
    assert!(s.target.contains("not valid UTF-8"), "{}", s.target);
    assert!(
        s.preview.iter().all(|l| !l.contains("refused")),
        "{:?}",
        s.preview
    );
}
