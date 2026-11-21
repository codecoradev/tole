//! The skills the owner has to look at, as data (#1042, `tole serve`): every
//! discovered skill with its approval state, the drafts waiting for review and
//! the skills the curator archived.
//!
//! Read-only by construction: it reads the approvals file, the skill ledger,
//! the draft store and the curator's mutation ledger, and writes nothing.
//! Approving, rejecting or restoring a skill stays with the owner at a
//! terminal (`tole skills approve`, `tole skills drafts`, `tole curator`): trust
//! approval is never a serve route.

use std::path::Path;

use serde_json::{json, Value};
use tole_core::skill_approval::format_time;
use tole_core::skills::curator::Archive;
use tole_core::skills::draft::{DraftStatus, DraftStore};

use super::drafts::printable;

/// Longest description or reason shown of a draft or an archive entry.
const TEXT_CHARS: usize = 300;

/// A model-written or file-borne string, terminal-safe and capped.
fn text(s: &str) -> String {
    printable(s).chars().take(TEXT_CHARS).collect()
}

/// `skills` (state is `approved`, `changed`, `not_approved`, `refused`,
/// `unreadable`), `drafts` (pending, oldest first) and `archived`, for the
/// skills discovered from `workspace`. A part that cannot be read is a line in
/// `notes`, not a failure of the rest.
pub fn state_json(workspace: Option<&Path>, secrets: &tole_core::secrets::SecretSet) -> Value {
    let mut notes: Vec<String> = Vec::new();
    let skills = super::approve::audit_rows(workspace).unwrap_or_else(|e| {
        notes.push(format!("skills not read: {e}"));
        Vec::new()
    });
    let drafts: Vec<Value> = DraftStore::default_store()
        .map(|s| s.list())
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.status == DraftStatus::Pending)
        .map(|d| {
            json!({
                "id": d.id,
                "name": text(&d.name),
                "description": text(&d.description),
                "created_at": format_time(d.created_at),
                "hash": d.content_hash,
                "tainted": d.tainted(),
                "warnings": d.warnings.iter().map(|f| text(&f.detail)).collect::<Vec<_>>(),
                "nearest": d.dedup.nearest.as_deref().map(text),
            })
        })
        .collect();
    let archived: Vec<Value> = Archive::default_archive()
        .map(|a| a.archived())
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            json!({
                "skill": text(&m.skill),
                "entry": m.id,
                "at": format_time(m.t),
                "reason": text(&m.reason),
            })
        })
        .collect();
    secrets.redact_value(&json!({
        "skills": skills,
        "drafts": drafts,
        "archived": archived,
        "notes": notes,
    }))
}
