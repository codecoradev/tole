//! `read_file` tool (E7): ReadOnly file read with the same path jail as
//! `write_file`. The agent can read files inside the working tree but
//! never traverse out of it (no `..`, no absolute paths, no symlink
//! escapes).

use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Hard cap on file size (bytes) — a runaway read (e.g. pointing at a
/// dataset) must not blow up the durable log / provider context.
pub const MAX_READ_BYTES: u64 = 1_048_576; // 1 MiB

/// Read a file under a fixed root directory. Input: `{ "path": "rel" }`.
#[derive(Debug, Clone)]
pub struct ReadFileTool {
    root: PathBuf,
}

impl ReadFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve `rel` inside the jail. Mirrors `WriteFileTool::jailed`
    /// (deepest-existing-ancestor canonicalize), plus a symlink check on
    /// the final target: reading *through* a symlink out of the jail is
    /// also an escape. Errors distinguish escape from plain not-found so
    /// the model gets actionable feedback.
    fn jailed(&self, rel: &str) -> Result<PathBuf, String> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() || rel.contains("..") {
            return Err(format!("read_file: path escapes the jail: {rel}"));
        }
        let target = self.root.join(rel_path);
        if !target.exists() {
            return Err(format!("read_file: not found in jail: {rel}"));
        }
        let canon_root = self
            .root
            .canonicalize()
            .map_err(|e| format!("read_file: cannot resolve jail root: {e}"))?;
        let canon_target = target
            .canonicalize()
            .map_err(|_| format!("read_file: not found in jail: {rel}"))?;
        if canon_target != canon_root && !canon_target.starts_with(&canon_root) {
            return Err(format!("read_file: path escapes the jail: {rel}"));
        }
        Ok(canon_target)
    }
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn describe(&self, input: &Value) -> String {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing path>");
        format!("read file {path}")
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path (no .., no absolute paths)" }
            },
            "required": ["path"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Err("read_file: missing 'path'".into());
        };
        let target = self.jailed(path)?;
        let meta = std::fs::metadata(&target)
            .map_err(|e| format!("read_file: stat {}: {e}", target.display()))?;
        if meta.is_dir() {
            return Err(format!("read_file: is a directory: {path}"));
        }
        if meta.len() > MAX_READ_BYTES {
            return Err(format!(
                "read_file: {} is {} bytes (max {})",
                path,
                meta.len(),
                MAX_READ_BYTES
            ));
        }
        let bytes =
            std::fs::read(&target).map_err(|e| format!("read_file: {}: {e}", target.display()))?;
        // Lossy decode: source files are UTF-8; anything else still gets
        // a readable form instead of a hard error.
        let content = String::from_utf8_lossy(&bytes).into_owned();
        Ok(json!({ "path": path, "content": content, "bytes": bytes.len() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("tole-core-read-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_file_inside_jail() {
        let dir = tmpdir("ok");
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let t = ReadFileTool::new(dir.clone());
        let out = t.execute(json!({ "path": "a.txt" })).unwrap();
        assert_eq!(out["content"], json!("hello"));
        assert_eq!(out["bytes"], json!(5));
    }

    #[test]
    fn rejects_absolute_and_traversal() {
        let dir = tmpdir("jail");
        let t = ReadFileTool::new(dir);
        assert!(t.execute(json!({ "path": "/etc/passwd" })).is_err());
        assert!(t.execute(json!({ "path": "../secret" })).is_err());
        assert!(t.execute(json!({ "path": "a/../../secret" })).is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let dir = tmpdir("symlink");
        let outside = std::env::temp_dir().join(format!("tole-outside-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        std::fs::write(&outside, "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("leak.txt")).unwrap();
        let t = ReadFileTool::new(dir);
        #[cfg(unix)]
        {
            let err = t.execute(json!({ "path": "leak.txt" })).unwrap_err();
            assert!(err.contains("escape"), "got: {err}");
        }
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn missing_file_reports_not_found() {
        let dir = tmpdir("missing");
        let t = ReadFileTool::new(dir);
        let err = t.execute(json!({ "path": "nope.txt" })).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
        assert!(!err.contains("escape"), "missing ≠ escape: {err}");
    }

    #[test]
    fn rejects_directory_read() {
        let dir = tmpdir("dir");
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        let t = ReadFileTool::new(dir);
        let err = t.execute(json!({ "path": "subdir" })).unwrap_err();
        assert!(err.contains("directory"), "got: {err}");
    }
}
