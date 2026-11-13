//! Skill drafts (#592, design `skills-registry-design.md` §4.4, §6.3): how a
//! skill grows from real work without the agent ever writing a loadable
//! skill itself.
//!
//! ```text
//! skill_propose → gate (lint · secret scan · injection patterns · taint ·
//!                       meaning dedup) → DRAFT (never loadable) → owner
//! ```
//!
//! - A **draft** lives under `$CODECORA_HOME/tole/skill-drafts/<id>/`, outside
//!   every skills directory, so no loader can reach it. Only the owner's
//!   promotion (a later step) turns its bytes into a skill with an approved
//!   hash.
//! - The **gate** refuses a draft with a blocking finding (bad shape, a known
//!   secret value or a secret-shaped token, an instruction-override phrase,
//!   invisible characters). Nothing blocking is ever stored.
//! - **Provenance** (source session ids, trigger, model) is recorded on every
//!   draft, so a poisoned source can later revoke what grew from it.
//! - **Taint**: a draft from a session that consumed untrusted content (web,
//!   MCP results, a webhook payload) is marked `tainted` and needs explicit
//!   owner review; it is never fast-tracked.
//! - **Dedup**: similarity only *finds* candidates ([`Band`]). A similar but
//!   contradictory skill is a conflict, decided by a reviewer's [`Verdict`],
//!   never merged automatically.

use crate::secrets::SecretSet;
use crate::skills::{parse_skill, MAX_SKILL_CHARS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The drafts directory name in the tole data dir.
pub const DRAFTS_DIR: &str = "skill-drafts";
/// The draft record inside a draft's directory.
pub const DRAFT_FILE: &str = "draft.json";
/// The proposed SKILL.md inside a draft's directory.
pub const DRAFT_SKILL_FILE: &str = "SKILL.md";
/// Largest draft record read back.
pub const MAX_DRAFT_RECORD_BYTES: u64 = 256 * 1024;
/// Most `references/`, `scripts/` or `assets/` files one skill may point at
/// (SkillsBench: 2–3 modules at most).
pub const MAX_MODULES: usize = 3;
/// Dated lines at which a body reads as an incident log, not a rule.
pub const INCIDENT_LOG_LINES: usize = 3;
/// Cosine at or above which a draft becomes a patch of the existing skill.
pub const PATCH_THRESHOLD: f32 = 0.92;
/// Cosine at or above which a draft becomes a reference of the closer skill.
pub const REFERENCE_THRESHOLD: f32 = 0.80;
/// Most source sessions a draft records.
pub const MAX_SOURCES: usize = 16;
/// Longest source/model label kept.
const MAX_LABEL: usize = 128;

/// Why the agent proposes the skill (design §4.4 triggers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    OwnerCorrection,
    DeadEnd,
    RepeatedWorkflow,
}

impl Trigger {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner_correction" => Some(Self::OwnerCorrection),
            "dead_end" => Some(Self::DeadEnd),
            "repeated_workflow" => Some(Self::RepeatedWorkflow),
            _ => None,
        }
    }
}

/// Where a draft came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Session ids the lesson was drawn from (the host's own session first).
    pub sessions: Vec<String>,
    pub trigger: Trigger,
    pub model: Option<String>,
}

/// How serious a gate finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The draft is refused.
    Block,
    /// The draft is stored; the owner sees the warning.
    Warn,
}

/// One gate finding. `rule` is a stable id; `detail` never quotes a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub rule: String,
    pub severity: Severity,
    pub detail: String,
}

fn block(rule: &str, detail: impl Into<String>) -> Finding {
    Finding {
        rule: rule.into(),
        severity: Severity::Block,
        detail: detail.into(),
    }
}

fn warn(rule: &str, detail: impl Into<String>) -> Finding {
    Finding {
        rule: rule.into(),
        severity: Severity::Warn,
        detail: detail.into(),
    }
}

/// The proposed action for a draft near an existing skill (design §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// ≥ [`PATCH_THRESHOLD`]: patch the existing skill, no new skill.
    Patch,
    /// ≥ [`REFERENCE_THRESHOLD`]: a `references/` file of the closer skill.
    Reference,
    /// Below both: a new skill candidate.
    New,
}

