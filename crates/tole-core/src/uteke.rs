//! Uteke first-class tools (B4/B5): semantic memory recall + documents.
//!
//! Two tools, both shelling out to the local `uteke` CLI (reuse over
//! embedding — the memory engine already exists):
//!
//! - `uteke_recall` (B5 rename of `uteke_search`): ReadOnly semantic
//!   recall, optionally room-scoped. The old name promised "search" but
//!   the CLI op is `recall` — now the tool name matches the verb.
//! - `uteke_document` (B4): create-or-update a markdown document in the
//!   uteke store and optionally link it to a room. Risk::Write — it
//!   mutates the owner's memory store. This is the owner's
//!   markdown-to-room workflow (agent drafts → durable knowledge).
//!
//! Both tools are registered behind startup probing (see the host): if
//! the `uteke` binary is missing, they are skipped with a warning
//! instead of appearing as phantom tools that fail on every call.

use crate::subprocess::{run_with_timeout, SUBPROCESS_TIMEOUT};
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::process::Command;

/// ReadOnly semantic recall over the local uteke store (room-optional).
#[derive(Debug, Clone)]
pub struct UtekeRecallTool {
    /// Max results (passed as `--limit`).
    pub limit: u32,
}

impl UtekeRecallTool {
    pub fn new() -> Self {
        Self { limit: 5 }
    }
}

