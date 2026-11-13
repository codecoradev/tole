//! The skill curator's core (#593, design `skills-registry-design.md` §4.5,
//! §6.4): what keeps a growing library from filling with skills nobody uses.
//!
//! Everything here is deterministic and LLM-free.
//!
//! - A **usage ledger** (`skill-usage.jsonl`, append-only): per skill, when it
//!   was recalled and loaded and how the runs that used it ended. A run that
//!   loaded no skill is recorded with `skill: null`, the baseline a skill's
//!   outcomes are compared against.
//! - A **lifecycle** derived from that ledger, never stored: `active` →
//!   `stale` → `archived` by inactivity (Hermes' 14 and 30 days), a grace
//!   period for a skill never used, and protection for a pinned skill and a
//!   skill a cron job names. Only agent-grown skills (promoted drafts) or
//!   skills the owner adopted are managed; hand-written and third-party
//!   skills are never touched.
//! - **Package-aware moves**: archiving moves the whole skill directory
//!   (`references/`, `scripts/`, `assets/` with its `SKILL.md`), never
//!   flattening one file. Every move is an entry in an append-only
//!   **mutation ledger** with the package hash before and after, so a move
//!   restores by itself.
//! - The curator **proposes** and the owner decides: nothing here edits a
//!   skill's text, and an archived skill stays on disk, restorable.

use crate::skill_approval::read_dir_package;
use crate::skills::{read_file_capped, ReadCapError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The usage ledger in the tole data dir.
pub const USAGE_FILE: &str = "skill-usage.jsonl";
/// The curator's state (paused, pinned, locked, adopted).
pub const STATE_FILE: &str = "skill-curator.json";
/// The mutation ledger.
pub const MUTATION_FILE: &str = "skill-curator-ledger.jsonl";
/// Where archived packages go.
pub const ARCHIVE_DIR: &str = "skill-archive";
/// Days without use before a skill is stale (Hermes' default).
pub const DEFAULT_STALE_DAYS: u64 = 14;
/// Days without use before a skill is archived (Hermes' default).
pub const DEFAULT_ARCHIVE_DAYS: u64 = 30;
/// Days a skill that was never used is left alone after it appeared.
pub const DEFAULT_GRACE_DAYS: u64 = 30;
/// Outcomes a skill and the baseline each need before a comparison counts.
pub const MIN_OUTCOME_SAMPLES: u64 = 5;
/// How much lower than the baseline a skill's success rate must be.
pub const UNDERPERFORM_MARGIN: f64 = 0.2;
/// Largest ledger file read back; the newest lines past it are kept.
pub const MAX_LEDGER_BYTES: u64 = 8 * 1024 * 1024;
/// Largest state file read.
pub const MAX_STATE_BYTES: u64 = 256 * 1024;
const DAY: u64 = 86_400;

/// What happened to a skill in a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    /// The index offered it for a task.
    Recalled,
    /// A session loaded its body.
    Loaded,
    /// A run that used it ended well.
    Success,
    /// A run that used it ended badly.
    Failure,
}

/// One line of the usage ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Seconds since the Unix epoch.
    pub t: u64,
    /// The skill, or `None` for a run that loaded no skill (the baseline).
    pub skill: Option<String>,
    pub kind: UsageKind,
}

/// A skill's usage, folded from the ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub recalled: u64,
    pub loaded: u64,
    pub success: u64,
    pub failure: u64,
    /// First time the ledger saw it (any kind); 0 = never.
    pub first_seen: u64,
    /// Last load or outcome; 0 = never used. A recall alone is not use.
    pub last_used: u64,
}

impl Usage {
    pub fn outcomes(&self) -> u64 {
        self.success + self.failure
    }
    /// Share of outcomes that succeeded; `None` with no outcome.
    pub fn success_rate(&self) -> Option<f64> {
        (self.outcomes() > 0).then(|| self.success as f64 / self.outcomes() as f64)
    }
}

/// Folded ledger: per skill, plus the runs that loaded none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageSummary {
    pub skills: BTreeMap<String, Usage>,
    pub baseline: Usage,
}