impl Band {
    pub fn from_score(score: f32) -> Self {
        if score >= PATCH_THRESHOLD {
            Self::Patch
        } else if score >= REFERENCE_THRESHOLD {
            Self::Reference
        } else {
            Self::New
        }
    }
}

/// A reviewer's call on a similar skill. Similarity cannot see negation
/// ("deploy to staging" vs "production"), so only a reviewer says `Conflict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Patch,
    Reference,
    New,
    Conflict,
}

/// The nearest existing skill and what to do about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dedup {
    /// The closest existing skill, if the index returned one.
    pub nearest: Option<String>,
    pub score: f32,
    pub band: Band,
    /// Set by a reviewer; `None` until then.
    #[serde(default)]
    pub verdict: Option<Verdict>,
}

impl Dedup {
    /// From the nearest candidate (`None`: nothing similar, a new skill).
    pub fn from_nearest(nearest: Option<(String, f32)>) -> Self {
        match nearest {
            Some((name, score)) => Self {
                nearest: Some(name),
                score,
                band: Band::from_score(score),
                verdict: None,
            },
            None => Self {
                nearest: None,
                score: 0.0,
                band: Band::New,
                verdict: None,
            },
        }
    }
}

/// Draft lifecycle. Only the owner moves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Pending,
    Rejected,
    Promoted,
}

/// A stored draft record (the SKILL.md sits beside it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: DraftStatus,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
    /// `sha256:<hex>` of the proposed SKILL.md bytes.
    pub content_hash: String,
    pub provenance: Provenance,
    /// Untrusted sources the source session consumed; empty = clean.
    pub taint: Vec<String>,
    /// Warnings kept for the owner (blocking findings never get here).
    pub warnings: Vec<Finding>,
    pub dedup: Dedup,
}

impl Draft {
    pub fn tainted(&self) -> bool {
        !self.taint.is_empty()
    }
}

/// Tools whose output is text written by someone outside the owner's trust:
/// a web page, an MCP server's result, an issue or PR body (`gh`, `gitea`),
/// what a child agent or another session produced, and shared memory.
///
/// Deliberately not listed: `read_file` and `run_command`. A coding session
/// reads files and runs commands all the time, so counting them would taint
/// every draft and `--accept-taint` would become a reflex. The tradeoff is
/// written down: a draft from a session that curled a page through
/// `run_command` is not marked.
pub fn is_untrusted_tool(name: &str) -> bool {
    matches!(
        name,
        "web_fetch"
            | "web_search"
            | "gh"
            | "gitea"
            | "agent_start"
            | "agent_poll"
            | "delegate"
            | "delegate_poll"
            | "tole_session_prompt"
            | "tole_session_status"
            | "tole_session_list"
            | "uteke_recall"
            | "uteke_document"
    ) || name.starts_with("mcp_")
}

/// What a session consumed from outside. Shared by clones, so a tool wrapper
/// records and `skill_propose` reads the same set.
#[derive(Debug, Clone, Default)]
pub struct TaintTracker {
    sources: Arc<Mutex<BTreeSet<String>>>,
}

impl TaintTracker {
    /// Record a source by label (a tool name, `webhook`, ...).
    pub fn mark(&self, source: &str) {
        if let Ok(mut s) = self.sources.lock() {
            s.insert(source.chars().take(MAX_LABEL).collect());
        }
    }

    /// Record `tool` when its output is untrusted.
    pub fn observe_tool(&self, tool: &str) {
        if is_untrusted_tool(tool) {
            self.mark(tool);
        }
    }

