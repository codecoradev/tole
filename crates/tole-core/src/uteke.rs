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

use crate::subprocess::{run_with_timeout, run_with_timeout_stdin, SUBPROCESS_TIMEOUT};
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::process::Command;

/// Longest room id of uteke's `category:title` convention (uteke 0.24).
const MAX_CONVENTIONAL_ROOM_ID_LEN: usize = 64;

const ROOM_ID_ERR: &str = "room must be [a-zA-Z0-9-_] (not starting with a hyphen) or uteke's \
     category:title form (lowercase, exactly one colon, at most 64 characters)";

/// True for uteke's `category:title` room id, mirroring uteke 0.24
/// (`^[a-z][a-z0-9-]*:[a-z0-9][a-z0-9._-]*$`, at most 64 bytes, one colon).
fn is_category_title_room(id: &str) -> bool {
    if id.len() > MAX_CONVENTIONAL_ROOM_ID_LEN {
        return false;
    }
    let Some((category, title)) = id.split_once(':') else {
        return false;
    };
    let mut c = category.bytes();
    let category_ok = matches!(c.next(), Some(b'a'..=b'z'))
        && c.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
    let mut t = title.bytes();
    let title_ok = matches!(t.next(), Some(b'a'..=b'z' | b'0'..=b'9'))
        && t.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
    category_ok && title_ok
}

