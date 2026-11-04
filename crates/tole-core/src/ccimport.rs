//! Import a Claude Code session transcript into a tole session (#381).
//!
//! Input is the JSONL Claude Code writes (`~/.claude/projects/*/<id>.jsonl`)
//! or `claude -p --output-format stream-json` output. The result is a NEW
//! session file `cc-<claude-session-id>.jsonl`; nothing existing is touched
//! and re-importing the same session is refused (the deterministic id is the
//! double-attach guard for a finished session).
//!
//! Mapping: user prompts and assistant text become `message` entries;
//! `tool_use` blocks become `intent` entries (id `cc_<tool_use_id>`, tool
//! name kept verbatim — Claude Code names are only history here, never
//! executed by tole); `tool_result` blocks become `tool_result` / `error`
//! entries under their intent. Thinking blocks, sidechain (subagent) lines
//! and non-conversation lines are skipped. Model, version, branch and
//! timestamps are recorded in the `fact/cc_import` register. The import is
//! cut at the last settled assistant answer so the session rests at `Final`
//! and `resume <id> "<prompt>"` continues it.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{json, Value};

use crate::entry::{EntryType, NewEntry};
use crate::register::RegisterWrite;
use crate::state::{Pc, StateTransition};
use crate::storage::{Commit, JsonlStorage, Storage, StorageError};

/// Outcome of an import.
#[derive(Debug, Clone)]
pub struct CcImport {
    /// Id of the new tole session (`cc-<claude session id>`).
    pub session_id: String,
    /// Entries written.
    pub imported: usize,
    /// Trailing entries not imported because they were not settled (a prompt
    /// with no answer yet, or tool calls still open).
    pub dropped: usize,
    /// One line per skipped or odd input line (never fatal).
    pub warnings: Vec<String>,
}

/// Reads `src` and imports it into `dir`.
pub fn import_file(src: &Path, dir: &Path) -> Result<CcImport, StorageError> {
    import_str(&std::fs::read_to_string(src)?, dir)
}

/// Imports transcript text into `dir`.
pub fn import_str(jsonl: &str, dir: &Path) -> Result<CcImport, StorageError> {
    let mut b = Builder::default();
    for (i, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => b.line(i + 1, &v),
            Err(e) => b
                .warnings
                .push(format!("line {}: not JSON ({e}), skipped", i + 1)),
        }
    }
    b.finish(dir)
}

#[derive(Default)]
struct Builder {
    session: Option<String>,
    model: Option<String>,
    version: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    entries: Vec<NewEntry>,
    open_intents: HashSet<String>,
    seen_intents: HashSet<String>,
    settled: usize,
    warnings: Vec<String>,
}

impl Builder {
    fn line(&mut self, n: usize, v: &Value) {
        let kind = v["type"].as_str().unwrap_or_default();
        // Both spellings: transcript files use `sessionId`, stream-json
        // uses `session_id`.
        let take = |slot: &mut Option<String>, val: Option<&str>| {
            if slot.is_none() {
                *slot = val.filter(|s| !s.is_empty()).map(str::to_string);
            }
        };
        take(
            &mut self.session,
            v["sessionId"].as_str().or(v["session_id"].as_str()),
        );
        take(&mut self.cwd, v["cwd"].as_str());
        take(&mut self.version, v["version"].as_str());
        take(&mut self.branch, v["gitBranch"].as_str());
        if kind == "system" {
            take(&mut self.model, v["model"].as_str());
            return;
        }
        if kind != "user" && kind != "assistant" {
            return;
        }
        if v["isSidechain"] == json!(true) || v["isMeta"] == json!(true) {
            return;
        }
        let ts = v["timestamp"]
            .as_str()
            .and_then(parse_rfc3339_ms)
            .unwrap_or(0);
        if ts > 0 {
            self.first_ts.get_or_insert(ts);
            self.last_ts = Some(ts);
        }
        let msg = &v["message"];
        if kind == "assistant" {
            take(&mut self.model, msg["model"].as_str());
        }
        let blocks: Vec<Value> = match &msg["content"] {
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            Value::Array(a) => a.clone(),
            _ => {
                self.warnings
                    .push(format!("line {n}: no message content, skipped"));
                return;
            }
        };
        let mut prompt_open = false;
        for blk in &blocks {
            match (kind, blk["type"].as_str().unwrap_or_default()) {
                ("user", "text") => {
                    if !prompt_open {
                        // An interrupted tool call (user pressed Esc / the
                        // process died) never gets its tool_result. At the
                        // next prompt it is abandoned, not left open forever:
                        // record a synthetic error (mirrors the denial-note
                        // shape of #476) so the session can settle again.
                        for id in std::mem::take(&mut self.open_intents) {
                            self.warnings
                                .push(format!("abandoned tool call {id} (no tool_result)"));
                            self.entries.push(NewEntry {
                                id: None,
                                parent_id: Some(id),
                                kind: EntryType::new(EntryType::ERROR),
                                payload: json!({"error": "abandoned: interrupted without a tool_result"}),
                                timestamp: ts,
                            });
                        }
                        self.mark_if_settled();
                        prompt_open = true;
                    }
                    self.push_message("user", blk["text"].as_str().unwrap_or_default(), ts);
                }
                ("assistant", "text") => {
                    self.push_message("assistant", blk["text"].as_str().unwrap_or_default(), ts)
                }
                ("assistant", "tool_use") => self.tool_use(n, blk, ts),
                ("user", "tool_result") => self.tool_result(n, blk, ts),
                (_, "thinking" | "redacted_thinking") => {}
                (_, other) => self
                    .warnings
                    .push(format!("line {n}: {other} block skipped")),
            }
        }
    }