/// The append-only usage ledger.
#[derive(Debug, Clone)]
pub struct UsageLedger {
    path: PathBuf,
}

impl UsageLedger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `$CODECORA_HOME/tole/skill-usage.jsonl`.
    pub fn default_ledger() -> Option<Self> {
        crate::paths::tole_data_dir().map(|d| Self::new(d.join(USAGE_FILE)))
    }

    /// Append one record as one line, in one write.
    pub fn append(&self, rec: &UsageRecord) -> Result<(), String> {
        append_line(&self.path, rec)
    }

    /// Fold the ledger. A malformed line is skipped, and a file over
    /// [`MAX_LEDGER_BYTES`] is read from its tail so the newest usage wins.
    pub fn summary(&self) -> UsageSummary {
        let mut out = UsageSummary::default();
        for rec in read_lines::<UsageRecord>(&self.path) {
            let u = match &rec.skill {
                Some(name) => out.skills.entry(name.clone()).or_default(),
                None => &mut out.baseline,
            };
            if u.first_seen == 0 || rec.t < u.first_seen {
                u.first_seen = rec.t;
            }
            match rec.kind {
                UsageKind::Recalled => u.recalled += 1,
                UsageKind::Loaded => {
                    u.loaded += 1;
                    u.last_used = u.last_used.max(rec.t);
                }
                UsageKind::Success => {
                    u.success += 1;
                    u.last_used = u.last_used.max(rec.t);
                }
                UsageKind::Failure => {
                    u.failure += 1;
                    u.last_used = u.last_used.max(rec.t);
                }
            }
        }
        out
    }
}

fn append_line<T: Serialize>(path: &Path, rec: &T) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut line = serde_json::to_string(rec).map_err(|e| e.to_string())?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The parseable lines of a JSONL file, oldest first. A missing file is
/// empty; past [`MAX_LEDGER_BYTES`] only the tail is read.
fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut skipped_head = false;
    if len > MAX_LEDGER_BYTES && f.seek(SeekFrom::Start(len - MAX_LEDGER_BYTES)).is_ok() {
        skipped_head = true;
    }
    let mut raw = Vec::new();
    if f.take(MAX_LEDGER_BYTES).read_to_end(&mut raw).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    if skipped_head {
        lines.next(); // a cut line
    }
    lines
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Curator settings the owner can change; the rest is derived.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuratorState {
    /// `tole curator pause`: a run changes nothing while true.
    #[serde(default)]
    pub paused: bool,
    /// Never archived.
    #[serde(default)]
    pub pinned: BTreeSet<String>,
    /// No agent edits (enforced where an agent could edit a skill).
    #[serde(default)]
    pub locked: BTreeSet<String>,
    /// Hand-written skills the owner put under curation.
    #[serde(default)]
    pub adopted: BTreeSet<String>,
}

impl CuratorState {
    /// `$CODECORA_HOME/tole/skill-curator.json`.
    pub fn default_path() -> Option<PathBuf> {
        crate::paths::tole_data_dir().map(|d| d.join(STATE_FILE))
    }

    /// A missing file is the default; a malformed one is an error.
    pub fn load(path: &Path) -> Result<Self, String> {
        match read_file_capped(path, MAX_STATE_BYTES) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|e| format!("{}: malformed curator state: {e}", path.display())),
            Err(ReadCapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".{STATE_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }
}

/// Where a managed skill is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Active,
    Stale,
    Archived,
}

/// Inactivity thresholds, in days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub stale_days: u64,
    pub archive_days: u64,
    pub grace_days: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            stale_days: DEFAULT_STALE_DAYS,
            archive_days: DEFAULT_ARCHIVE_DAYS,
            grace_days: DEFAULT_GRACE_DAYS,
        }
    }
}

impl Thresholds {
    /// `archive_days` must be longer than `stale_days`, or a skill would skip
    /// the stale step.
    pub fn check(&self) -> Result<(), String> {
        if self.stale_days == 0 || self.archive_days <= self.stale_days {
            return Err(format!(
                "archive_days ({}) must be greater than stale_days ({}), which must be at least 1",
                self.archive_days, self.stale_days
            ));
        }
        Ok(())
    }
}

