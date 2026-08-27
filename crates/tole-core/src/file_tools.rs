//! File-mutation tools (E12): `edit_file` (Write risk, content-hash
//! anchored) and `delete_file` (Destructive risk). Together with
//! `write_file` (create) and `read_file` (read) this completes the
//! agent's CRUD file capability.
//!
//! ## Hash anchoring (hashline concept, oh-my-pi MIT)
//!
//! `read_file` returns a content hash alongside the content. `edit_file`
//! requires the model to quote that hash: the live file's hash must match
//! before any mutation is applied. A stale anchor (file changed since the
//! model last read it) is refused with an error that instructs a re-read
//! — the loop-guard-compatible recovery path. This kills the classic
//! edit-by-LLM failure mode: patching from stale context, silently
//! corrupting code.
//!
//! ## Editing model
//!
//! `old_text` → `new_text` exact replacement (not line ranges): content
//! anchors are the same evidence as the hash anchor, and line numbers rot
//! the moment anything above them changes. `old_text` must match exactly
//! once — 0 or 2+ matches are actionable errors, not best-effort guesses.

use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Hard cap on the file size `edit_file` will touch (bytes) — matches
/// `read_file`'s cap so a readable file is always editable.
pub const MAX_EDIT_BYTES: u64 = 1_048_576; // 1 MiB

/// Stable 16-hex content hash used by `read_file`/`edit_file` anchoring.
/// Hash lifetime is a single session — std hasher is fine, no new deps.
pub fn content_hash(content: &str) -> String {
    let mut h = DefaultHasher::new();
    content.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Resolve `rel` inside the jail. Shared shape with `WriteFileTool` /
/// `ReadFileTool`: `rel` must be relative, no `..`, and the resolved
/// target (through symlinks) must stay inside `root`.
fn jailed(root: &Path, rel: &str, tool: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() || rel.contains("..") {
        return Err(format!("{tool}: path escapes the jail: {rel}"));
    }
    let target = root.join(rel_path);
    if !target.exists() {
        return Err(format!("{tool}: not found in jail: {rel}"));
    }
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("{tool}: cannot resolve jail root: {e}"))?;
    let canon_target = target
        .canonicalize()
        .map_err(|_| format!("{tool}: not found in jail: {rel}"))?;
    if canon_target != canon_root && !canon_target.starts_with(&canon_root) {
        return Err(format!("{tool}: path escapes the jail: {rel}"));
    }
    Ok(canon_target)
}

/// Write `new` content to `target` atomically (temp + rename in the
/// target's directory), refusing symlinked targets. Atomicity matters:
/// a torn write would leave the on-disk hash inconsistent with what the
/// model believes it read.
fn atomic_write(target: &Path, new: &str) -> Result<(), String> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    // Temp name must not collide with anything the model could predict;
    // suffix with pid to keep concurrent sessions apart. A stale temp
    // from a crashed prior attempt would brick the tool (create_new
    // refuses overwrite), so an AlreadyExists temp is removed once and
    // the open retried — self-healing instead of session-bricking.
    let tmp = parent.join(format!(".tole-edit-{}.tmp", std::process::id()));
    let open_fresh = |tmp: &Path| -> Result<std::fs::File, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(tmp)
                .map_err(|e| format!("edit_file: open temp {}: {e}", tmp.display()))
        }
        #[cfg(not(unix))]
        {
            let is_symlink = tmp
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                return Err(format!("edit_file: refusing symlink {}", tmp.display()));
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(tmp)
                .map_err(|e| format!("edit_file: open temp {}: {e}", tmp.display()))
        }
    };
    let mut file = match open_fresh(&tmp) {
        Ok(f) => f,
        Err(e) => {
            // Self-heal: clear a stale temp from a crashed attempt, then
            // retry exactly once. Any second failure is a real error.
            let _ = std::fs::remove_file(&tmp);
            open_fresh(&tmp).map_err(|_| e)?
        }
    };
    if let Err(e) = file.write_all(new.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("edit_file: write temp {}: {e}", tmp.display()));
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "edit_file: rename into place {}: {e}",
            target.display()
        ));
    }
    Ok(())
}
/// Edit a file under a fixed root directory. Input:
/// `{ "path": "rel", "old_hash": "<16-hex from read_file>", "old_text": "...", "new_text": "..." }`.
#[derive(Debug, Clone)]
pub struct EditFileTool {
    root: PathBuf,
}

