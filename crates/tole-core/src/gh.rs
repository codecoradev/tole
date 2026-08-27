//! `gh` CLI tool (E7): GitHub operations — issue comment, issue create,
//! PR create. `Risk::Write`: every call goes through the Approver (E6).
//!
//! The binary path is injectable so tests drive it with a fake `gh`
//! (shell script) instead of the real CLI — no network, deterministic.

use crate::subprocess::{run_with_timeout, SUBPROCESS_TIMEOUT};
use crate::tool::{Risk, Tool};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

/// Allowed `gh` subcommands. Whitelist, not blacklist: anything not
/// listed is refused *before* spawning a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhOp {
    IssueComment,
    IssueCreate,
    PrCreate,
}

impl GhOp {
    /// Parse from the input's `op` field. Unknown/missing → `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "issue_comment" => Some(GhOp::IssueComment),
            "issue_create" => Some(GhOp::IssueCreate),
            "pr_create" => Some(GhOp::PrCreate),
            _ => None,
        }
    }

    /// The argv (after `gh`) for this op, given the JSON input.
    fn argv(&self, input: &Value) -> Result<Vec<String>, String> {
        let s = |k: &str| -> Result<String, String> {
            input
                .get(k)
                .and_then(Value::as_str)
                .map(|v| v.to_string())
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| format!("gh: missing or empty '{k}'"))
        };
        /// Positional and flag-adjacent values must never look like gh
        /// flags: a model-supplied `number` or `base` of `--repo` would
        /// redirect the command (argument injection past the whitelist).
        /// Values passed as option payloads (`--body X`) are consumed by
        /// clap as plain values, but we still reject a leading dash for
        /// defense in depth.
        fn no_flags(k: &str, v: &str) -> Result<String, String> {
            if v.starts_with('-') {
                return Err(format!("gh: '{k}' must not start with '-': {v:?}"));
            }
            Ok(v.to_string())
        }
        match self {
            GhOp::IssueComment => {
                // Issue numbers are digits by definition.
                let number = s("number")?;
                if !number.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!(
                        "gh: 'number' must be an issue number, got {number:?}"
                    ));
                }
                Ok(vec![
                    "issue".into(),
                    "comment".into(),
                    number,
                    "--body".into(),
                    s("body")?,
                ])
            }
            GhOp::IssueCreate => Ok(vec![
                "issue".into(),
                "create".into(),
                "--title".into(),
                no_flags("title", &s("title")?)?,
                "--body".into(),
                s("body")?,
            ]),
            GhOp::PrCreate => Ok(vec![
                "pr".into(),
                "create".into(),
                "--title".into(),
                no_flags("title", &s("title")?)?,
                "--body".into(),
                s("body")?,
                "--base".into(),
                // Branch names: alphanumeric plus - _ . / — nothing else,
                // and never a leading dash.
                {
                    let base = s("base")?;
                    if !base
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
                    {
                        return Err(format!("gh: 'base' is not a valid branch name: {base:?}"));
                    }
                    no_flags("base", &base)?
                },
            ]),
        }
    }
}

/// GitHub operations via the `gh` CLI. Every invocation is Write risk →
/// the approval gate shows exactly what will run before it runs.
#[derive(Debug, Clone)]
pub struct GhTool {
    /// Path to the `gh` binary (defaults to `gh` on PATH); injectable
    /// for tests.
    pub bin: PathBuf,
    /// Fixed `--repo` so the model can't redirect writes elsewhere.
    pub repo: String,
    /// Working directory for the command (repo checkout).
    pub workdir: Option<PathBuf>,
}

