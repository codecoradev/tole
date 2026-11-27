//! Unit tests of `edit_file` / `delete_file` / `content_hash`, moved out of
//! `src/` (#1249): they only use the public API of the crate.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::Path;
use tole_core::file_tools::*;
use tole_core::tool::Tool;

use test_dir::TestDir;
fn tmpdir(name: &str) -> TestDir {
    TestDir::new(&format!("tole-file-tools-{name}"))
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

// ---- content_hash ----

#[test]
fn hash_is_stable_and_sensitive() {
    let a = content_hash("fn main() {}");
    assert_eq!(a, content_hash("fn main() {}"));
    assert_ne!(a, content_hash("fn main()  "));
    assert_eq!(a.len(), 16);
}

#[test]
fn stale_temp_self_heals() {
    let dir = tmpdir("stale-temp");
    write(&dir, "f.txt", "old\n");
    // Simulate a crashed prior attempt: stale temp with our pid.
    let stale = dir.join(format!(".tole-edit-{}.tmp", std::process::id()));
    std::fs::write(&stale, "garbage").unwrap();
    let h = content_hash(&std::fs::read_to_string(dir.join("f.txt")).unwrap());
    let t = EditFileTool::new(dir.to_path_buf());
    t.execute(json!({
        "path": "f.txt",
        "old_hash": h,
        "old_text": "old",
        "new_text": "new"
    }))
    .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "new\n");
    assert!(!stale.exists(), "stale temp must be cleaned up");
}

// ---- edit_file ----

#[test]
fn edit_applies_with_fresh_anchor_and_returns_new_hash() {
    let dir = tmpdir("edit-ok");
    write(&dir, "src/lib.rs", "fn a() {}\nfn b() {}\n");
    let t = EditFileTool::new(dir.to_path_buf());
    let original = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    let out = t
        .execute(json!({
            "path": "src/lib.rs",
            "old_hash": content_hash(&original),
            "old_text": "fn a() {}",
            "new_text": "fn a() -> u32 { 42 }"
        }))
        .unwrap();
    let updated = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert_eq!(updated, "fn a() -> u32 { 42 }\nfn b() {}\n");
    assert_eq!(out["new_hash"], json!(content_hash(&updated)));
    // Hash chaining: the returned new_hash is a valid anchor for the
    // next edit without re-reading.
    let out2 = t
        .execute(json!({
            "path": "src/lib.rs",
            "old_hash": out["new_hash"].as_str().unwrap(),
            "old_text": "fn b() {}",
            "new_text": "fn b() -> u32 { 7 }"
        }))
        .unwrap();
    assert_eq!(
        out2["new_hash"],
        json!(content_hash("fn a() -> u32 { 42 }\nfn b() -> u32 { 7 }\n"))
    );
}

#[test]
fn edit_refuses_stale_anchor() {
    let dir = tmpdir("edit-stale");
    write(&dir, "src/lib.rs", "one\ntwo\n");
    let t = EditFileTool::new(dir.to_path_buf());
    let stale_hash = content_hash("different content entirely");
    let err = t
        .execute(json!({
            "path": "src/lib.rs",
            "old_hash": stale_hash,
            "old_text": "one",
            "new_text": "1"
        }))
        .unwrap_err();
    // Refused before any mutation: file unchanged.
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        "one\ntwo\n"
    );
    assert!(err.contains("stale anchor"), "error should explain: {err}");
    assert!(
        err.contains("Re-read"),
        "error should instruct re-read: {err}"
    );
}