impl EditFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing path>");
        format!("edit file {path}")
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path (no .., no absolute paths)" },
                "old_hash": { "type": "string", "description": "Content hash (16-hex) returned by read_file when you read this file. Refused if the file changed since." },
                "old_text": { "type": "string", "description": "Exact text to replace. Must match exactly once in the file." },
                "new_text": { "type": "string", "description": "Replacement text. Empty string deletes the matched text." }
            },
            "required": ["path", "old_hash", "old_text", "new_text"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Err("edit_file: missing 'path'".into());
        };
        let Some(old_hash) = input.get("old_hash").and_then(Value::as_str) else {
            return Err("edit_file: missing 'old_hash' (read the file first)".into());
        };
        let Some(old_text) = input.get("old_text").and_then(Value::as_str) else {
            return Err("edit_file: missing 'old_text'".into());
        };
        let Some(new_text) = input.get("new_text").and_then(Value::as_str) else {
            return Err("edit_file: missing 'new_text'".into());
        };
        let target = jailed(&self.root, path, "edit_file")?;
        let meta = std::fs::metadata(&target)
            .map_err(|e| format!("edit_file: stat {}: {e}", target.display()))?;
        if meta.is_dir() {
            return Err(format!("edit_file: is a directory: {path}"));
        }
        if meta.len() > MAX_EDIT_BYTES {
            return Err(format!(
                "edit_file: {} is {} bytes (max {})",
                path,
                meta.len(),
                MAX_EDIT_BYTES
            ));
        }
        let original = std::fs::read_to_string(&target)
            .map_err(|e| format!("edit_file: read {}: {e}", target.display()))?;
        // Hash anchor: refuse stale context BEFORE looking at old_text.
        let live_hash = content_hash(&original);
        if live_hash != old_hash {
            return Err(format!(
                "edit_file: stale anchor for {path}: content hash is now {live_hash}, you quoted {old_hash}. \
                 The file changed since you read it. Re-read the file with read_file, quote its fresh hash, and retry."
            ));
        }
        // Exact-match count: 0 or 2+ are actionable errors.
        let matches = original.matches(old_text).count();
        match matches {
            0 => Err(format!(
                "edit_file: old_text not found in {path}. It may have moved or changed — re-read the file and retry with the exact current text."
            )),
            1 => {
                let updated = original.replacen(old_text, new_text, 1);
                atomic_write(&target, &updated)?;
                Ok(json!({
                    "path": path,
                    "new_hash": content_hash(&updated),
                    "bytes": updated.len()
                }))
            }
            n => Err(format!(
                "edit_file: old_text matches {n} times in {path}. Make it unique (include surrounding lines) and retry."
            )),
        }
    }
}

/// Delete a file under a fixed root directory. Input:
/// `{ "path": "rel" }`. Destructive risk: deletion is irreversible, so
/// the registry only accepts it behind an interactive approver and the
/// approver prompts on every call — allowlists and --yes never apply.
#[derive(Debug, Clone)]
pub struct DeleteFileTool {
    root: PathBuf,
}

impl DeleteFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn risk(&self) -> Risk {
        Risk::Destructive
    }

    fn describe(&self, input: &Value) -> String {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing path>");
        format!("delete file {path}")
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path (no .., no absolute paths). Files only." }
            },
            "required": ["path"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Err("delete_file: missing 'path'".into());
        };
        let target = jailed(&self.root, path, "delete_file")?;
        let meta = std::fs::symlink_metadata(&target)
            .map_err(|e| format!("delete_file: stat {}: {e}", target.display()))?;
        if meta.is_dir() {
            return Err(format!(
                "delete_file: is a directory: {path} (directories are out of scope)"
            ));
        }
        std::fs::remove_file(&target)
            .map_err(|e| format!("delete_file: {}: {e}", target.display()))?;
        Ok(json!({ "path": path, "deleted": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("tole-file-tools-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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
        let t = EditFileTool::new(dir.clone());
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
        let t = EditFileTool::new(dir.clone());
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
        let t = EditFileTool::new(dir.clone());
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
        let t = EditFileTool::new(dir.clone());
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
        let t = EditFileTool::new(dir.clone());
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
        let t = EditFileTool::new(dir.clone());
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
        let t = EditFileTool::new(dir.clone());
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
        let t = DeleteFileTool::new(dir.clone());
        let out = t.execute(json!({ "path": "scratch.txt" })).unwrap();
        assert_eq!(out["deleted"], json!(true));
        assert!(!dir.join("scratch.txt").exists());
    }

    #[test]
    fn delete_refuses_directories_and_escapes() {
        let dir = tmpdir("del-dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let t = DeleteFileTool::new(dir.clone());
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
        let t = DeleteFileTool::new(dir.clone());
        let err = t.execute(json!({ "path": "ghost.txt" })).unwrap_err();
        assert!(
            err.contains("not found"),
            "error should say not found: {err}"
        );
    }
}