impl Default for UtekeRecallTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for UtekeRecallTool {
    fn name(&self) -> &str {
        "uteke_recall"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn describe(&self, input: &Value) -> String {
        let q = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("<missing query>");
        match input.get("room").and_then(Value::as_str) {
            Some(room) => format!("uteke room recall {room:?}: {q}"),
            None => format!("uteke recall: {q}"),
        }
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Semantic query for the uteke memory index" },
                "room": { "type": "string", "description": "Optional room id to scope the recall (uteke room recall <room> \"<query>\")" }
            },
            "required": ["query"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must be {\"query\": \"...\"}".to_owned())?;
        if query.trim().is_empty() {
            return Err("query must not be empty".into());
        }
        let mut cmd = Command::new("uteke");
        match input.get("room").and_then(Value::as_str) {
            Some(room) if !room.trim().is_empty() => {
                // Room-scoped: uteke room recall <room> "<query>" --limit N --json
                cmd.arg("room")
                    .arg("recall")
                    .arg(room)
                    .arg(query)
                    .arg("--limit")
                    .arg(self.limit.to_string())
                    .arg("--json");
            }
            _ => {
                cmd.arg("recall")
                    .arg(query)
                    .arg("--limit")
                    .arg(self.limit.to_string())
                    .arg("--json");
            }
        }
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

/// Write-path document tool: create-or-update a markdown document in
/// uteke and (optionally) link it to a room.
#[derive(Debug, Clone)]
pub struct UtekeDocumentTool {
    /// Default room to link into when the caller does not specify one.
    /// When `None`, documents are created unlinked (host decides).
    pub default_room: Option<String>,
}

impl UtekeDocumentTool {
    pub fn new(default_room: Option<String>) -> Self {
        Self { default_room }
    }
}

impl Tool for UtekeDocumentTool {
    fn name(&self) -> &str {
        "uteke_document"
    }

    fn risk(&self) -> Risk {
        // Mutates the owner's memory store (doc upsert + room link).
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        let slug = input
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or("<missing slug>");
        let room = input
            .get("room")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.default_room.clone());
        match room {
            Some(r) => format!("uteke doc create {slug} (linked to room {r})"),
            None => format!("uteke doc create {slug} (unlinked)"),
        }
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string", "description": "URL-friendly document identifier (kebab-case)" },
                "title": { "type": "string", "description": "Document title (defaults to the first '# ' heading)" },
                "markdown": { "type": "string", "description": "Full markdown content of the document" },
                "room": { "type": "string", "description": "Optional room id to link the document into" },
                "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags" }
            },
            "required": ["slug", "markdown"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let slug = input
            .get("slug")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must include \"slug\"".to_owned())?;
        if slug.trim().is_empty() {
            return Err("slug must not be empty".into());
        }
        // Slug charset: letters, digits, hyphen; must not START with a
        // hyphen — "--json" passes a charset check but would be parsed
        // as a flag by the CLI. Blocks argv flag injection.
        if slug.starts_with('-') || !slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("slug must be [a-zA-Z0-9-] and not start with a hyphen".into());
        }
        let markdown = input
            .get("markdown")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must include \"markdown\"".to_owned())?;
        if markdown.trim().is_empty() {
            return Err("markdown must not be empty".into());
        }
        let room = input
            .get("room")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.default_room.clone())
            .filter(|r| !r.trim().is_empty());
        if let Some(r) = &room {
            if r.starts_with('-')
                || !r
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err("room must be [a-zA-Z0-9-_] and not start with a hyphen".into());
            }
        }

        // 1) Create/update the document (content via stdin: no argv
        //    length limits, no content ever visible in `ps` output).
        let mut cmd = Command::new("uteke");
        cmd.arg("doc")
            .arg("create")
            .arg(slug)
            .arg("--file")
            .arg("-");
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawning uteke doc create: {e}"))?;
        {
            use std::io::Write;
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| "uteke stdin unavailable".to_owned())?;
            stdin
                .write_all(markdown.as_bytes())
                .map_err(|e| format!("writing document to uteke stdin: {e}"))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("waiting for uteke doc create: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let snippet: String = stderr.chars().take(500).collect();
            return Err(format!(
                "uteke doc create exited {}: {}",
                out.status, snippet
            ));
        }

        // 2) Link to the room when one is resolved.
        let mut linked = false;
        if let Some(room) = &room {
            let mut link = Command::new("uteke");
            link.arg("room")
                .arg("add-document")
                .arg(room)
                .arg(slug)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let out = run_with_timeout(&mut link, SUBPROCESS_TIMEOUT)?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let snippet: String = stderr.chars().take(500).collect();
                return Err(format!(
                    "document created but room link '{room}' failed: {snippet}"
                ));
            }
            linked = true;
        }

        Ok(json!({
            "slug": slug,
            "room": room,
            "linked": linked,
            "chars": markdown.chars().count(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_classified_readonly() {
        assert_eq!(UtekeRecallTool::new().risk(), Risk::ReadOnly);
    }

    #[test]
    fn recall_name_is_recall() {
        // B5: the tool name matches the verb it performs.
        assert_eq!(UtekeRecallTool::new().name(), "uteke_recall");
    }

    #[test]
    fn document_classified_write() {
        let t = UtekeDocumentTool::new(None);
        assert_eq!(t.risk(), Risk::Write);
    }

    #[test]
    fn document_rejects_bad_slug() {
        let t = UtekeDocumentTool::new(None);
        // Flag injection via slug must be rejected before any spawn.
        let err = t
            .execute(json!({"slug": "--json", "markdown": "# x"}))
            .unwrap_err();
        assert!(err.contains("not start with a hyphen"), "{err}");
    }

    #[test]
    fn document_rejects_bad_room() {
        let t = UtekeDocumentTool::new(None);
        let err = t
            .execute(json!({"slug": "ok-slug", "markdown": "# x", "room": "bad room"}))
            .unwrap_err();
        assert!(
            err.contains("not start with a hyphen") || err.contains("[a-zA-Z0-9-_]"),
            "{err}"
        );
    }

    #[test]
    fn document_requires_slug_and_markdown() {
        let t = UtekeDocumentTool::new(None);
        assert!(t.execute(json!({"markdown": "# x"})).is_err());
        assert!(t.execute(json!({"slug": "x"})).is_err());
        assert!(t.execute(json!({"slug": "x", "markdown": "  "})).is_err());
    }

    #[test]
    fn document_describe_shows_room() {
        let t = UtekeDocumentTool::new(Some("tole-dev".into()));
        let d = t.describe(&json!({"slug": "retro-b1"}));
        assert!(d.contains("tole-dev"), "{d}");
        let t2 = UtekeDocumentTool::new(None);
        let d2 = t2.describe(&json!({"slug": "retro-b1", "room": "ops"}));
        assert!(d2.contains("ops"), "{d2}");
    }
}
