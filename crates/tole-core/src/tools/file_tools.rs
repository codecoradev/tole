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

use crate::action_summary::{diff_lines, refused, ActionEffect, ActionKind, ActionSummary};
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

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

/// The path to check as a workspace-relative one (#1285). A relative `path`
/// is returned as given. An absolute one only when it lies under `root` (as
/// given or canonical, compared as text, so no file is touched and nothing
/// outside the workspace can be probed); what remains after the root is then
/// held to the same rules as any relative path (no `..`, symlinks resolved).
/// `None`: not inside the workspace, or the workspace root itself.
pub fn within_workspace(root: &Path, path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let canon = root.canonicalize().ok();
    let rest = [Some(root), canon.as_deref()]
        .into_iter()
        .flatten()
        .find_map(|base| {
            p.strip_prefix(base)
                .ok()
                .filter(|r| r.components().next().is_some())
        });
    rest.map(Path::to_path_buf)
}

/// The refusal text of a jail check, shared with `read_file` and `write_file`
/// (#1285): a path that names no place inside the workspace says where the
/// workspace is and how to write the path; `..` and a target that resolves
/// outside `root` stay "escapes the jail". Both name the workspace.
pub fn refusal(tool: &str, root: &Path, rel: &str) -> String {
    let w = root.display();
    let p = Path::new(rel);
    let rooted = matches!(
        p.components().next(),
        Some(Component::RootDir | Component::Prefix(_))
    );
    if rooted && (!p.is_absolute() || within_workspace(root, rel).is_none()) {
        format!("{tool}: path is not inside the workspace ({w}): {rel} (use a path relative to {w}, or an absolute path under it; --workspace <dir> widens it)")
    } else {
        format!("{tool}: path escapes the jail: {rel} (use a path inside the workspace {w}; --workspace <dir> widens it)")
    }
}

/// Resolve `rel` inside the jail. Shared shape with `WriteFileTool` /
/// `ReadFileTool`: `rel` is relative, or absolute under `root`; no `..`, and
/// the resolved target (through symlinks) must stay inside `root`.
fn jailed(root: &Path, rel: &str, tool: &str) -> Result<PathBuf, String> {
    let rel_path = within_workspace(root, rel).ok_or_else(|| refusal(tool, root, rel))?;
    // Component-based validation (uniform with WriteFileTool; CodeCora
    // scan #6/#12): on Windows, drive-prefixed and rooted relatives are
    // NOT `is_absolute()` yet `Path::join` lets both escape the base, and
    // a substring `..` check falsely rejects benign names containing "..".
    // Require every component to be a normal name.
    let all_normal = rel_path
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if !all_normal {
        return Err(refusal(tool, root, rel));
    }
    let target = root.join(&rel_path);
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
        return Err(refusal(tool, root, rel));
    }
    Ok(canon_target)
}

/// [`jailed`] for `read_file`, which shares the jail.
pub(super) fn resolve_in_jail(root: &Path, rel: &str, tool: &str) -> Result<PathBuf, String> {
    jailed(root, rel, tool)
}

