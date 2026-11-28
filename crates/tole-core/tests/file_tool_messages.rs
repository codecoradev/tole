//! #1285: the file tools take an absolute path that lies inside the workspace
//! as the same file as its relative form, and tell a path outside it from one
//! that escapes the jail; both refusals name the workspace. The jail itself is
//! unchanged: every refusal below is still a refusal and nothing outside is
//! touched.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use std::path::Path;
use test_dir::TestDir;
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
use tole_core::read_file::ReadFileTool;
use tole_core::tool::Tool;

/// Every tool that resolves a `path` through the jail, over one workspace.
fn tools(root: &Path) -> Vec<(&'static str, Box<dyn Tool>)> {
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

/// The path as a model would have it after `pwd`: absolute, under the root.
fn abs(ws: &TestDir, rel: &str) -> String {
    ws.join(rel).to_string_lossy().to_string()
}

#[test]
fn an_absolute_path_inside_the_workspace_is_the_same_file_as_the_relative_one() {
    let ws = TestDir::new("tole-msg-abs-in");
    std::fs::create_dir(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/f.txt"), "keep").unwrap();
    let read = ReadFileTool::new(ws.to_path_buf());
    let out = read
        .execute(json!({ "path": abs(&ws, "sub/f.txt") }))
        .unwrap();
    assert_eq!(out["content"], "keep");
    // The canonical spelling of the root works too (a root given through a link).
    let canon = ws.canonicalize().unwrap().join("sub/f.txt");
    let out = read
        .execute(json!({ "path": canon.to_str().unwrap() }))
        .unwrap();
    assert_eq!(out["content"], "keep");
    // edit_file: the hash read through the absolute path anchors the edit.
    let edit = EditFileTool::new(ws.to_path_buf());
    edit.execute(json!({
        "path": abs(&ws, "sub/f.txt"), "old_hash": out["hash"],
        "old_text": "keep", "new_text": "kept"
    }))
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.join("sub/f.txt")).unwrap(),
        "kept"
    );
    // The approval target and the whole summary are the relative form's.
    let both = |p: String| {
        let input = json!({ "path": p, "old_hash": "0", "old_text": "kept", "new_text": "x" });
        (
            edit.approval_target(&input).unwrap(),
            edit.action_summary(&input).unwrap(),
        )
    };
    assert_eq!(both(abs(&ws, "sub/f.txt")), both("sub/f.txt".into()));
    assert_eq!(both(abs(&ws, "sub/f.txt")).0, "sub/f.txt");
    DeleteFileTool::new(ws.to_path_buf())
        .execute(json!({ "path": abs(&ws, "sub/f.txt") }))
        .unwrap();
    assert!(!ws.join("sub/f.txt").exists());
}

#[test]
fn an_absolute_path_outside_the_workspace_is_refused_and_says_where_the_workspace_is() {
    let ws = TestDir::new("tole-msg-abs-out");
    let outside = TestDir::new("tole-msg-abs-outside");
    std::fs::write(outside.join("s.txt"), "secret").unwrap();
    let abs_out = outside.join("s.txt").to_string_lossy().to_string();
    let missing = outside.join("nope.txt").to_string_lossy().to_string();
    for (name, t) in tools(&ws) {
        let err = refusal(t.as_ref(), &abs_out);
        assert!(err.contains("not inside the workspace"), "{name}: {err}");
        assert!(
            err.contains(&*ws.to_string_lossy()),
            "{name} names the root: {err}"
        );
        assert!(err.contains(&abs_out), "{name} names the path: {err}");
        assert!(
            err.contains("relative to"),
            "{name} says how to fix it: {err}"
        );
        assert!(err.contains("--workspace"), "{name}: {err}");
        assert!(!err.contains("escapes the jail"), "{name}: {err}");
        // No oracle: a file that is not there is refused the same way.
        let gone = refusal(t.as_ref(), &missing);
        assert!(gone.contains("not inside the workspace"), "{name}: {gone}");
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("s.txt")).unwrap(),
        "secret"
    );
}

#[test]
fn the_workspace_root_itself_and_a_sibling_with_the_same_prefix_are_not_inside() {
    let base = TestDir::new("tole-msg-prefix");
    std::fs::create_dir(base.join("ws")).unwrap();
    std::fs::create_dir(base.join("ws-evil")).unwrap();
    std::fs::write(base.join("ws-evil/s.txt"), "secret").unwrap();
    let ws = base.join("ws");
    for (name, t) in tools(&ws) {
        for p in [
            ws.to_string_lossy().to_string(),
            base.join("ws-evil/s.txt").to_string_lossy().to_string(),
        ] {
            let err = refusal(t.as_ref(), &p);
            assert!(
                err.contains("not inside the workspace"),
                "{name} {p}: {err}"
            );
        }
    }
}

#[test]
fn an_absolute_path_with_dot_dot_back_out_is_an_escape() {
    let base = TestDir::new("tole-msg-abs-dotdot");
    std::fs::create_dir(base.join("ws")).unwrap();
    std::fs::write(base.join("s.txt"), "secret").unwrap();
    let root = base.join("ws");
    let p = format!("{}/../s.txt", base.join("ws").display());
    for (name, t) in tools(&root) {
        let err = refusal(t.as_ref(), &p);
        assert!(err.contains("escapes the jail"), "{name}: {err}");
    }
    assert_eq!(
        std::fs::read_to_string(base.join("s.txt")).unwrap(),
        "secret"
    );
}

#[cfg(unix)]
#[test]
fn an_absolute_path_through_a_symlink_out_is_an_escape() {
    let ws = TestDir::new("tole-msg-abs-symlink");
    let outside = TestDir::new("tole-msg-abs-symlink-out");
    std::fs::write(outside.join("s.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.join("s.txt"), ws.join("leak.txt")).unwrap();
    std::os::unix::fs::symlink(&*outside, ws.join("dir")).unwrap();
    for (name, t) in tools(&ws) {
        for p in [abs(&ws, "leak.txt"), abs(&ws, "dir/s.txt")] {
            let err = refusal(t.as_ref(), &p);
            assert!(err.contains("escapes the jail"), "{name} {p}: {err}");
        }
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
