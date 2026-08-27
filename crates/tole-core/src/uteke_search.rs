//! `uteke search` tool (E7): ReadOnly semantic memory recall via the uteke CLI.
//!
//! Shells out to `uteke recall "<query>" --limit N --json` — reusing the
//! existing memory engine instead of embedding it in the agent. Read-only
//! by construction: recall performs no mutation, and `Risk::ReadOnly` is
//! the approval-gate enforcement point.

use crate::subprocess::{run_with_timeout, SUBPROCESS_TIMEOUT};
use crate::tool::{Risk, Tool};
use serde_json::Value;
use std::process::Command;

/// ReadOnly semantic recall over the local uteke store.
#[derive(Debug, Clone)]
pub struct UtekeSearchTool {
    /// Max results (passed as `--limit`).
    pub limit: u32,
}

impl UtekeSearchTool {
    pub fn new() -> Self {
        Self { limit: 5 }
    }

    /// The exact CLI invocation this tool performs — public for tests and
    /// for hosts that want to audit what would run.
    pub fn command_line(&self, query: &str) -> String {
        format!(
            "uteke recall {} --limit {} --json",
            shlex_quote(query),
            self.limit
        )
    }
}

impl Default for UtekeSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for UtekeSearchTool {
    fn name(&self) -> &str {
        "uteke_search"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn describe(&self, input: &Value) -> String {
        let q = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("<missing query>");
        format!("uteke recall: {q}")
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let query = input
            .get("query")
            .and_then(|q| q.as_str())
            .ok_or_else(|| "input must be {\"query\": \"...\"}".to_string())?;
        if query.trim().is_empty() {
            return Err("query must not be empty".into());
        }
        let mut cmd = Command::new("uteke");
        cmd.arg("recall")
            .arg(query)
            .arg("--limit")
            .arg(self.limit.to_string())
            .arg("--json");
        let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let snippet: String = stderr.chars().take(500).collect();
            return Err(format!("uteke recall exited {}: {}", out.status, snippet));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: Value = serde_json::from_str(&stdout)
            .map_err(|e| format!("uteke recall output was not valid JSON: {e}"))?;
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
    use serde_json::json;

    #[test]
    fn classified_readonly() {
        let t = UtekeSearchTool::new();
        assert_eq!(t.risk(), Risk::ReadOnly);
    }

    #[test]
    fn rejects_empty_query() {
        let t = UtekeSearchTool::new();
        let err = t.execute(json!({"query": "   "})).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn rejects_missing_query_field() {
        let t = UtekeSearchTool::new();
        assert!(t.execute(json!({"q": "x"})).is_err());
    }

    #[test]
    fn command_line_is_audit_friendly() {
        let t = UtekeSearchTool::new();
        assert_eq!(
            t.command_line("simple_query"),
            "uteke recall simple_query --limit 5 --json"
        );
        // Special chars get quoted so the audit string is a faithful
        // representation of a single argument.
        assert_eq!(
            t.command_line("q with space"),
            "uteke recall 'q with space' --limit 5 --json"
        );
    }
}