    /// The sources seen so far, sorted.
    pub fn sources(&self) -> Vec<String> {
        self.sources
            .lock()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Phrases that try to steer the reader of a skill. Matched on lowercased,
/// whitespace-collapsed text.
const OVERRIDE_PHRASES: &[&str] = &[
    "ignore previous instructions",
    "ignore all previous",
    "ignore the above",
    "disregard previous",
    "disregard the above",
    "forget your instructions",
    "override your instructions",
    "new instructions:",
    "do not tell the user",
    "don't tell the user",
    "without telling the user",
    "do not mention this to the user",
    "reveal your system prompt",
];

/// Shapes that run fetched code or reach for credentials: a warning, not a
/// refusal, since a legitimate deploy skill may mention them.
const RISKY_PHRASES: &[&str] = &[
    "| sh",
    "| bash",
    "| zsh",
    "base64 -d",
    "base64 --decode",
    "~/.ssh",
    "~/.aws",
    "~/.codecora",
    "authorization: bearer",
    "disable the approval",
    "skip approval",
    "--dangerously",
];

/// Token shapes that are secrets whatever their value. A small interim set;
/// the shared ruleset with uteke (#597) replaces it.
fn secret_shape(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Some("a private key block");
    }
    let has_prefixed = |prefix: &str, min_tail: usize| {
        text.match_indices(prefix).any(|(i, _)| {
            // A token starts at a boundary: `risk-assessment-...` is a word.
            let boundary = text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
            boundary
                && text[i + prefix.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .count()
                    >= min_tail
        })
    };
    if has_prefixed("ghp_", 30)
        || has_prefixed("gho_", 30)
        || has_prefixed("github_pat_", 30)
        || has_prefixed("sk-", 32)
        || has_prefixed("AKIA", 16)
        || has_prefixed("xoxb-", 20)
    {
        return Some("a token-shaped string");
    }
    None
}

/// Characters a reviewer cannot see: everything the approval sanitizer
/// treats as unsafe, plus the ranges it leaves alone because they are legal
/// in a prompt but can smuggle text into a skill (Unicode tag characters,
/// variation selectors, the soft hyphen, Hangul and Braille fillers).
pub fn is_hidden(c: char) -> bool {
    crate::sanitize::is_unsafe(c)
        || matches!(
            c,
            '\u{00AD}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{2800}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FFA0}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// Lowercased, hidden characters removed and whitespace collapsed, so
/// `ig\u{00AD}nore previous instructions` still matches a phrase.
fn collapse(text: &str) -> String {
    text.chars()
        .filter(|c| !is_hidden(*c) || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Shape rules: valid frontmatter, a description, the size cap, one
/// procedure, 2–3 modules at most, rules not an incident log.
pub fn lint(name: &str, content: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Err(e) = name_ok(name) {
        out.push(block("name", e));
        return out;
    }
    let skill = match parse_skill(content, name) {
        Ok(s) => s,
        Err(e) => {
            out.push(block("frontmatter", e));
            return out;
        }
    };
    if skill.description.trim().is_empty() {
        out.push(block(
            "description",
            "the description is empty; recall depends on it",
        ));
    }
    if skill.body.trim().is_empty() {
        out.push(block("body", "the body is empty"));
    }
    if content.len() > MAX_SKILL_CHARS {
        out.push(block(
            "size",
            format!(
                "{} bytes, over the {MAX_SKILL_CHARS} cap; split it or cut it",
                content.len()
            ),
        ));
    }
    let h1 = skill
        .body
        .lines()
        .filter(|l| l.starts_with("# ") && !l.starts_with("##"))
        .count();
    if h1 > 1 {
        out.push(block(
            "one-procedure",
            format!("{h1} top-level headings: one skill is one procedure"),
        ));
    }
    let modules = module_paths(&skill.body);
    if modules.len() > MAX_MODULES {
        out.push(block(
            "modules",
            format!(
                "{} module files referenced, at most {MAX_MODULES}",
                modules.len()
            ),
        ));
    }
    let dated = skill.body.lines().filter(|l| starts_with_date(l)).count();
    if dated >= INCIDENT_LOG_LINES {
        out.push(block(
            "incident-log",
            format!("{dated} dated lines: write the rule, not the incident"),
        ));
    }
    out
}

/// Distinct `references/…`, `scripts/…`, `assets/…` paths a body mentions.
fn module_paths(body: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for dir in ["references/", "scripts/", "assets/"] {
        for (i, _) in body.match_indices(dir) {
            let tail: String = body[i..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
                .collect();
            if tail.len() > dir.len() {
                set.insert(tail.trim_end_matches('.').to_string());
            }
        }
    }
    set
}

fn starts_with_date(line: &str) -> bool {
    let t = line
        .trim_start_matches(['-', '*', ' ', '[', '#'])
        .as_bytes();
    t.len() >= 10
        && t[..4].iter().all(u8::is_ascii_digit)
        && t[4] == b'-'
        && t[5..7].iter().all(u8::is_ascii_digit)
        && t[7] == b'-'
        && t[8..10].iter().all(u8::is_ascii_digit)
}

/// A skill name: lowercase letters, digits and hyphens, 1–64 characters,
/// starting with a letter or digit. Also the draft directory's safety rule.
pub fn name_ok(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid skill name {name:?}: lowercase letters, digits and '-', up to 64"
        ))
    }
}

/// Secrets in the text: a known value from `secrets`, or a secret shape.
/// The detail never quotes the match.
pub fn scan_secrets(content: &str, secrets: &SecretSet) -> Vec<Finding> {
    let mut out = Vec::new();
    if secrets.redact(content) != content {
        out.push(block(
            "secret",
            "the text holds a value of a known secret; remove it",
        ));
    }
    if let Some(what) = secret_shape(content) {
        out.push(block(
            "secret-shape",
            format!("the text holds {what}; remove it"),
        ));
    }
    out
}

/// Instruction-override phrases (block), risky shapes (warn) and invisible
/// characters (block: they hide text from the owner who reviews it).
pub fn scan_injection(content: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let flat = collapse(content);
    for p in OVERRIDE_PHRASES {
        if flat.contains(p) {
            out.push(block(
                "injection",
                format!("an instruction-override phrase ({p:?})"),
            ));
        }
    }
    for p in RISKY_PHRASES {
        if flat.contains(p) {
            out.push(warn("risky-shape", format!("mentions {p:?}")));
        }
    }
    let hidden = content
        .chars()
        .filter(|c| is_hidden(*c) && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    if hidden > 0 {
        out.push(block(
            "hidden-chars",
            format!("{hidden} invisible or control characters"),
        ));
    }
    out
}

/// What the agent supplies; the host adds the rest.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub name: String,
    /// The whole SKILL.md (frontmatter + body).
    pub skill_md: String,
    pub trigger: Trigger,
    /// Extra source session ids the agent names (the host's own is added).
    pub sources: Vec<String>,
}

/// What the host knows and the agent cannot set.
#[derive(Debug, Clone)]
pub struct HostContext {
    pub session: Option<String>,
    pub model: Option<String>,
    pub taint: Vec<String>,
    pub dedup: Dedup,
    pub now: u64,
}

/// Run the gate. `Err` carries every finding when any blocks; `Ok` is a
/// pending draft with its warnings.
pub fn gate(
    p: &Proposal,
    ctx: &HostContext,
    secrets: &SecretSet,
) -> Result<(Draft, String), Vec<Finding>> {
    let mut findings = lint(&p.name, &p.skill_md);
    findings.extend(scan_secrets(&p.skill_md, secrets));
    findings.extend(scan_injection(&p.skill_md));
    if findings.iter().any(|f| f.severity == Severity::Block) {
        return Err(findings);
    }
    // Lint passed, so the frontmatter parses.
    let skill = parse_skill(&p.skill_md, &p.name).map_err(|e| vec![block("frontmatter", e)])?;
    let mut sessions: Vec<String> = Vec::new();
    for s in ctx.session.iter().chain(p.sources.iter()) {
        let s: String = s.chars().take(MAX_LABEL).collect();
        if !s.trim().is_empty() && !sessions.contains(&s) && sessions.len() < MAX_SOURCES {
            sessions.push(s);
        }
    }
    let hash = crate::skill_index::content_hash(p.skill_md.as_bytes());
    let id = format!(
        "d{}-{}",
        ctx.now,
        hash.trim_start_matches("sha256:")
            .chars()
            .take(8)
            .collect::<String>()
    );
    let draft = Draft {
        id,
        name: p.name.clone(),
        description: skill.description,
        status: DraftStatus::Pending,
        created_at: ctx.now,
        content_hash: hash,
        provenance: Provenance {
            sessions,
            trigger: p.trigger,
            model: ctx
                .model
                .as_ref()
                .map(|m| m.chars().take(MAX_LABEL).collect()),
        },
        taint: ctx.taint.clone(),
        warnings: findings,
        dedup: ctx.dedup.clone(),
    };
    Ok((draft, p.skill_md.clone()))
}

/// The drafts directory: one subdirectory per draft.
#[derive(Debug, Clone)]
pub struct DraftStore {
    dir: PathBuf,
}

impl DraftStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `$CODECORA_HOME/tole/skill-drafts`; `None` when the home is unresolvable.
    pub fn default_store() -> Option<Self> {
        crate::paths::tole_data_dir().map(|d| Self::new(d.join(DRAFTS_DIR)))
    }

    fn draft_dir(&self, id: &str) -> Result<PathBuf, String> {
        let ok = !id.is_empty()
            && id.len() <= 80
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if ok {
            Ok(self.dir.join(id))
        } else {
            Err(format!("invalid draft id {id:?}"))
        }
    }

    /// Store a draft and its SKILL.md. Refuses an id already present: a
    /// draft is never overwritten.
    pub fn create(&self, draft: &Draft, skill_md: &str) -> Result<(), String> {
        let dir = self.draft_dir(&draft.id)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        write_file(&dir.join(DRAFT_SKILL_FILE), skill_md.as_bytes())?;
        write_record(&dir, draft)
    }

    /// One draft record.
    pub fn get(&self, id: &str) -> Result<Draft, String> {
        let path = self.draft_dir(id)?.join(DRAFT_FILE);
        let raw = crate::skills::read_file_capped(&path, MAX_DRAFT_RECORD_BYTES)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_slice(&raw)
            .map_err(|e| format!("{}: malformed draft: {e}", path.display()))
    }

    /// The proposed SKILL.md of a draft.
    pub fn skill_md(&self, id: &str) -> Result<String, String> {
        let path = self.draft_dir(id)?.join(DRAFT_SKILL_FILE);
        let raw = crate::skills::read_file_capped(&path, crate::skills::MAX_SKILL_FILE_BYTES)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        String::from_utf8(raw).map_err(|_| format!("{}: not UTF-8", path.display()))
    }

    /// Every readable draft, oldest first. An unreadable one is skipped.
    pub fn list(&self) -> Vec<Draft> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<Draft> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|id| self.get(&id).ok())
            .collect();
        out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        out
    }

    /// Move a draft out of `Pending` (the owner's call). A decided draft does
    /// not move again.
    pub fn decide(&self, id: &str, status: DraftStatus) -> Result<Draft, String> {
        let mut d = self.get(id)?;
        if d.status != DraftStatus::Pending {
            return Err(format!("draft {id} is already {:?}", d.status));
        }
        if status == DraftStatus::Pending {
            return Err("a draft cannot go back to pending".into());
        }
        d.status = status;
        write_record(&self.draft_dir(id)?, &d)?;
        Ok(d)
    }

    /// Record a reviewer's verdict on the draft's nearest skill.
    pub fn set_verdict(&self, id: &str, verdict: Verdict) -> Result<Draft, String> {
        let mut d = self.get(id)?;
        if d.status != DraftStatus::Pending {
            return Err(format!("draft {id} is already {:?}", d.status));
        }
        d.dedup.verdict = Some(verdict);
        write_record(&self.draft_dir(id)?, &d)?;
        Ok(d)
    }
}

fn write_record(dir: &Path, draft: &Draft) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(draft).map_err(|e| e.to_string())?;
    write_file(&dir.join(DRAFT_FILE), &json)
}

/// Atomic write: temp file in the same directory, then rename.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

#[cfg(test)]
#[path = "skill_draft_tests.rs"]
mod tests;