/// The one room-id check shared by `uteke_recall` and `uteke_document`.
///
/// Accepts the legacy charset `[a-zA-Z0-9-_]` (unchanged, no leading hyphen,
/// as `repo-*`, `profile-*` and `agent-*` rooms use) and uteke's
/// `category:title` form. Neither can start with `-`, so a room can never be
/// parsed as an argv flag; callers also pass it as a positional value after
/// `room <sub>`, never as a flag.
fn validate_room_id(room: &str) -> Result<(), String> {
    let legacy = !room.is_empty()
        && !room.starts_with('-')
        && room
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if legacy || is_category_title_room(room) {
        Ok(())
    } else {
        Err(ROOM_ID_ERR.into())
    }
}

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

    fn summary(&self) -> String {
        "Recall relevant long-term memories from uteke, optionally scoped to a room.".into()
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
        // A leading-dash query would reach the uteke binary as a CLI FLAG,
        // not as search text (`uteke recall "--store /elsewhere"`) — the
        // same argv-flag-injection defense as the room check below.
        // (CodeCora scan 2026-09-18, MAJOR.)
        if query.starts_with('-') {
            return Err("query must not start with '-'".into());
        }
        let mut cmd = Command::new("uteke");
        crate::subprocess::scrub_env_for(&mut cmd, "uteke");
        match input.get("room").and_then(Value::as_str) {
            Some(room) if !room.trim().is_empty() => {
                // Same argv-flag-injection defense as UtekeDocumentTool:
                // a room like "--json" must never reach the argv.
                validate_room_id(room)?;
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
    /// Known secrets redacted from the markdown before it is stored (#595).
    secrets: crate::secrets::SecretSet,
    /// The uteke binary (`uteke` on PATH; a fake in tests).
    bin: String,
}

impl UtekeDocumentTool {
    pub fn new(default_room: Option<String>) -> Self {
        Self {
            default_room,
            secrets: crate::secrets::SecretSet::default(),
            bin: "uteke".into(),
        }
    }

    /// Redact these secrets from every document written (#595).
    pub fn with_secrets(mut self, secrets: &crate::secrets::SecretSet) -> Self {
        self.secrets = secrets.clone();
        self
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

    fn summary(&self) -> String {
        "Create or update a uteke document and link it into a room.".into()
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
        let under = input
            .get("parent")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .map(|p| format!(" under {p}"))
            .unwrap_or_default();
        match room {
            Some(r) => format!("uteke doc create {slug}{under} (linked to room {r})"),
            None => format!("uteke doc create {slug}{under} (unlinked)"),
        }
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string", "description": "URL-friendly document identifier (kebab-case)" },
                // NO advertised `title`/`tags`: execute() passes slug +
                // markdown only (the uteke doc-create CLI derives the
                // title from the first '# ' heading). Advertising inputs
                // the tool silently drops is a spec lie (cora scan-3
                // #33) — models waste turns filling dead fields.
                "markdown": { "type": "string", "description": "Full markdown content of the document (title = first '# ' heading)" },
                "room": { "type": "string", "description": "Optional room id to link the document into" },
                "parent": { "type": "string", "description": "Optional slug of an existing document to nest this one under (a tree up to 10 deep)" }
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
        // `parent` is a slug and is checked exactly like one: it reaches the
        // argv as the value of `--parent`. A blank one means none.
        let parent = input
            .get("parent")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty());
        if let Some(p) = parent {
            if p.starts_with('-') || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err("parent must be [a-zA-Z0-9-] and not start with a hyphen".into());
            }
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
            validate_room_id(r)?;
        }

        // 1) Create/update the document (content via stdin: no argv
        //    length limits, no content ever visible in `ps` output).
        //    Uses run_with_timeout_stdin: drain threads on all three
        //    pipes — no deadlock on large documents — and the hard
        //    subprocess timeout every other invocation gets.
        let mut cmd = Command::new(&self.bin);
        crate::subprocess::scrub_env_for(&mut cmd, "uteke");
        cmd.arg("doc").arg("create").arg(slug);
        if let Some(p) = parent {
            // An unknown parent or a tree past 10 deep is uteke's error,
            // surfaced unchanged below: no client-side guessing.
            cmd.arg("--parent").arg(p);
        }
        cmd.arg("--file").arg("-");
        // #595: the document leaves the process (uteke cloud when
        // configured); a known secret value never does.
        let markdown = self.secrets.redact(markdown);
        let out = run_with_timeout_stdin(&mut cmd, SUBPROCESS_TIMEOUT, markdown.as_bytes())?;
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
            let mut link = Command::new(&self.bin);
            // ENV-1: same secret scrubbing as every other spawn point
            // (CodeCora scan 2026-09-18 — this spawn point missed it).
            crate::subprocess::scrub_env_for(&mut link, "uteke");
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
            "parent": parent,
            "room": room,
            "linked": linked,
            "chars": markdown.chars().count(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #595: the stored document never carries a known secret value.
    #[cfg(unix)]
    #[test]
    fn document_markdown_is_redacted_before_it_leaves_the_process() {
        let dir = std::env::temp_dir().join(format!("tole-uteke-595-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("uteke");
        let stored = dir.join("stored.md");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh
cat > {}
echo ok
",
                stored.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let secrets = crate::secrets::SecretSet::collect(
            [(
                "TOLE_TEST_DOC_TOKEN".to_string(),
                "doc-secret-0123456".to_string(),
            )],
            std::iter::empty(),
        );
        let mut t = UtekeDocumentTool::new(None).with_secrets(&secrets);
        t.bin = bin.display().to_string();
        t.execute(json!({"slug": "notes", "markdown": "# Notes\nkey: doc-secret-0123456\n"}))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&stored).unwrap(),
            "# Notes\nkey: «TOLE_TEST_DOC_TOKEN»\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fake uteke that logs its argv, one call per line, and stdin.
    #[cfg(unix)]
    fn fake_uteke(tag: &str, exit: i32) -> (UtekeDocumentTool, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tole-uteke-576-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("uteke");
        let d = dir.display();
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> {d}/argv.log\ncat > /dev/null\n\
                 [ {exit} -ne 0 ] && echo 'parent not found: nope' >&2\nexit {exit}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut t = UtekeDocumentTool::new(None);
        t.bin = bin.display().to_string();
        (t, dir)
    }

    #[cfg(unix)]
    #[test]
    fn document_passes_parent_to_the_cli_only_when_given() {
        let (t, dir) = fake_uteke("argv", 0);
        t.execute(json!({"slug": "goal-1-r1", "markdown": "# r1", "parent": "goal-1"}))
            .unwrap();
        t.execute(json!({"slug": "plain", "markdown": "# p"}))
            .unwrap();
        t.execute(json!({"slug": "blank", "markdown": "# b", "parent": "  "}))
            .unwrap();
        let log = std::fs::read_to_string(dir.join("argv.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines,
            [
                "doc create goal-1-r1 --parent goal-1 --file -",
                "doc create plain --file -",
                "doc create blank --file -",
            ]
        );
        let out = t
            .execute(json!({"slug": "goal-2", "markdown": "# g", "parent": "goal-1"}))
            .unwrap();
        assert_eq!(out["parent"], "goal-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn document_rejects_a_bad_parent_before_any_spawn() {
        let (t, dir) = fake_uteke("bad", 0);
        for bad in ["--json", "-x", "has space", "a/b", "a_b", "ünï"] {
            let err = t
                .execute(json!({"slug": "ok", "markdown": "# x", "parent": bad}))
                .unwrap_err();
            assert!(err.contains("parent must be"), "{bad}: {err}");
        }
        assert!(!dir.join("argv.log").exists(), "nothing was spawned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn document_surfaces_the_cli_error_for_an_unknown_parent_unchanged() {
        let (t, dir) = fake_uteke("unknown", 2);
        let err = t
            .execute(json!({"slug": "child", "markdown": "# c", "parent": "nope"}))
            .unwrap_err();
        assert!(err.contains("parent not found: nope"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn document_describe_shows_the_parent_and_the_spec_offers_it() {
        let t = UtekeDocumentTool::new(Some("ops".into()));
        assert_eq!(
            t.describe(&json!({"slug": "goal-1-r1", "parent": "goal-1"})),
            "uteke doc create goal-1-r1 under goal-1 (linked to room ops)"
        );
        let t = UtekeDocumentTool::new(None);
        assert_eq!(
            t.describe(&json!({"slug": "x", "parent": "p"})),
            "uteke doc create x under p (unlinked)"
        );
        assert_eq!(
            t.describe(&json!({"slug": "x", "parent": " "})),
            "uteke doc create x (unlinked)"
        );
        assert!(t.spec().unwrap()["properties"]["parent"].is_object());
    }

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
            err.contains("not starting with a hyphen") || err.contains("[a-zA-Z0-9-_]"),
            "{err}"
        );
    }

    #[test]
    fn document_room_grammar_boundary() {
        // Refused ids fail before any spawn, so this never touches a store.
        let t = UtekeDocumentTool::new(None);
        for bad in [
            "-x",
            "--json",
            "foo:bar:baz",
            "Project:x",
            "project:X",
            ":x",
            "x:",
            "-a:b",
            "a b:c",
        ] {
            let err = t
                .execute(json!({"slug": "ok-slug", "markdown": "# x", "room": bad}))
                .unwrap_err();
            assert!(err.contains("category:title form"), "{bad}: {err}");
        }
        let long = format!("a:{}", "b".repeat(63));
        let err = t
            .execute(json!({"slug": "ok-slug", "markdown": "# x", "room": long}))
            .unwrap_err();
        assert!(err.contains("category:title form"), "{err}");
        // Accepted ids are pinned through validate_room_id (room_validator_grammar):
        // executing one would spawn uteke against the real store.
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

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[test]
    fn recall_rejects_flag_like_room() {
        // Regression: room values must never reach argv as flags.
        let t = UtekeRecallTool::new();
        let err = t
            .execute(json!({"query": "x", "room": "--json"}))
            .unwrap_err();
        assert!(err.contains("not starting with a hyphen"), "{err}");
    }

    #[test]
    fn recall_room_grammar_boundary() {
        let t = UtekeRecallTool::new();
        for bad in [
            "-x",
            "--json",
            "foo:bar:baz",
            "Project:x",
            "project:X",
            ":x",
            "x:",
            "-a:b",
            "a b:c",
        ] {
            let err = t.execute(json!({"query": "q", "room": bad})).unwrap_err();
            assert!(err.contains("category:title form"), "{bad}: {err}");
        }
        let long = format!("a:{}", "b".repeat(63));
        let err = t.execute(json!({"query": "q", "room": long})).unwrap_err();
        assert!(err.contains("category:title form"), "{err}");
    }

    #[test]
    fn room_validator_grammar() {
        for ok in [
            "ops",
            "repo-foo_bar",
            "Profile-Foo",
            "agent-a-1f-2-0",
            "_x",
            "project:qa-check",
            "research:docker",
            "team:weekly.standup",
            "a:0",
            "x-1:y_z",
        ] {
            assert!(validate_room_id(ok).is_ok(), "{ok}");
        }
        let at_limit = format!("a:{}", "b".repeat(62));
        assert_eq!(at_limit.len(), 64);
        assert!(validate_room_id(&at_limit).is_ok());
        assert!(validate_room_id(&format!("{at_limit}b")).is_err());
        for bad in [
            "",
            "-x",
            "--json",
            "-a:b",
            "foo:bar:baz",
            ":x",
            "x:",
            ":",
            "Project:x",
            "project:X",
            "1a:b",
            "a:-b",
            "a:.b",
            "a_b:c",
            "a b",
            "a:b c",
            "a:b\n",
            "project:utéke",
        ] {
            assert!(validate_room_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn colon_room_is_never_an_argv_flag() {
        // Every accepted colon id starts with a lowercase letter, so it can
        // never parse as a flag; call sites pass it as a positional value
        // after `room recall` / `room add-document`.
        for id in ["project:qa-check", "a:b", "x-1:0"] {
            assert!(validate_room_id(id).is_ok());
            assert!(!id.starts_with('-'));
        }
        for id in ["-x", "--json:a", "-p:q", "-:x"] {
            assert!(validate_room_id(id).is_err(), "{id}");
        }
    }

    #[test]
    fn recall_rejects_bad_room_charset() {
        let t = UtekeRecallTool::new();
        assert!(t
            .execute(json!({"query": "x", "room": "bad room"}))
            .is_err());
    }

    #[test]
    fn recall_rejects_leading_dash_query() {
        // A leading-dash query would reach the uteke CLI as a flag, not
        // as search text (CodeCora scan 2026-09-18, MAJOR).
        let t = UtekeRecallTool::new();
        assert!(t.execute(json!({"query": "--store /elsewhere"})).is_err());
        assert!(t.execute(json!({"query": "-x"})).is_err());
        // Benign queries pass the guard; the rest of the pipeline may go
        // either way depending on whether the uteke binary is installed
        // and the store reachable — assert the FAILURE REASON, not
        // success/failure itself (this test must never touch the real
        // store non-deterministically).
        match t.execute(json!({"query": "state machine design"})) {
            Ok(_) => {}
            Err(err) => assert!(
                !err.contains("must not start with '-'"),
                "guard fired on benign query: {err}"
            ),
        }
    }
}
