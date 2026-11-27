//! #1285: `write_file` tells an absolute path (it must be relative) from a
//! path that escapes the jail, and names the workspace. The jail itself is
//! unchanged: both are still refused and nothing is written.

mod common;
use common::TestDir;
use serde_json::json;
use tole_cli::tools::WriteFileTool;
use tole_core::tool::Tool;

fn write(ws: &TestDir, path: &str) -> String {
    WriteFileTool::new(ws.to_path_buf())
        .execute(json!({ "path": path, "content": "x" }))
        .expect_err(&format!("{path} must be refused"))
}

#[test]
fn an_absolute_path_is_told_to_be_relative() {
    let ws = TestDir::new("tole-wmsg-abs");
    let inside = ws.join("in.txt").to_string_lossy().to_string();
    let outside = std::env::temp_dir().join("tole-wmsg-outside.txt");
    for p in [inside.as_str(), outside.to_str().unwrap()] {
        let err = write(&ws, p);
        assert!(err.contains("must be relative, not absolute"), "{err}");
        assert!(
            err.contains(&*ws.to_string_lossy()),
            "names the root: {err}"
        );
        assert!(!err.contains("escapes the jail"), "{err}");
    }
    assert!(!ws.join("in.txt").exists() && !outside.exists());
}

#[test]
fn dot_dot_still_escapes_the_jail_and_names_the_workspace() {
    let ws = TestDir::new("tole-wmsg-dotdot");
    let err = write(&ws, "../escape.txt");
    assert!(err.contains("escapes the jail"), "{err}");
    assert!(err.contains(&*ws.to_string_lossy()), "{err}");
    assert!(err.contains("--workspace"), "{err}");
}
