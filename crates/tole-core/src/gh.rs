//! `gh` CLI tool (E7): GitHub operations — issue comment, issue create,
//! PR create. `Risk::Write`: every call goes through the Approver (E6).
//!
//! The binary path is injectable so tests drive it with a fake `gh`
//! (shell script) instead of the real CLI — no network, deterministic.

use crate::subprocess::{run_with_timeout, SUBPROCESS_TIMEOUT};
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

/// Allowed `gh` subcommands. Whitelist, not blacklist: anything not
/// listed is refused *before* spawning a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhOp {
    IssueComment,
    IssueCreate,
    PrCreate,
    IssueView,
    IssueList,
    PrView,
}

impl GhOp {
    /// Parse from the input's `op` field. Unknown/missing → `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "issue_comment" => Some(GhOp::IssueComment),
            "issue_create" => Some(GhOp::IssueCreate),
            "pr_create" => Some(GhOp::PrCreate),
            "issue_view" => Some(GhOp::IssueView),
            "issue_list" => Some(GhOp::IssueList),
            "pr_view" => Some(GhOp::PrView),
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
            GhOp::IssueView => {
                let number = digit_field(input, "issue number")?;
                Ok(vec![
                    "issue".into(),
                    "view".into(),
                    number,
                    "--json".into(),
                    ISSUE_VIEW_FIELDS.into(),
                ])
            }
            GhOp::IssueList => {
                // Optional state filter: only "open" / "closed" / "all",
                // whitelisted — never a free-form flag the model could
                // shape into something else.
                let mut argv = vec![
                    "issue".to_string(),
                    "list".to_string(),
                    "--json".into(),
                    ISSUE_LIST_FIELDS.into(),
                ];
                if let Some(state) = input.get("state").and_then(Value::as_str) {
                    match state {
                        "open" | "closed" | "all" => argv.push("--state".into()),
                        _ => {
                            return Err(format!(
                                "gh: 'state' must be one of open|closed|all, got {state:?}"
                            ))
                        }
                    }
                    // safety: matched against the whitelist above
                    argv.push(state.to_string());
                }
                if let Some(limit) = digit_field_opt(input, "limit")? {
                    argv.push("--limit".into());
                    argv.push(limit);
                }
                Ok(argv)
            }
            GhOp::PrView => {
                let number = digit_field(input, "PR number")?;
                Ok(vec![
                    "pr".into(),
                    "view".into(),
                    number,
                    "--json".into(),
                    PR_VIEW_FIELDS.into(),
                ])
            }
        }
    }
}

/// Fields fetched by issue_view — deliberately minimal: no author email,
/// no assignees' private data. Comments are fetched separately if needed
/// (a follow-up read op), keeping this payload bounded.
const ISSUE_VIEW_FIELDS: &str = "number,title,body,state,url,labels,createdAt,closedAt";

const ISSUE_LIST_FIELDS: &str = "number,title,state,url,updatedAt";

const PR_VIEW_FIELDS: &str =
    "number,title,body,state,url,baseRefName,headRefName,mergeable,statusCheckRollup";

/// Validate a required digit-only field (an issue/PR number).
fn digit_field(input: &Value, what: &str) -> Result<String, String> {
    let v = input
        .get("number")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("gh: missing or empty 'number' ({what})"))?;
    if !v.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "gh: 'number' must be a {what} (digits only), got {v:?}"
        ));
    }
    Ok(v.to_string())
}

