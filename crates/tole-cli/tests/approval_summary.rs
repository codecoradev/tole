//! The approval summary (#1041) of `write_file` on the cli side: create or
//! replace, the bounded preview, a path the tool refuses, and a resolved name
//! that is not UTF-8.

mod common;
use common::TestDir;
use serde_json::{json, Value};

use tole_cli::tools::WriteFileTool;
use tole_core::action_summary::{
    ActionEffect, ActionKind, ActionSummary, MAX_LINE_CHARS, MAX_PREVIEW_LINES,
};
use tole_core::approval::ToolRequest;
use tole_core::tool::{Risk, Tool};

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