#[test]
fn edit_refuses_ambiguous_and_missing_old_text() {
    let dir = tmpdir("edit-ambig");
    write(&dir, "dup.txt", "x\nx\n");
    write(&dir, "no.txt", "a\n");
    let t = EditFileTool::new(dir.to_path_buf());
    // 2 matches -> refused, file unchanged.
    let dup = std::fs::read_to_string(dir.join("dup.txt")).unwrap();
    let err = t
        .execute(json!({
            "path": "dup.txt",
            "old_hash": content_hash(&dup),
            "old_text": "x",
            "new_text": "y"
        }))
        .unwrap_err();
    assert!(err.contains("2 times"), "error should count matches: {err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("dup.txt")).unwrap(),
        "x\nx\n"
    );
    // 0 matches -> actionable error.
    let no = std::fs::read_to_string(dir.join("no.txt")).unwrap();
    let err = t
        .execute(json!({
            "path": "no.txt",
            "old_hash": content_hash(&no),
            "old_text": "zzz",
            "new_text": "y"
        }))
        .unwrap_err();
    assert!(
        err.contains("not found"),
        "error should say not found: {err}"
    );
}

#[test]
fn edit_rejects_jail_escapes() {
    let dir = tmpdir("edit-jail");
    write(&dir, "in.txt", "x");
    let t = EditFileTool::new(dir.to_path_buf());
    for bad in ["/etc/passwd", "../escape.txt", "a/../../esc.txt"] {
        assert!(
            t.execute(json!({
                "path": bad,
                "old_hash": "0000000000000000",
                "old_text": "x",
                "new_text": "y"
            }))
            .is_err(),
            "should reject {bad}"
        );
    }
}

#[test]
fn edit_empty_new_text_deletes_match() {
    let dir = tmpdir("edit-del-match");
    write(&dir, "f.txt", "keep\nDROP ME\nkeep\n");
    let t = EditFileTool::new(dir.to_path_buf());
    let original = std::fs::read_to_string(dir.join("f.txt")).unwrap();
    t.execute(json!({
        "path": "f.txt",
        "old_hash": content_hash(&original),
        "old_text": "DROP ME\n",
        "new_text": ""
    }))
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "keep\nkeep\n"
    );
}

#[test]
fn edit_refuses_directory_and_missing_file() {
    let dir = tmpdir("edit-dir");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let t = EditFileTool::new(dir.to_path_buf());
    let err = t
        .execute(json!({
            "path": "sub",
            "old_hash": "0000000000000000",
            "old_text": "x",
            "new_text": "y"
        }))
        .unwrap_err();
    assert!(
        err.contains("directory"),
        "error should mention directory: {err}"
    );
    let err = t
        .execute(json!({
            "path": "missing.txt",
            "old_hash": "0000000000000000",
            "old_text": "x",
            "new_text": "y"
        }))
        .unwrap_err();
    assert!(
        err.contains("not found"),
        "error should say not found: {err}"
    );
}

// ---- delete_file ----

#[test]
fn delete_removes_file() {
    let dir = tmpdir("del-ok");
    write(&dir, "scratch.txt", "bye");
    let t = DeleteFileTool::new(dir.to_path_buf());
    let out = t.execute(json!({ "path": "scratch.txt" })).unwrap();
    assert_eq!(out["deleted"], json!(true));
    assert!(!dir.join("scratch.txt").exists());
}

#[test]
fn delete_refuses_directories_and_escapes() {
    let dir = tmpdir("del-dir");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let t = DeleteFileTool::new(dir.to_path_buf());
    let err = t.execute(json!({ "path": "sub" })).unwrap_err();
    assert!(
        err.contains("directory"),
        "error should mention directory: {err}"
    );
    assert!(dir.join("sub").exists(), "directory must survive");
    assert!(t.execute(json!({ "path": "../out.txt" })).is_err());
    assert!(t.execute(json!({ "path": "/etc/passwd" })).is_err());
}

#[test]
fn delete_missing_file_is_actionable_error() {
    let dir = tmpdir("del-missing");
    let t = DeleteFileTool::new(dir.to_path_buf());
    let err = t.execute(json!({ "path": "ghost.txt" })).unwrap_err();
    assert!(
        err.contains("not found"),
        "error should say not found: {err}"
    );
}