/// Validate an optional digit-only field (e.g. `limit`).
fn digit_field_opt(input: &Value, name: &str) -> Result<Option<String>, String> {
    let Some(v) = input.get(name).and_then(Value::as_str) else {
        return Ok(None);
    };
    if v.trim().is_empty() {
        return Ok(None);
    }
    if !v.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("gh: '{name}' must be digits only, got {v:?}"));
    }
    // Cap: gh's own limit range is 1..=1000; 200 keeps payloads bounded.
    match v.parse::<u32>() {
        Ok(n) if (1..=200).contains(&n) => Ok(Some(n.to_string())),
        _ => Err(format!("gh: '{name}' must be within 1..=200, got {v:?}")),
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
                "gh: 'op' must be one of issue_view | issue_list | pr_view | issue_comment | issue_create | pr_create".to_string()
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

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["issue_view", "issue_list", "pr_view", "issue_comment", "issue_create", "pr_create"],
                    "description": "GitHub operation: issue_view/issue_list/pr_view are read-only (fetch issue/PR data as JSON); issue_comment/issue_create/pr_create are writes"
                },
                "repo": { "type": "string", "description": "Repository as owner/name (fixed at registration; input value is ignored)" },
                "number": { "type": "string", "description": "Issue or PR number (digits only)" },
                "state": { "type": "string", "enum": ["open", "closed", "all"], "description": "Filter for issue_list (optional, default open)" },
                "limit": { "type": "string", "description": "Max results for issue_list (digits, 1..=200, default 30)" },
                "body": { "type": "string", "description": "Markdown body for the comment, issue, or PR description" },
                "title": { "type": "string", "description": "Title for issue_create / pr_create" },
                "head": { "type": "string", "description": "Branch to merge FROM (pr_create)" },
                "base": { "type": "string", "description": "Branch to merge INTO (pr_create)" }
            },
            "required": ["op"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let op = input
            .get("op")
            .and_then(Value::as_str)
            .and_then(GhOp::parse)
            .ok_or_else(|| {
                "gh: 'op' must be one of issue_view | issue_list | pr_view | issue_comment | issue_create | pr_create".to_string()
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
    fn read_ops_build_expected_argv() {
        let t = GhTool::new("codecoradev/tole");
        // issue_view
        let line = t
            .command_line(&json!({ "op": "issue_view", "number": "33" }))
            .unwrap();
        assert_eq!(
            line,
            format!(
                "gh issue view 33 --json '{}' --repo codecoradev/tole",
                ISSUE_VIEW_FIELDS
            )
        );
        // pr_view
        let line = t
            .command_line(&json!({ "op": "pr_view", "number": "34" }))
            .unwrap();
        assert_eq!(
            line,
            format!(
                "gh pr view 34 --json '{}' --repo codecoradev/tole",
                PR_VIEW_FIELDS
            )
        );
        // issue_list default
        let line = t.command_line(&json!({ "op": "issue_list" })).unwrap();
        assert_eq!(
            line,
            format!(
                "gh issue list --json '{}' --repo codecoradev/tole",
                ISSUE_LIST_FIELDS
            )
        );
        // issue_list with state + limit
        let line = t
            .command_line(&json!({ "op": "issue_list", "state": "closed", "limit": "10" }))
            .unwrap();
        assert_eq!(
            line,
            format!(
                "gh issue list --json '{}' --state closed --limit 10 --repo codecoradev/tole",
                ISSUE_LIST_FIELDS
            )
        );
    }

    #[test]
    fn read_ops_reject_injection() {
        let t = GhTool::new("codecoradev/tole");
        // number must be digits
        assert!(t
            .command_line(&json!({ "op": "issue_view", "number": "--repo" }))
            .is_err());
        assert!(t
            .command_line(&json!({ "op": "pr_view", "number": "web" }))
            .is_err());
        // state whitelist
        assert!(t
            .command_line(&json!({ "op": "issue_list", "state": "--limit" }))
            .is_err());
        assert!(t
            .command_line(&json!({ "op": "issue_list", "state": "OPEN" }))
            .is_err());
        // limit digits + range
        assert!(t
            .command_line(&json!({ "op": "issue_list", "limit": "--repo" }))
            .is_err());
        assert!(t
            .command_line(&json!({ "op": "issue_list", "limit": "999" }))
            .is_err());
        // missing number
        assert!(t.command_line(&json!({ "op": "issue_view" })).is_err());
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