    /// A settled assistant answer with no open call is a safe cut point;
    /// checked when a new prompt starts and at end of input.
    fn mark_if_settled(&mut self) {
        let last_is_answer = self.entries.last().is_some_and(|e| {
            e.kind.as_str() == EntryType::MESSAGE && e.payload["role"] == "assistant"
        });
        if last_is_answer && self.open_intents.is_empty() {
            self.settled = self.entries.len();
        }
    }

    fn push_message(&mut self, role: &str, text: &str, ts: u64) {
        if text.trim().is_empty() {
            return;
        }
        self.entries.push(NewEntry {
            id: None,
            parent_id: None,
            kind: EntryType::new(EntryType::MESSAGE),
            payload: json!({"role": role, "text": text}),
            timestamp: ts,
        });
    }

    fn tool_use(&mut self, n: usize, blk: &Value, ts: u64) {
        let (Some(id), Some(name)) = (blk["id"].as_str(), blk["name"].as_str()) else {
            self.warnings
                .push(format!("line {n}: tool_use without id/name, skipped"));
            return;
        };
        let id = format!("cc_{id}");
        if !self.seen_intents.insert(id.clone()) {
            self.warnings
                .push(format!("line {n}: duplicate tool_use {id}, skipped"));
            return;
        }
        let mut payload = json!({
            "tool": name,
            "input": blk.get("input").cloned().unwrap_or(json!({})),
            "replay": "never",
        });
        // Several calls in one response flatten into one assistant message
        // on the wire, exactly like a tole read-only batch (#480).
        if self
            .entries
            .last()
            .is_some_and(|e| e.kind.as_str() == EntryType::INTENT)
        {
            payload["batch"] = json!(true);
        }
        self.open_intents.insert(id.clone());
        self.entries.push(NewEntry {
            id: Some(id),
            parent_id: None,
            kind: EntryType::new(EntryType::INTENT),
            payload,
            timestamp: ts,
        });
    }

    fn tool_result(&mut self, n: usize, blk: &Value, ts: u64) {
        let id = format!("cc_{}", blk["tool_use_id"].as_str().unwrap_or_default());
        if !self.open_intents.remove(&id) {
            self.warnings.push(format!(
                "line {n}: tool_result for unknown call {id}, skipped"
            ));
            return;
        }
        let text = match &blk["content"] {
            Value::String(s) => s.clone(),
            Value::Array(a) => a
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let (kind, payload) = if blk["is_error"] == json!(true) {
            (EntryType::ERROR, json!({"error": text}))
        } else {
            (EntryType::TOOL_RESULT, json!({"ok": true, "output": text}))
        };
        self.entries.push(NewEntry {
            id: None,
            parent_id: Some(id),
            kind: EntryType::new(kind),
            payload,
            timestamp: ts,
        });
    }

    fn finish(mut self, dir: &Path) -> Result<CcImport, StorageError> {
        self.mark_if_settled();
        let claude_id = self.session.clone().ok_or_else(|| {
            StorageError::Invalid("no Claude Code session id in the transcript".into())
        })?;
        let session_id = format!("cc-{claude_id}");
        if !crate::storage::is_valid_session_id(&session_id) {
            return Err(StorageError::Invalid(format!(
                "Claude Code session id {claude_id:?} cannot name a tole session"
            )));
        }
        if self.settled == 0 {
            return Err(StorageError::Invalid(
                "nothing to resume: the transcript has no completed assistant answer".into(),
            ));
        }
        let dropped = self.entries.len() - self.settled;
        self.entries.truncate(self.settled);
        let path = dir.join(format!("{session_id}.jsonl"));
        if path.exists() {
            return Err(StorageError::Invalid(format!(
                "already imported as {session_id} ({})",
                path.display()
            )));
        }
        let mut s = JsonlStorage::create(dir, session_id.clone(), self.cwd.clone())?;
        let imported = self.entries.len();
        let meta = json!({
            "sessionId": claude_id,
            "model": self.model,
            "version": self.version,
            "gitBranch": self.branch,
            "startedAt": self.first_ts,
            "endedAt": self.last_ts,
        });
        let written: Result<(), StorageError> = (|| {
            let mut c = Commit::new().register(RegisterWrite::set("fact", "cc_import", meta));
            for e in self.entries {
                c = c.entry(e);
            }
            s.commit(c)?;
            for pc in [Pc::Planning, Pc::Final] {
                let seq = s.state().seq;
                s.commit(Commit::new().transition(StateTransition::from(seq, pc)))?;
            }
            Ok(())
        })();
        if let Err(e) = written {
            drop(s);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        Ok(CcImport {
            session_id,
            imported,
            dropped,
            warnings: self.warnings,
        })
    }
}

/// `2026-11-01T10:00:01.000Z` → ms since the Unix epoch (UTC `Z` or a
/// numeric offset; anything else is `None`).
fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (time, off) = match rest.find(['Z', '+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "Z"),
    };
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    let off_s = match off {
        "Z" => 0,
        o => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let (oh, om) = o[1..].split_once(':')?;
            sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60)
        }
    };
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 - off_s;
    u64::try_from(secs)
        .ok()
        .map(|s| s * 1000 + (sec * 1000.0).round() as u64)
}