/// What the lifecycle needs to know about one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFacts {
    pub name: String,
    /// Agent-grown (a promoted draft) or adopted by the owner.
    pub managed: bool,
    pub pinned: bool,
    /// A cron job names it.
    pub cron_protected: bool,
    /// When the skill appeared (its promotion time), seconds.
    pub created_at: u64,
    pub usage: Usage,
}

/// The lifecycle of one skill and why. `None` = the curator does not manage
/// it (hand-written, third-party, pinned or named by a cron job).
pub fn assess(f: &SkillFacts, t: &Thresholds, now: u64) -> Option<(Lifecycle, String)> {
    if !f.managed || f.pinned || f.cron_protected {
        return None;
    }
    let idle = |since: u64| now.saturating_sub(since) / DAY;
    if f.usage.last_used == 0 {
        // Never used: left alone through the grace period, then judged by
        // how long it has existed.
        let age = idle(f.created_at);
        if age < t.grace_days {
            return Some((
                Lifecycle::Active,
                format!("never used, {age} of {} grace days", t.grace_days),
            ));
        }
        return Some(by_idle(age, t, "never used in"));
    }
    Some(by_idle(idle(f.usage.last_used), t, "unused for"))
}

fn by_idle(days: u64, t: &Thresholds, what: &str) -> (Lifecycle, String) {
    let reason = format!("{what} {days} days");
    if days >= t.archive_days {
        (Lifecycle::Archived, reason)
    } else if days >= t.stale_days {
        (Lifecycle::Stale, reason)
    } else {
        (Lifecycle::Active, reason)
    }
}

/// One managed skill's assessment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub name: String,
    pub lifecycle: Lifecycle,
    pub reason: String,
}

