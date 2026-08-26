//! Host tools for the CLI: `write_file` (Write risk) with a path jail.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use tole_core::tool::{Risk, Tool};

/// Write a file under a fixed root directory. Inputs:
/// `{ "path": "rel/path.txt", "content": "..." }`.
///
/// Path jail: `path` must be relative, no `..`, no symlink escape — the
/// resolved target must stay inside `root`. This is the structural
/// defense against the model escaping the working tree.
pub struct WriteFileTool {
    root: PathBuf,
}

impl WriteFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve `rel` inside the jail; `None` when it escapes.
    ///
    /// Missing parents are fine (`execute` creates them) — canonicalize
    /// the deepest *existing* ancestor, verify it stays inside the root,
    /// then re-attach the not-yet-existing tail. Symlinks in the
    /// existing portion are resolved by canonicalization; the missing
    /// tail cannot be a symlink yet.
    fn jailed(&self, rel: &str) -> Option<PathBuf> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() || rel.contains("..") {
            return None;
        }
        let target = self.root.join(rel_path);
        let canon_root = self.root.canonicalize().ok()?;
        // Deepest existing ancestor of `target`.
        let mut existing = target.as_path();
        let mut tail = Vec::new();
        loop {
            if existing.exists() {
                break;
            }
            let parent = existing.parent()?;
            tail.push(existing.file_name()?);
            existing = parent;
        }
        let canon_existing = existing.canonicalize().ok()?;
        if canon_existing != canon_root && !canon_existing.starts_with(&canon_root) {
            return None; // symlink escape through an existing ancestor
        }
        let mut resolved = canon_existing;
        for comp in tail.iter().rev() {
            resolved = resolved.join(comp);
        }
        Some(resolved)
    }

    /// Reject final targets that are symlinks — kept for the
    /// non-unix fallback path inside `write_jailed`.
    #[cfg(not(unix))]
    fn reject_symlink_target(target: &Path) -> Result<(), String> {
        let is_symlink = target
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink {
            return Err(format!(
                "refusing to write through symlink {}",
                target.display()
            ));
        }
        Ok(())
    }
}

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing path>");
        format!("write file {path}")
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Err("write_file: missing 'path'".into());
        };
        let Some(content) = input.get("content").and_then(Value::as_str) else {
            return Err("write_file: missing 'content'".into());
        };
        let Some(target) = self.jailed(path) else {
            return Err(format!("write_file: path escapes the jail: {path}"));
        };
        let bytes = content.len();
        self.write_jailed(&target, content)?;
        Ok(json!({ "path": path, "bytes": bytes }))
    }
}

impl WriteFileTool {
    /// Create the missing parent chain one component at a time —
    /// `symlink_metadata` never follows links, so a symlinked component
    /// is rejected instead of traversed — then write with `O_NOFOLLOW`
    /// (unix): if the final component is or becomes a symlink, the open
    /// itself fails instead of following it out of the jail. Closes the
    /// check-then-write race (TOCTOU) on both the tail and the target.
    fn write_jailed(&self, target: &Path, content: &str) -> Result<(), String> {
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("write_file: jail root: {e}"))?;
        let parent = target.parent().unwrap_or(Path::new(""));
        let rel_parent = parent
            .strip_prefix(&root)
            .map_err(|_| "write_file: internal: target outside jail".to_string())?;
        // Walk down from the canonical root, creating what's missing.
        let mut cur = root;
        for comp in rel_parent.components() {
            cur = cur.join(comp.as_os_str());
            cur = match std::fs::symlink_metadata(&cur) {
                // Existing real directory — descend.
                Ok(m) if m.is_dir() => cur,
                // Symlink or non-directory: refuse (symlink_metadata
                // doesn't follow links, so a symlinked dir never reads
                // as `is_dir`).
                Ok(_) => {
                    return Err(format!(
                        "write_file: refusing non-directory/symlink {}",
                        cur.display()
                    ))
                }
                // Missing: create, then re-check the winner of any race
                // — only descend if it's now a real directory.
                Err(_) => {
                    std::fs::create_dir(&cur)
                        .map_err(|e| format!("write_file: create {}: {e}", cur.display()))?;
                    match std::fs::symlink_metadata(&cur) {
                        Ok(m) if m.is_dir() => cur,
                        Ok(_) => {
                            return Err(format!(
                                "write_file: {} lost race to non-directory",
                                cur.display()
                            ))
                        }
                        Err(e) => return Err(format!("write_file: stat {}: {e}", cur.display())),
                    }
                }
            };
        }
        // Final component: open with O_NOFOLLOW so a symlink at the
        // leaf fails at open time rather than being followed. The
        // libc constant is per-platform (0o400000 on Linux, 0x0100 on
        // macOS/FreeBSD, …) — never hardcode it.
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(target)
                .map_err(|e| format!("write_file: open {}: {e}", target.display()))?;
            f.write_all(content.as_bytes())
                .map_err(|e| format!("write_file: write {}: {e}", target.display()))?;
        }
        #[cfg(not(unix))]
        {
            let is_symlink = target
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                return Err(format!("write_file: refusing symlink {}", target.display()));
            }
            std::fs::write(target, content)
                .map_err(|e| format!("write_file: {}: {e}", target.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("tole-cli-tools-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_and_reports_bytes() {
        let dir = tmpdir("write-ok");
        let t = WriteFileTool::new(dir.clone());
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
        let t = WriteFileTool::new(dir);
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
    fn creates_missing_parent_directories() {
        let dir = tmpdir("write-parents");
        let t = WriteFileTool::new(dir.clone());
        t.execute(json!({ "path": "a/b/c.txt", "create_dirs": true, "content": "deep" }))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a/b/c.txt")).unwrap(),
            "deep"
        );
    }
}
