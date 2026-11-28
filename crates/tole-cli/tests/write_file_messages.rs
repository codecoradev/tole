//! #1285: `write_file` takes an absolute path that lies inside the workspace
//! as the same file as its relative form, tells a path outside it from one
//! that escapes the jail, and names the workspace. The jail itself is
//! unchanged: everything outside is still refused and nothing is written.

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
fn an_absolute_path_inside_the_workspace_writes_the_same_file_as_the_relative_one() {
    let ws = TestDir::new("tole-wmsg-abs-in");
    let t = WriteFileTool::new(ws.to_path_buf());
    let inside = ws.join("sub/in.txt").to_string_lossy().to_string();
    t.execute(json!({ "path": inside, "content": "a" }))
        .unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("sub/in.txt")).unwrap(), "a");
    // The approval target and the whole summary are the relative form's.
    let both = |p: &str| {
        let input = json!({ "path": p, "content": "b" });
        (t.approval_target(&input), t.action_summary(&input))
    };
    assert_eq!(both(&inside), both("sub/in.txt"));
    assert_eq!(both(&inside).0.as_deref(), Some("sub/in.txt"));
    assert!(both(&inside).1.is_some_and(|s| s.inside_workspace));
}

#[test]
fn an_absolute_path_outside_the_workspace_is_refused_and_names_the_workspace() {
    let ws = TestDir::new("tole-wmsg-abs-out");
    let outside = std::env::temp_dir().join("tole-wmsg-outside.txt");
    let root = ws.join("").to_string_lossy().to_string();
    for p in [
        outside.to_str().unwrap().to_string(),
        ws.to_string_lossy().to_string(), // the workspace itself is no file
    ] {
        let err = write(&ws, &p);
        assert!(err.contains("not inside the workspace"), "{err}");
        assert!(
            err.contains(root.trim_end_matches('/')),
            "names the root: {err}"
        );
        assert!(err.contains(&p), "names the path: {err}");
        assert!(err.contains("--workspace"), "{err}");
        assert!(!err.contains("escapes the jail"), "{err}");
    }
    assert!(!outside.exists());
}

#[cfg(unix)]
#[test]
fn an_absolute_path_through_a_symlink_or_dot_dot_still_escapes_the_jail() {
    let ws = TestDir::new("tole-wmsg-abs-esc");
    let out = TestDir::new("tole-wmsg-abs-esc-out");
    std::os::unix::fs::symlink(&*out, ws.join("dir")).unwrap();
    for p in [
        ws.join("dir/x.txt").to_string_lossy().to_string(),
        format!("{}/../x.txt", ws.display()),
    ] {
        let err = write(&ws, &p);
        assert!(err.contains("escapes the jail"), "{p}: {err}");
    }
    assert!(!out.join("x.txt").exists());
}

#[test]
fn dot_dot_still_escapes_the_jail_and_names_the_workspace() {
    let ws = TestDir::new("tole-wmsg-dotdot");
    let err = write(&ws, "../escape.txt");
    assert!(err.contains("escapes the jail"), "{err}");
    assert!(err.contains(&*ws.to_string_lossy()), "{err}");
    assert!(err.contains("--workspace"), "{err}");
}