/// The `path` input resolved like [`jailed`], relative to the canonical root
/// and `/`-separated; `None` when it does not resolve inside the jail.
fn jailed_rel(root: &Path, input: &Value, tool: &str) -> Option<String> {
    let target = jailed(root, input.get("path")?.as_str()?, tool).ok()?;
    let root = root.canonicalize().ok()?;
    let rel = target.strip_prefix(&root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    let parts = parts?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// What an approval summary names: the path `execute` will really touch,
/// resolved by the same [`jailed`] call and shown relative to the root (lossy
/// for a name that is not UTF-8, which is still a path `execute` accepts).
/// `Err` carries the path as sent, only when `jailed` itself refuses it.
fn summary_target(root: &Path, input: &Value, tool: &str) -> Result<String, String> {
    let raw = input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("<missing path>");
    let target = jailed(root, raw, tool).map_err(|_| raw.to_string())?;
    let canon_root = root.canonicalize().map_err(|_| raw.to_string())?;
    Ok(crate::action_summary::relative_lossy(&target, &canon_root)
        .unwrap_or_else(|| raw.to_string()))
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
    // Legacy stale sweep: crashed attempts of the OLD pid-only naming
    // (`.tole-edit-{pid}.tmp`) are removed up front. A concurrent
    // nanos-unique temp can never match this exact name, so the sweep
    // cannot delete a live writer's file (CodeCora scan 2026-09-18).
    let _ = std::fs::remove_file(parent.join(format!(".tole-edit-{}.tmp", std::process::id())));
    // New names are unique per attempt (pid + subsec-nanos): create_new
    // can therefore never collide with a concurrent same-pid writer, and
    // the on-collision self-heal below only ever fires for a genuinely
    // stale exact match.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".tole-edit-{}-{nanos}.tmp", std::process::id()));
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
    // Preserve the TARGET's permissions across the atomic rename (cora
    // scan-3 #60): the temp file was created with default permissions,
    // so a rename-over would silently drop exec bits and restrictive
    // modes (e.g. 0600 secrets) from the resulting inode.
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(target) {
        let perms = meta.permissions();
        if let Err(e) = std::fs::set_permissions(&tmp, perms) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "edit_file: preserve permissions on {}: {e}",
                tmp.display()
            ));
        }
    }
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

    fn summary(&self) -> String {
        "Edit a workspace file by replacing an exact text snippet with new text.".into()
    }

    fn describe(&self, input: &Value) -> String {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing path>");
        // The approver approves THIS string: show the actual change, not
        // just the file name (CodeCora scan 2026-09-18). Bounded previews
        // keep the approval line readable.
        fn preview(v: Option<&Value>, max: usize) -> String {
            let t = v
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .replace('\n', "\\n");
            let mut out: String = t.chars().take(max).collect();
            if t.chars().count() > max {
                out.push('…');
            }
            out
        }
        format!(
            "edit file {path}: {:?} -> {:?}",
            preview(input.get("old_text"), 80),
            preview(input.get("new_text"), 80)
        )
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path (no ..; absolute only inside the workspace)" },
                "old_hash": { "type": "string", "description": "Content hash (16-hex) returned by read_file when you read this file. Refused if the file changed since." },
                "old_text": { "type": "string", "description": "Exact text to replace. Must match exactly once in the file." },
                "new_text": { "type": "string", "description": "Replacement text. Empty string deletes the matched text." }
            },
            "required": ["path", "old_hash", "old_text", "new_text"]
        }))
    }

    /// The resolved target relative to the canonical root (#437): the
    /// same [`jailed`] resolution `execute` uses, symlinks followed.
    fn approval_target(&self, input: &Value) -> Option<String> {
        jailed_rel(&self.root, input, "edit_file")
    }

    fn action_summary(&self, input: &Value) -> Option<ActionSummary> {
        let target = match summary_target(&self.root, input, "edit_file") {
            Ok(t) => t,
            Err(raw) => {
                return Some(refused(
                    "edit_file",
                    "refused or not found: the path does not resolve to an existing file inside the working folder",
                    &raw,
                ))
            }
        };
        let text = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
        Some(
            ActionSummary::new(
                ActionKind::ReplaceFile,
                target,
                ActionEffect::ReplacesNoBackup,
                true,
            )
            .with_preview(diff_lines(text("old_text"), text("new_text"))),
        )
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

    fn summary(&self) -> String {
        "Delete one file inside the workspace.".into()
    }

    fn describe(&self, input: &Value) -> String {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing path>");
        format!("delete file {path}")
    }

    fn action_summary(&self, input: &Value) -> Option<ActionSummary> {
        let target = match summary_target(&self.root, input, "delete_file") {
            Ok(t) => t,
            Err(raw) => {
                return Some(refused(
                    "delete_file",
                    "refused or not found: the path does not resolve to an existing file inside the working folder",
                    &raw,
                ))
            }
        };
        let mut summary = ActionSummary::new(
            ActionKind::DeleteFile,
            &target,
            ActionEffect::CannotBeUndone,
            true,
        );
        // `execute` removes the LINK when the path is a symlink, not what it
        // points to: name the path as sent and say so, instead of showing the
        // canonical referent.
        let sent = input.get("path").and_then(Value::as_str);
        if let Some(sent) = sent {
            let is_link = std::fs::symlink_metadata(self.root.join(sent))
                .is_ok_and(|m| m.file_type().is_symlink());
            if is_link {
                summary = ActionSummary::new(
                    ActionKind::DeleteFile,
                    sent,
                    ActionEffect::CannotBeUndone,
                    true,
                )
                .with_preview([format!(
                    "this path is a symbolic link to {target}: only the link is removed, not the file it points to"
                )]);
            }
        }
        Some(summary)
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path (no ..; absolute only inside the workspace). Files only." }
            },
            "required": ["path"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Err("delete_file: missing 'path'".into());
        };
        let target = jailed(&self.root, path, "delete_file")?;
        // The jail canonicalizes, which FOLLOWS a symlink: deleting
        // `target` would remove the referent and leave a dangling link in
        // the workspace. Detect a symlinked path via the pre-canonical
        // location and remove the LINK itself instead (CodeCora scan
        // 2026-09-18).
        let direct = self.root.join(path);
        let meta = std::fs::symlink_metadata(&direct)
            .map_err(|e| format!("delete_file: stat {}: {e}", target.display()))?;
        if meta.is_dir() {
            return Err(format!(
                "delete_file: is a directory: {path} (directories are out of scope)"
            ));
        }
        let is_symlink = meta.file_type().is_symlink();
        let victim = if is_symlink { &direct } else { &target };
        std::fs::remove_file(victim)
            .map_err(|e| format!("delete_file: {}: {e}", victim.display()))?;
        Ok(json!({ "path": path, "deleted": true, "symlink": is_symlink }))
    }
}