/// Assess every managed skill, sorted by name.
pub fn plan(facts: &[SkillFacts], t: &Thresholds, now: u64) -> Vec<Assessment> {
    let mut out: Vec<Assessment> = facts
        .iter()
        .filter_map(|f| {
            assess(f, t, now).map(|(lifecycle, reason)| Assessment {
                name: f.name.clone(),
                lifecycle,
                reason,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A skill whose runs do worse than runs without any skill.
#[derive(Debug, Clone, PartialEq)]
pub struct Underperformer {
    pub name: String,
    pub rate: f64,
    pub baseline: f64,
    pub outcomes: u64,
}

/// Managed skills whose success rate is [`UNDERPERFORM_MARGIN`] below the
/// baseline of runs that loaded no skill, with at least
/// [`MIN_OUTCOME_SAMPLES`] outcomes on each side. A proposal for the owner,
/// not a verdict: the runs differ in more than the skill.
pub fn underperforming(facts: &[SkillFacts], baseline: &Usage) -> Vec<Underperformer> {
    let Some(base) = baseline
        .success_rate()
        .filter(|_| baseline.outcomes() >= MIN_OUTCOME_SAMPLES)
    else {
        return Vec::new();
    };
    let mut out: Vec<Underperformer> = facts
        .iter()
        .filter(|f| f.managed && !f.pinned && f.usage.outcomes() >= MIN_OUTCOME_SAMPLES)
        .filter_map(|f| {
            let rate = f.usage.success_rate()?;
            (rate + UNDERPERFORM_MARGIN <= base).then(|| Underperformer {
                name: f.name.clone(),
                rate,
                baseline: base,
                outcomes: f.usage.outcomes(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// What a mutation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    Archive,
    Restore,
}

/// One line of the mutation ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mutation {
    /// Monotonic within the ledger, from 1.
    pub id: u64,
    pub t: u64,
    pub kind: MutationKind,
    pub skill: String,
    /// The package directory before the move.
    pub from: String,
    /// The package directory after the move.
    pub to: String,
    /// Package hash before and after; equal for a lossless move.
    pub before_hash: String,
    pub after_hash: String,
    pub reason: String,
}

/// The append-only mutation ledger and the archive it describes.
#[derive(Debug, Clone)]
pub struct Archive {
    ledger: PathBuf,
    root: PathBuf,
}

impl Archive {
    pub fn new(ledger: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        Self {
            ledger: ledger.into(),
            root: root.into(),
        }
    }

    /// The default ledger and archive in the tole data dir.
    pub fn default_archive() -> Option<Self> {
        crate::paths::tole_data_dir().map(|d| Self::new(d.join(MUTATION_FILE), d.join(ARCHIVE_DIR)))
    }

    /// Every mutation, oldest first.
    pub fn mutations(&self) -> Vec<Mutation> {
        read_lines(&self.ledger)
    }

    fn next_id(&self) -> u64 {
        self.mutations().iter().map(|m| m.id).max().unwrap_or(0) + 1
    }

    /// The archived packages not restored since: the latest `Archive`
    /// mutation per skill when no later `Restore` follows it.
    pub fn archived(&self) -> Vec<Mutation> {
        let mut by_skill: BTreeMap<String, Mutation> = BTreeMap::new();
        for m in self.mutations() {
            match m.kind {
                MutationKind::Archive => {
                    by_skill.insert(m.skill.clone(), m);
                }
                MutationKind::Restore => {
                    by_skill.remove(&m.skill);
                }
            }
        }
        by_skill.into_values().collect()
    }

    /// Move the whole package directory `dir` of skill `name` into the
    /// archive, recording the hash before and after. Refuses a symlinked or
    /// missing directory.
    pub fn archive(
        &self,
        name: &str,
        dir: &Path,
        reason: &str,
        now: u64,
    ) -> Result<Mutation, String> {
        crate::skill_draft::name_ok(name)?;
        let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !meta.is_dir() {
            return Err(format!("{}: not a skill directory", dir.display()));
        }
        let before = read_dir_package(name, dir)?;
        std::fs::create_dir_all(&self.root).map_err(|e| format!("{}: {e}", self.root.display()))?;
        let id = self.next_id();
        let to = self.root.join(format!("{name}-{now}-{id}"));
        move_dir(dir, &to)?;
        let after = read_dir_package(name, &to)?;
        let m = Mutation {
            id,
            t: now,
            kind: MutationKind::Archive,
            skill: name.to_string(),
            from: dir.display().to_string(),
            to: to.display().to_string(),
            before_hash: before.hash,
            after_hash: after.hash,
            reason: reason.chars().take(200).collect(),
        };
        append_line(&self.ledger, &m)?;
        Ok(m)
    }

    /// Put an archived skill back where it was. Refuses when something is
    /// there now or when the archived bytes changed since the move.
    pub fn restore(&self, name: &str, now: u64) -> Result<Mutation, String> {
        let a = self
            .archived()
            .into_iter()
            .find(|m| m.skill == name)
            .ok_or_else(|| format!("no archived skill named {name:?}"))?;
        let (from, to) = (PathBuf::from(&a.to), PathBuf::from(&a.from));
        if std::fs::symlink_metadata(&to).is_ok() {
            return Err(format!(
                "{} exists now; move it aside before restoring {name:?}",
                to.display()
            ));
        }
        let now_hash = read_dir_package(name, &from)?.hash;
        if now_hash != a.after_hash {
            return Err(format!(
                "the archived copy of {name:?} changed since it was archived ({} then, {now_hash} now)",
                a.after_hash
            ));
        }
        move_dir(&from, &to)?;
        let back = read_dir_package(name, &to)?;
        let m = Mutation {
            id: self.next_id(),
            t: now,
            kind: MutationKind::Restore,
            skill: name.to_string(),
            from: a.to,
            to: a.from,
            before_hash: now_hash,
            after_hash: back.hash,
            reason: format!("restored archive entry {}", a.id),
        };
        append_line(&self.ledger, &m)?;
        Ok(m)
    }
}

/// Rename, falling back to copy then remove across filesystems.
fn move_dir(from: &Path, to: &Path) -> Result<(), String> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from).map_err(|e| format!("{}: {e}", from.display()))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            copy_dir(&src, &dst)?;
        } else if ft.is_file() {
            std::fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
        } else {
            return Err(format!(
                "{}: not a regular file or directory",
                src.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "skill_curator_tests.rs"]
mod tests;
