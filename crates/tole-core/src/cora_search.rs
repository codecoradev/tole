//! `cora search` tool (E4): ReadOnly codebase search via the cora CLI.
//!
//! Shells out to `cora brain <query> --json` — reusing the existing
//! hybrid search (FTS5 + vector + graph → RRF) instead of reinventing it
//! inside the agent. Read-only by construction: the subcommand performs
//! no mutation, and the tool classification (`Risk::ReadOnly`) is the
//! enforcement point for the approval gate.

use crate::subprocess::{run_with_timeout, SUBPROCESS_TIMEOUT};
use crate::tool::{Risk, Tool};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::process::Command;
#[cfg(test)]
use std::time::Duration;

/// Hard ceiling on any subprocess this tool spawns. The turn loop is
/// synchronous — a hung `cora brain` would freeze the whole agent.
/// ReadOnly search over the current project's symbol index.
#[derive(Debug, Clone)]
pub struct CoraSearchTool {
    /// Working directory the CLI runs in (defaults to cwd).
    pub workdir: Option<String>,
    /// Max results (passed as `--limit`).
    pub limit: u32,
}

impl CoraSearchTool {
    pub fn new() -> Self {
        Self {
            workdir: None,
            limit: 5,
        }
    }

    pub fn in_dir(mut self, dir: impl Into<String>) -> Self {
        self.workdir = Some(dir.into());
        self
    }

    /// The exact CLI invocation this tool performs — public for tests and
    /// for hosts that want to audit what would run.
    pub fn command_line(&self, query: &str) -> String {
        format!(
            "cora brain {} --json --limit {}",
            shlex_quote(query),
            self.limit
        )
    }
}

impl Default for CoraSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for CoraSearchTool {
    fn name(&self) -> &str {
        "cora_search"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let query = input
            .get("query")
            .and_then(|q| q.as_str())
            .ok_or_else(|| "input must be {\"query\": \"...\"}".to_string())?;
        if query.trim().is_empty() {
            return Err("query must not be empty".into());
        }
        let mut cmd = Command::new("cora");
        cmd.arg("brain")
            .arg(query)
            .arg("--json")
            .arg("--limit")
            .arg(self.limit.to_string());
        if let Some(dir) = &self.workdir {
            cmd.current_dir(dir);
        }
        let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?;
        if !out.status.success() {
            // stderr can be long; cap it so a runaway error doesn't blow
            // up the durable log record.
            let stderr = String::from_utf8_lossy(&out.stderr);
            let snippet: String = stderr.chars().take(500).collect();
            return Err(format!("cora brain exited {}: {}", out.status, snippet));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: Value = serde_json::from_str(&stdout)
            .map_err(|e| format!("cora brain output was not valid JSON: {e}"))?;
        Ok(parsed)
    }
}

/// Minimal single-word shlex quoting for the audit string.
fn shlex_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '_') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classified_readonly() {
        let t = CoraSearchTool::new();
        assert_eq!(t.risk(), Risk::ReadOnly);
    }

    #[test]
    fn rejects_empty_query() {
        let t = CoraSearchTool::new();
        let err = t.execute(json!({"query": "   "})).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn rejects_missing_query_field() {
        let t = CoraSearchTool::new();
        assert!(t.execute(json!({"q": "x"})).is_err());
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn subprocess_is_killed_on_timeout() {
        // `sleep 30` would outlive the 150ms budget; the wrapper must kill
        // it and return an error instead of hanging the turn loop.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = std::time::Instant::now();
        let res = run_with_timeout(&mut cmd, Duration::from_millis(150));
        assert!(res.is_err(), "expected timeout error");
        assert!(res.unwrap_err().contains("timed out"));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "kill took too long"
        );
    }

    #[test]
    fn fast_subcommand_completes_normally() {
        let mut cmd = Command::new("true");
        let res = run_with_timeout(&mut cmd, Duration::from_secs(5));
        assert!(res.is_ok());
        assert!(res.unwrap().status.success());
    }
}
