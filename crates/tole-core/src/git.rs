//! `git` CLI tool (A2): light git operations — status, diff, add, commit.
//! `Risk::Write` for everything (conservative: even reads go through the
//! approver; per-op risk is a future refinement, see gh.rs note).
//!
//! Same argv-whitelist discipline as `gh.rs`: an op enum maps to a fixed
//! argv shape, every free-form value is validated (no leading dash, sane
//! charset), and anything unexpected is refused *before* spawning.
//! `push` and everything network/history-mutating is intentionally NOT
//! whitelisted — push stays a human action.

use crate::subprocess::run_with_timeout;
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Git ops may trigger hooks (pre-commit review bots are common on this
/// host's global template). LLM-backed hooks routinely exceed the 30s
/// generic subprocess ceiling, so git gets its own budget.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Allowed `git` subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOp {
    Status,
    Diff,
    Add,
    Commit,
}

impl GitOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "status" => Some(GitOp::Status),
            "diff" => Some(GitOp::Diff),
            "add" => Some(GitOp::Add),
            "commit" => Some(GitOp::Commit),
            _ => None,
        }
    }

    /// Build the argv after `git`. Free-form values validated:
    /// - paths: non-empty, no leading dash (a path arg like `--exec`
    ///   could redirect git behavior)
    /// - message: non-empty, no leading dash (git reads it as a value,
    ///   but defense in depth)
    fn argv(&self, input: &Value) -> Result<Vec<String>, String> {
        fn opt_str<'a>(input: &'a Value, k: &str) -> Option<&'a str> {
            input
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        }
        fn no_dash(k: &str, v: &str) -> Result<String, String> {
            if v.starts_with('-') {
                return Err(format!("git: '{k}' must not start with '-': {v:?}"));
            }
            Ok(v.to_string())
        }
        match self {
            GitOp::Status => Ok(vec!["status".into(), "--short".into(), "--branch".into()]),
            GitOp::Diff => {
                // --staged only when explicitly asked; default = worktree diff.
                let mut argv = vec!["diff".to_string()];
                if input.get("staged").and_then(Value::as_bool) == Some(true) {
                    argv.push("--staged".into());
                }
                Ok(argv)
            }
            GitOp::Add => {
                // Paths must stay inside the workdir subtree: no absolute
                // paths, no `..` components (git would happily stage them
                // when the repository root sits above the workdir, e.g. a
                // subdirectory of a monorepo). This is the jail the file
                // tools enforce via canonicalize; for add, lexical
                // rejection is airtight because git resolves the path
                // itself afterwards.
                let mut paths = Vec::new();
                let Some(arr) = input.get("paths").and_then(Value::as_array) else {
                    return Err("git: 'paths' array is required for add".into());
                };
                if arr.is_empty() {
                    return Err("git: 'paths' must not be empty".into());
                }
                for p in arr {
                    let Some(p) = p.as_str().map(str::trim).filter(|v| !v.is_empty()) else {
                        return Err("git: every path must be a non-empty string".into());
                    };
                    if p.starts_with('/') || p.starts_with('\\') {
                        return Err(format!(
                            "git: 'path' must be relative to the workspace: {p:?}"
                        ));
                    }
                    if p.split(['/', '\\']).any(|c| c == "..") {
                        return Err(format!(
                            "git: 'path' must not escape the workspace (..): {p:?}"
                        ));
                    }
                    paths.push(no_dash("path", p)?);
                }
                // The one sanctioned catch-all: the whole tree, same as
                // a human typing `git add .` in the repo root.
                if paths.len() == 1 && paths[0] == "." {
                    return Ok(vec!["add".into(), ".".into()]);
                }
                let mut argv = vec!["add".to_string(), "--".into()];
                argv.extend(paths);
                Ok(argv)
            }
            GitOp::Commit => {
                let msg = opt_str(input, "message")
                    .ok_or_else(|| "git: missing or empty 'message'".to_string())?;
                if msg.len() > 500 {
                    return Err("git: 'message' too long (max 500 chars)".into());
                }
                let mut argv = vec!["commit".to_string(), "--".into()];
                // `--` then a `-m` would be parsed as a path. Instead we
                // validate and pass the message after -m BEFORE any path
                // separator — rebuild: commit -m MSG [-- paths...]
                argv.clear();
                argv.push("commit".into());
                argv.push("-m".into());
                argv.push(no_dash("message", msg)?);
                Ok(argv)
            }
        }
    }
}

/// Light git tool. cwd = the repo checkout (jail root for paths is the
/// same cwd the file tools use).
#[derive(Debug, Clone)]
pub struct GitTool {
    pub bin: PathBuf,
    pub workdir: Option<PathBuf>,
}

impl GitTool {
    pub fn new() -> Self {
        Self {
            bin: PathBuf::from("git"),
            workdir: None,
        }
    }

