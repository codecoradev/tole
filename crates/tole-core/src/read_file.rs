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

/// Head cap (characters) on the content returned per call — the same
/// 20,000-char bound git/web apply. Larger files are read in chunks via
/// `offset`/`limit` and the `next_offset` continuation (#211).
pub(crate) const MAX_READ_CHARS: usize = 20_000;

/// Optional non-negative integer parameter (absent/null = `None`).
fn opt_uint(input: &Value, key: &str) -> Result<Option<u64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("read_file: '{key}' must be a non-negative integer")),
    }
}

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
        // Component-based validation (uniform with the other file tools;
        // CodeCora scan #6/#12): drive-prefixed/rooted Windows relatives
        // are NOT is_absolute() yet escape via join; a substring ".." both
        // misses them and rejects benign names. Require all-normal
        // components.
        use std::path::Component;
        let all_normal = rel_path
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
        if !all_normal {
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

    fn summary(&self) -> String {
        "Read a text file inside the workspace. Returns at most 20000 chars; if `truncated`, \
         continue with offset=`next_offset` (offset/limit are in chars)."
            .into()
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
                "path": { "type": "string", "description": "Workspace-relative file path (no .., no absolute paths)" },
                "offset": { "type": "integer", "minimum": 0, "description": "Start char (default 0)" },
                "limit": { "type": "integer", "minimum": 1, "description": "Max chars (default/max 20000)" }
            },
            "required": ["path"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return Err("read_file: missing 'path'".into());
        };
        let offset = opt_uint(&input, "offset")?;
        let limit = opt_uint(&input, "limit")?;
        if limit == Some(0) {
            return Err("read_file: 'limit' must be at least 1".into());
        }
        let target = self.jailed(path)?;
        let meta = std::fs::metadata(&target)
            .map_err(|e| format!("read_file: stat {}: {e}", target.display()))?;
        if meta.is_dir() {
            return Err(format!("read_file: is a directory: {path}"));
        }
        // Regular files ONLY (cora full-scan #21): FIFOs block on open
        // until a writer appears, sockets/devices error opaquely — and a
        // FIFO reports len() == 0 so the size cap below never fires. A
        // hang inside a ReadOnly tool freezes the whole turn loop.
        if !meta.is_file() {
            return Err(format!(
                "read_file: not a regular file (refusing to read): {path}"
            ));
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
        // Content hash (E12): the model quotes this in `edit_file.old_hash`
        // so stale anchors are refused before they corrupt a file.
        let hash = crate::file_tools::content_hash(&content);
        // Slice in CHARACTERS on the decoded text (boundary-safe: byte
        // positions come from `char_indices`, never from arithmetic).
        // `hash`/`bytes` always describe the WHOLE file.
        let total_chars = content.chars().count();
        let offset = usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX);
        let limit = usize::try_from(limit.unwrap_or(MAX_READ_CHARS as u64))
            .unwrap_or(MAX_READ_CHARS)
            .min(MAX_READ_CHARS);
        if offset > 0 && offset >= total_chars {
            return Ok(json!({
                "path": path, "content": "", "bytes": bytes.len(), "hash": hash,
                "truncated": false, "total_chars": total_chars,
                "note": format!("offset {offset} is at or beyond the end ({total_chars} chars)")
            }));
        }
        let byte_at = |n: usize| {
            content
                .char_indices()
                .nth(n)
                .map_or(content.len(), |(b, _)| b)
        };
        let (start, end) = (byte_at(offset), byte_at(offset + limit));
        let end_chars = (offset + limit).min(total_chars);
        let truncated = end_chars < total_chars;
        let mut out = json!({
            "path": path, "content": &content[start..end], "bytes": bytes.len(), "hash": hash,
            "truncated": truncated, "total_chars": total_chars
        });
        if truncated {
            out["next_offset"] = json!(end_chars);
        }
        Ok(out)
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

    fn read(t: &ReadFileTool, input: Value) -> Value {
        t.execute(input).unwrap()
    }

    fn file_tool(name: &str, content: &str) -> ReadFileTool {
        let dir = tmpdir(name);
        std::fs::write(dir.join("f.txt"), content).unwrap();
        ReadFileTool::new(dir)
    }

    #[test]
    fn small_file_reports_untruncated_and_total() {
        let t = file_tool("small", "hello");
        let out = read(&t, json!({ "path": "f.txt" }));
        assert_eq!(out["content"], json!("hello"));
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["total_chars"], json!(5));
        assert!(out.get("next_offset").is_none());
        assert_eq!(
            out["hash"],
            json!(crate::file_tools::content_hash("hello")),
            "hash stays the whole-file hash"
        );
    }

    #[test]
    fn exactly_cap_is_not_truncated() {
        let t = file_tool("exact", &"a".repeat(MAX_READ_CHARS));
        let out = read(&t, json!({ "path": "f.txt" }));
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["total_chars"], json!(MAX_READ_CHARS));
        assert_eq!(out["content"].as_str().unwrap().len(), MAX_READ_CHARS);
        assert!(out.get("next_offset").is_none());
    }

    #[test]
    fn one_over_cap_is_truncated_with_next_offset() {
        let t = file_tool("over", &"a".repeat(MAX_READ_CHARS + 1));
        let out = read(&t, json!({ "path": "f.txt" }));
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["total_chars"], json!(MAX_READ_CHARS + 1));
        assert_eq!(out["next_offset"], json!(MAX_READ_CHARS));
        assert_eq!(
            out["content"].as_str().unwrap().chars().count(),
            MAX_READ_CHARS
        );
        let rest = read(&t, json!({ "path": "f.txt", "offset": MAX_READ_CHARS }));
        assert_eq!(rest["content"], json!("a"));
        assert_eq!(rest["truncated"], json!(false));
    }

    #[test]
    fn multibyte_chunks_reassemble_exactly() {
        // 1-, 2-, 3- and 4-byte chars, mixed so every boundary kind occurs.
        let unit = "aé日本語😀x";
        let text: String = unit.repeat(40);
        let t = file_tool("multibyte", &text);
        for limit in [1usize, 2, 3, 7, 13, 1000] {
            let mut out = String::new();
            let mut offset = 0u64;
            loop {
                let r = read(
                    &t,
                    json!({ "path": "f.txt", "offset": offset, "limit": limit }),
                );
                let chunk = r["content"].as_str().unwrap();
                assert!(chunk.chars().count() <= limit);
                out.push_str(chunk);
                if r["truncated"] == json!(true) {
                    offset = r["next_offset"].as_u64().unwrap();
                } else {
                    break;
                }
            }
            assert_eq!(out, text, "limit {limit}");
        }
    }

    #[test]
    fn offset_is_in_chars_not_bytes() {
        let t = file_tool("charoff", "é日😀z");
        let r = read(&t, json!({ "path": "f.txt", "offset": 2, "limit": 2 }));
        assert_eq!(r["content"], json!("😀z"));
        assert_eq!(r["bytes"], json!("é日😀z".len()));
        assert_eq!(r["total_chars"], json!(4));
    }

    #[test]
    fn offset_beyond_end_is_empty_not_error() {
        let t = file_tool("beyond", "abc");
        for off in [3u64, 4, 1_000_000] {
            let r = read(&t, json!({ "path": "f.txt", "offset": off }));
            assert_eq!(r["content"], json!(""));
            assert_eq!(r["truncated"], json!(false));
            assert_eq!(r["total_chars"], json!(3));
            assert!(r["note"].as_str().unwrap().contains("beyond"));
        }
    }

    #[test]
    fn limit_is_clamped_to_default_max() {
        let t = file_tool("clamp", &"b".repeat(MAX_READ_CHARS + 500));
        let r = read(&t, json!({ "path": "f.txt", "limit": 1_000_000 }));
        assert_eq!(r["content"].as_str().unwrap().len(), MAX_READ_CHARS);
        assert_eq!(r["truncated"], json!(true));
        assert_eq!(r["next_offset"], json!(MAX_READ_CHARS));
        let r = read(&t, json!({ "path": "f.txt", "limit": 10 }));
        assert_eq!(r["content"], json!("b".repeat(10)));
        assert_eq!(r["next_offset"], json!(10));
    }

    #[test]
    fn invalid_offset_or_limit_is_a_tool_error() {
        let t = file_tool("badargs", "abc");
        for bad in [json!(-1), json!(1.5), json!("2"), json!(true), json!([1])] {
            for key in ["offset", "limit"] {
                let err = t
                    .execute(json!({ "path": "f.txt", key: bad.clone() }))
                    .unwrap_err();
                assert!(err.contains(key) && err.contains("non-negative"), "{err}");
            }
        }
        let err = t
            .execute(json!({ "path": "f.txt", "limit": 0 }))
            .unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn crlf_is_preserved_and_empty_file_is_fine() {
        let t = file_tool("crlf", "a\r\nb\r\n");
        let r = read(&t, json!({ "path": "f.txt" }));
        assert_eq!(r["content"], json!("a\r\nb\r\n"));
        assert_eq!(r["total_chars"], json!(6));
        let r = read(&t, json!({ "path": "f.txt", "offset": 1, "limit": 2 }));
        assert_eq!(r["content"], json!("\r\n"));
        let e = file_tool("empty", "");
        for input in [
            json!({ "path": "f.txt" }),
            json!({ "path": "f.txt", "offset": 0 }),
        ] {
            let r = read(&e, input);
            assert_eq!(r["content"], json!(""));
            assert_eq!(r["total_chars"], json!(0));
            assert_eq!(r["truncated"], json!(false));
        }
    }

    #[test]
    fn spec_declares_offset_and_limit() {
        let t = ReadFileTool::new(PathBuf::from("."));
        let spec = t.spec().unwrap();
        assert!(spec["properties"]["offset"].is_object());
        assert!(spec["properties"]["limit"].is_object());
        assert_eq!(spec["required"], json!(["path"]));
        assert!(t.summary().contains("next_offset"));
    }

    #[test]
    fn rejects_directory_read() {
        let dir = tmpdir("dir");
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        let t = ReadFileTool::new(dir);
        let err = t.execute(json!({ "path": "subdir" })).unwrap_err();
        assert!(err.contains("directory"), "got: {err}");
    }

    #[test]
    fn rejects_non_regular_files_instead_of_hanging() {
        // FIFO regression (cora full-scan #21): a named pipe reports
        // len() == 0 (size cap never fires) and open() BLOCKS until a
        // writer appears — a hang inside a ReadOnly tool freezes the
        // turn loop. execute() must refuse cleanly, before opening.
        // The whole test runs under a hard timeout: a regression turns
        // into a test timeout, not an infinite CI hang.
        let dir = tmpdir("fifo");
        let t = ReadFileTool::new(dir.clone());
        let fifo = dir.join("pipe");
        let created = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !created {
            // Platform without mkfifo in PATH (e.g. Windows runners):
            // nothing to assert here.
            eprintln!("mkfifo unavailable — skipping FIFO test");
            return;
        }
        // If execute() ever opens the FIFO, this join blocks forever and
        // the test suite times out — the failure mode IS the assertion.
        let handle = std::thread::spawn(move || t.execute(json!({ "path": "pipe" })));
        match handle.join() {
            Ok(Ok(_)) => panic!("reading a FIFO must fail, not succeed"),
            Ok(Err(e)) => assert!(
                e.contains("not a regular file"),
                "expected regular-file refusal, got: {e}"
            ),
            Err(_) => panic!("read_file worker thread panicked"),
        }
    }
}
