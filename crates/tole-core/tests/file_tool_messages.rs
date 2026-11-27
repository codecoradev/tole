//! #1285: the file tools tell an absolute path (it must be relative) from a
//! path that escapes the jail, and both name the workspace. The jail itself
//! is unchanged: every case below is still refused.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use test_dir::TestDir;
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
use tole_core::read_file::ReadFileTool;
use tole_core::tool::Tool;

/// Every tool that resolves a `path` through the jail, over one workspace.
fn tools(root: &TestDir) -> Vec<(&'static str, Box<dyn Tool>)> {
    vec![
        ("read_file", Box::new(ReadFileTool::new(root.to_path_buf()))),
        ("edit_file", Box::new(EditFileTool::new(root.to_path_buf()))),
        (
            "delete_file",
            Box::new(DeleteFileTool::new(root.to_path_buf())),
        ),
    ]
}

fn refusal(tool: &dyn Tool, path: &str) -> String {
    let input: Value = json!({ "path": path, "old_hash": "0", "old_text": "a", "new_text": "b" });
    tool.execute(input)
        .expect_err(&format!("{path} must be refused"))
}

#[test]
fn an_absolute_path_inside_the_workspace_is_refused_and_told_to_be_relative() {
    let ws = TestDir::new("tole-msg-abs-in");
    std::fs::write(ws.join("f.txt"), "keep").unwrap();
    let abs = ws.join("f.txt").to_string_lossy().to_string();
    for (name, t) in tools(&ws) {
        let err = refusal(t.as_ref(), &abs);
        assert!(
            err.contains("must be relative to the workspace"),
            "{name}: {err}"
        );
        assert!(
            err.contains(&*ws.to_string_lossy()),
            "{name} names the root: {err}"
        );
        assert!(!err.contains("escapes the jail"), "{name}: {err}");
    }
    assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "keep");
}

#[test]
fn an_absolute_path_outside_the_workspace_is_refused_the_same_way() {
    let ws = TestDir::new("tole-msg-abs-out");
    let outside = TestDir::new("tole-msg-abs-outside");
    std::fs::write(outside.join("s.txt"), "secret").unwrap();
    let abs = outside.join("s.txt").to_string_lossy().to_string();
    for (name, t) in tools(&ws) {
        let err = refusal(t.as_ref(), &abs);
        assert!(
            err.contains("must be relative to the workspace"),
            "{name}: {err}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("s.txt")).unwrap(),
        "secret"
    );
}

#[test]
fn dot_dot_still_escapes_the_jail_and_names_the_workspace() {
    let ws = TestDir::new("tole-msg-dotdot");
    for (name, t) in tools(&ws) {
        for p in ["../x.txt", "a/../../x.txt"] {
            let err = refusal(t.as_ref(), p);
            assert!(err.contains("escapes the jail"), "{name} {p}: {err}");
            assert!(err.contains(&*ws.to_string_lossy()), "{name} {p}: {err}");
            assert!(err.contains("--workspace"), "{name} {p}: {err}");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_out_still_escapes_the_jail_and_names_the_workspace() {
    let ws = TestDir::new("tole-msg-symlink");
    let outside = TestDir::new("tole-msg-symlink-out");
    std::fs::write(outside.join("s.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.join("s.txt"), ws.join("leak.txt")).unwrap();
    for (name, t) in tools(&ws) {
        let err = refusal(t.as_ref(), "leak.txt");
        assert!(err.contains("escapes the jail"), "{name}: {err}");
        assert!(err.contains(&*ws.to_string_lossy()), "{name}: {err}");
        assert!(err.contains("--workspace"), "{name}: {err}");
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("s.txt")).unwrap(),
        "secret"
    );
}