    pub fn with_bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.bin = bin.into();
        self
    }

    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(dir.into());
        self
    }

    pub fn command_line(&self, input: &Value) -> Result<String, String> {
        let op = parse_op(input)?;
        let argv = op.argv(input)?;
        let mut parts = vec![self.bin.display().to_string()];
        parts.extend(argv.iter().map(|a| shquote(a)));
        Ok(parts.join(" "))
    }
}

fn parse_op(input: &Value) -> Result<GitOp, String> {
    input
        .get("op")
        .and_then(Value::as_str)
        .and_then(GitOp::parse)
        .ok_or_else(|| "git: 'op' must be one of status | diff | add | commit".to_string())
}

impl Default for GitTool {
    git_default!();
}

impl Tool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        self.command_line(input).unwrap_or_else(|e| e)
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["status", "diff", "add", "commit"],
                    "description": "Light git operation. push is NOT available — a human pushes."
                },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Paths for add (relative to repo root; use [\".\"] for all changes)" },
                "message": { "type": "string", "description": "Commit message (required for commit, max 500 chars)" },
                "staged": { "type": "boolean", "description": "diff --staged when true (default false)" }
            },
            "required": ["op"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let op = parse_op(&input)?;
        let argv = op.argv(&input)?;
        let mut cmd = Command::new(&self.bin);
        cmd.args(&argv);
        if let Some(dir) = &self.workdir {
            cmd.current_dir(dir);
        }
        let out = run_with_timeout(&mut cmd, GIT_TIMEOUT)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let snippet: String = stderr.chars().take(500).collect();
            return Err(format!("git exited {}: {}", out.status, snippet));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(json!({
            "stdout": stdout.trim(),
        }))
    }
}

/// Minimal shlex-style quoting for the audit string (same rules as gh.rs).
fn shquote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// The macro trick avoids a Default impl that silently diverges from new().
macro_rules! git_default {
    () => {
        fn default() -> Self {
            Self::new()
        }
    };
}
use git_default;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classified_write() {
        assert_eq!(GitTool::new().risk(), Risk::Write);
    }

    #[test]
    fn status_argv() {
        let t = GitTool::new();
        assert_eq!(
            t.command_line(&json!({"op":"status"})).unwrap(),
            "git status --short --branch"
        );
    }

    #[test]
    fn diff_argv() {
        let t = GitTool::new();
        assert_eq!(t.command_line(&json!({"op":"diff"})).unwrap(), "git diff");
        assert_eq!(
            t.command_line(&json!({"op":"diff","staged":true})).unwrap(),
            "git diff --staged"
        );
    }

    #[test]
    fn add_argv() {
        let t = GitTool::new();
        assert_eq!(
            t.command_line(&json!({"op":"add","paths":["."]})).unwrap(),
            "git add ."
        );
        assert_eq!(
            t.command_line(&json!({"op":"add","paths":["a.txt","src/b.rs"]}))
                .unwrap(),
            "git add -- a.txt src/b.rs"
        );
    }

    #[test]
    fn commit_argv() {
        let t = GitTool::new();
        assert_eq!(
            t.command_line(&json!({"op":"commit","message":"feat: x"}))
                .unwrap(),
            "git commit -m 'feat: x'"
        );
    }

    #[test]
    fn rejects_injection() {
        let t = GitTool::new();
        // unknown op
        assert!(t.command_line(&json!({"op":"push"})).is_err());
        assert!(t.command_line(&json!({"op":"remote"})).is_err());
        // leading-dash path
        assert!(t
            .command_line(&json!({"op":"add","paths":["--exec=/bin/sh"]}))
            .is_err());
        // jail escapes: absolute + parent traversal
        assert!(t
            .command_line(&json!({"op":"add","paths":["/etc/passwd"]}))
            .is_err());
        assert!(t
            .command_line(&json!({"op":"add","paths":["../sibling/file"]}))
            .is_err());
        assert!(t
            .command_line(&json!({"op":"add","paths":["safe/../../escape"]}))
            .is_err());
        // interior .. still caught after normalization by component check
        assert!(t
            .command_line(&json!({"op":"add","paths":["a/../b"]}))
            .is_err());
        // leading-dash message
        assert!(t
            .command_line(&json!({"op":"commit","message":"--amend"}))
            .is_err());
        // missing fields
        assert!(t.command_line(&json!({"op":"add"})).is_err());
        assert!(t.command_line(&json!({"op":"commit"})).is_err());
        // empty paths
        assert!(t.command_line(&json!({"op":"add","paths":[]})).is_err());
        // overlong message
        assert!(t
            .command_line(&json!({"op":"commit","message":"x".repeat(501)}))
            .is_err());
    }

    #[test]
    fn fake_git_binary_end_to_end() {
        let dir = std::env::temp_dir().join(format!("tole-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("git");
        let log = dir.join("argv.log");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho \"$@\" > {}\necho ok\n", log.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let t = GitTool::new().with_bin(&bin);
        let out = t
            .execute(json!({"op":"commit","message":"feat: x"}))
            .unwrap();
        assert_eq!(out["stdout"], json!("ok"));
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert_eq!(recorded.trim(), "commit -m feat: x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