impl GhTool {
    pub fn new(repo: impl Into<String>) -> Self {
        Self {
            bin: PathBuf::from("gh"),
            repo: repo.into(),
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

    /// The exact command line this tool would run — used in the approval
    /// prompt so the human sees precisely what executes.
    pub fn command_line(&self, input: &Value) -> Result<String, String> {
        let op = input
            .get("op")
            .and_then(Value::as_str)
            .and_then(GhOp::parse)
            .ok_or_else(|| {
                "gh: 'op' must be one of issue_comment | issue_create | pr_create".to_string()
            })?;
        let mut argv = op.argv(input)?;
        argv.push("--repo".into());
        argv.push(self.repo.clone());
        let quoted: Vec<String> = argv.iter().map(|a| shlex_quote(a)).collect();
        Ok(format!("{} {}", self.bin.display(), quoted.join(" ")))
    }
}

impl Tool for GhTool {
    fn name(&self) -> &str {
        "gh"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        self.command_line(input).unwrap_or_else(|e| e)
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let op = input
            .get("op")
            .and_then(Value::as_str)
            .and_then(GhOp::parse)
            .ok_or_else(|| {
                "gh: 'op' must be one of issue_comment | issue_create | pr_create".to_string()
            })?;
        let argv = op.argv(&input)?;
        let mut cmd = Command::new(&self.bin);
        cmd.args(&argv).arg("--repo").arg(&self.repo);
        if let Some(dir) = &self.workdir {
            cmd.current_dir(dir);
        }
        let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let snippet: String = stderr.chars().take(500).collect();
            return Err(format!("gh exited {}: {}", out.status, snippet));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(serde_json::json!({
            "stdout": stdout.trim(),
            "url": stdout.trim(),
        }))
    }
}

/// Minimal single-word shlex quoting for the audit string.
fn shlex_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classified_write() {
        let t = GhTool::new("codecoradev/tole");
        assert_eq!(t.risk(), Risk::Write);
    }

    #[test]
    fn command_line_shows_exact_invocation() {
        let t = GhTool::new("codecoradev/tole");
        let line = t
            .command_line(&json!({
                "op": "issue_comment",
                "number": "7",
                "body": "Fixed in abc123"
            }))
            .unwrap();
        assert_eq!(
            line,
            "gh issue comment 7 --body 'Fixed in abc123' --repo codecoradev/tole"
        );
    }

    #[test]
    fn rejects_unknown_op() {
        let t = GhTool::new("codecoradev/tole");
        assert!(t.command_line(&json!({ "op": "repo_delete" })).is_err());
        assert!(t.command_line(&json!({})).is_err());
    }

    #[test]
    fn rejects_missing_fields() {
        let t = GhTool::new("codecoradev/tole");
        // body missing
        assert!(t
            .command_line(&json!({ "op": "issue_comment", "number": "7" }))
            .is_err());
        // number missing
        assert!(t
            .command_line(&json!({ "op": "issue_comment", "body": "x" }))
            .is_err());
    }

    #[test]
    fn rejects_argument_injection() {
        let t = GhTool::new("codecoradev/tole");
        // number: must be digits — no flags, no repo redirects.
        assert!(t
            .command_line(&json!({
                "op": "issue_comment", "number": "--repo", "body": "x"
            }))
            .is_err());
        // base: branch-name charset only.
        assert!(t
            .command_line(&json!({
                "op": "pr_create", "title": "t", "body": "b", "base": "--flag"
            }))
            .is_err());
        assert!(t
            .command_line(&json!({
                "op": "pr_create", "title": "t", "body": "b", "base": "evil branch;rm"
            }))
            .is_err());
        // title: never a leading dash.
        assert!(t
            .command_line(&json!({
                "op": "issue_create", "title": "--web", "body": "b"
            }))
            .is_err());
        // body: payload of --body; clap consumes it as a value, but the
        // audit string still quotes anything dash-led.
        assert!(t
            .command_line(&json!({
                "op": "issue_comment", "number": "7", "body": "--repo x/y"
            }))
            .is_ok());
    }

    #[test]
    fn fake_gh_binary_end_to_end() {
        // Write a fake `gh` shell script that records its argv to a file
        // and exits 0. Proves the tool spawns *exactly* the whitelisted
        // argv — no shell interpolation surprises.
        let dir = std::env::temp_dir().join(format!("tole-gh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("gh");
        let log = dir.join("argv.log");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$@\" > {}\necho https://example/pr/1\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let t = GhTool::new("codecoradev/tole").with_bin(&bin);
        let out = t
            .execute(json!({
                "op": "pr_create",
                "title": "feat: x",
                "body": "body text",
                "base": "develop",
            }))
            .unwrap();
        assert_eq!(out["url"], json!("https://example/pr/1"));
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            recorded.trim(),
            "pr create --title feat: x --body body text --base develop --repo codecoradev/tole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
