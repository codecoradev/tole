//! Unit tests of `write_file`, moved out of `src/` (#1249): they only use the
//! public API of the crate.

mod common;
use common::TestDir;
use serde_json::json;
use tole_cli::tools::WriteFileTool;
use tole_core::tool::Tool;

fn tmpdir(name: &str) -> TestDir {
    TestDir::new(&format!("tole-cli-tools-{name}"))
}

#[test]
fn writes_and_reports_bytes() {
    let dir = tmpdir("write-ok");
    let t = WriteFileTool::new(dir.to_path_buf());
    let out = t
        .execute(json!({ "path": "notes.txt", "content": "hello" }))
        .unwrap();
    assert_eq!(out["bytes"], json!(5));
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "hello"
    );
}

#[test]
fn rejects_absolute_and_traversal() {
    let dir = tmpdir("write-jail");
    let t = WriteFileTool::new(dir.to_path_buf());
    assert!(t
        .execute(json!({ "path": "/etc/passwd", "content": "x" }))
        .is_err());
    assert!(t
        .execute(json!({ "path": "../escape.txt", "content": "x" }))
        .is_err());
    assert!(t
        .execute(json!({ "path": "a/../../escape.txt", "content": "x" }))
        .is_err());
}

#[test]
fn wire_spec_declares_required_properties() {
    // CodeCora scan + live E2E: a missing spec reached providers as a
    // property-less schema and the model legally answered
    // `arguments: {}`.
    let t = WriteFileTool::new(std::env::temp_dir());
    let spec = t.spec().expect("write_file must declare a schema");
    let props = &spec["properties"];
    assert!(props.get("path").is_some(), "path must be declared");
    assert!(props.get("content").is_some(), "content must be declared");
    assert_eq!(spec["required"], json!(["path", "content"]));
}

#[test]
fn creates_missing_parent_directories() {
    let dir = tmpdir("write-parents");
    let t = WriteFileTool::new(dir.to_path_buf());
    t.execute(json!({ "path": "a/b/c.txt", "create_dirs": true, "content": "deep" }))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("a/b/c.txt")).unwrap(),
        "deep"
    );
}
