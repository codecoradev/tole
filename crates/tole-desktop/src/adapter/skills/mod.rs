//! Skills and connected tools: `GET /skills` and `GET /tools` to the model's
//! `Raw*` types and the page state (`SkillsPageState`).
//!
//! The mapping is the one the model's `skills` module documents: a `SkillRow`
//! is a [`RawSkill`], a `SkillDraft` a [`RawDraft`], an `ArchivedSkill` a
//! [`RawArchived`], a `ToolServer` a [`RawTool`]; `notes` and `probed` go
//! through as they are. Text is raw (the page cleans and bounds it).
//!
//! What the client does not carry, and so this adapter cannot pass on:
//!
//! - **A partial flag.** The wire type of either answer has no such field. What
//!   the server sends when it could not read part of the data is a line in
//!   `notes` ("skills not read: ...", "plugins not read: ..."), and the model
//!   reads any note as "this answer may be incomplete" (a badge of `Unknown`,
//!   never a zero). So the server's `notes` reach the model as they are (no
//!   filter, no reordering, empty lines included) up to [`MAX_NOTES`], and a
//!   cut is itself a note (see Bounds). If
//!   the server ever gets a separate flag, the client type needs the field
//!   first.
//! - **Notes cover the skills audit only (server side, #1117).** `GET /skills`
//!   adds a "skills not read" note when the audit of the skill files fails, but
//!   it drops an unreadable draft or archive entry without a note. A store with
//!   one corrupt draft answers no drafts and no notes, and the page then says
//!   "nothing to review". The adapter cannot see that; the server has to say it.
//! - Unmapped on purpose, because the model has no field for them: a skill's
//!   `approved_hash`, `source` (its text form `source_text` is used) and
//!   `history`; a draft's none; an archived skill's `entry`; a tool's
//!   `manifest_hash` and `lock` (they stay at the terminal).
//!
//! # Bounds
//!
//! The model bounds every string it shows but keeps every row, so the adapter
//! bounds the counts: at most [`MAX_ROWS`] skills, drafts, archived skills and
//! tools, [`MAX_NOTES`] notes, [`MAX_WARNINGS`] warnings per draft and
//! [`MAX_SECRETS`] secret names per tool. Whatever is left out is said in an
//! added note ("N more skills not shown"), which the model reads as a partial
//! answer: the badge is `Unknown` and the page never says "nothing to review"
//! for rows that were dropped. The first rows are kept, in the server's order.
//!
//! # Late replies
//!
//! A call gets a [`LoadId`] from [`LoadIds`] when it starts, and only the reply
//! of the latest call of a tab reaches the page, so an older answer that
//! arrives last cannot show a changed skill as approved.

mod load;

#[cfg(test)]
mod bounds_tests;
#[cfg(test)]
mod tests;

pub use load::{LoadId, LoadIds};

use tole_client::{
    ArchivedSkill, Error, SkillDraft, SkillRow, SkillsState, ToolServer, ToolsState,
};
use tole_desktop_model::{RawArchived, RawDraft, RawSkill, RawTool, SkillsPageState};

/// A `SkillRow` as the model's skill: `name`, `state`, `status`, `source_text`,
/// `approved_at` and `hash`.
pub fn raw_skill(row: &SkillRow) -> RawSkill {
    RawSkill {
        name: row.name.clone(),
        state: row.state.clone(),
        status: row.status.clone(),
        source_text: row.source_text.clone(),
        approved_at: row.approved_at.clone(),
        hash: row.hash.clone(),
    }
}

/// Most rows of each list (skills, drafts, archived skills, tools) kept.
pub const MAX_ROWS: usize = 1_000;
/// Most notes of one answer kept (before the note that says what was cut).
pub const MAX_NOTES: usize = 50;
/// Most warnings of one draft kept.
pub const MAX_WARNINGS: usize = 50;
/// Most secret names of one tool kept.
pub const MAX_SECRETS: usize = 100;

/// The first `max` items mapped, and how many were left out.
fn capped<T, U>(items: &[T], max: usize, map: impl Fn(&T) -> U) -> (Vec<U>, usize) {
    let kept = items.iter().take(max).map(map).collect();
    (kept, items.len().saturating_sub(max))
}

/// The notes of an answer, capped, and a closing note for each thing left out
/// (`(count, what)` pairs; a zero count says nothing).
fn bounded_notes(notes: &[String], cut: &[(usize, &str)]) -> Vec<String> {
    let mut out: Vec<String> = notes.iter().take(MAX_NOTES).cloned().collect();
    let dropped = notes.len().saturating_sub(MAX_NOTES);
    let more = std::iter::once((dropped, "notes"))
        .chain(cut.iter().copied())
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} more {what} not shown"));
    out.extend(more);
    out
}

/// A `SkillDraft` as the model's draft, field for field, with at most
/// [`MAX_WARNINGS`] warnings.
pub fn raw_draft(draft: &SkillDraft) -> RawDraft {
    RawDraft {
        id: draft.id.clone(),
        name: draft.name.clone(),
        description: draft.description.clone(),
        created_at: draft.created_at.clone(),
        hash: draft.hash.clone(),
        tainted: draft.tainted,
        warnings: draft.warnings.iter().take(MAX_WARNINGS).cloned().collect(),
        nearest: draft.nearest.clone(),
    }
}

/// An `ArchivedSkill` as the model's: `skill`, `at` and `reason`.
pub fn raw_archived(archived: &ArchivedSkill) -> RawArchived {
    RawArchived {
        skill: archived.skill.clone(),
        at: archived.at.clone(),
        reason: archived.reason.clone(),
    }
}

/// A `ToolServer` as the model's tool: everything the page shows, with at most
/// [`MAX_SECRETS`] secret names. `trust` is
/// the server's word; the model checks it against the `state` sentence and
/// reads an unknown word as not trusted.
pub fn raw_tool(server: &ToolServer) -> RawTool {
    RawTool {
        trust: server.trust.clone(),
        name: server.name.clone(),
        state: server.state.clone(),
        tier: server.tier.clone(),
        source: server.source.clone(),
        version: server.version.clone(),
        approved_at: server.approved_at.clone(),
        secrets: server.secrets.iter().take(MAX_SECRETS).cloned().collect(),
        sandbox: server.sandbox.clone(),
    }
}

/// The four inputs of `SkillsPageState::skills_loaded`, in order, bounded (see
/// the module docs): what was left out is a closing note.
pub fn skills_input(
    state: &SkillsState,
) -> (Vec<RawSkill>, Vec<RawDraft>, Vec<RawArchived>, Vec<String>) {
    let (skills, skills_cut) = capped(&state.skills, MAX_ROWS, raw_skill);
    let (drafts, drafts_cut) = capped(&state.drafts, MAX_ROWS, raw_draft);
    let (archived, archived_cut) = capped(&state.archived, MAX_ROWS, raw_archived);
    let warnings_cut = (state.drafts.iter().take(MAX_ROWS))
        .map(|d| d.warnings.len().saturating_sub(MAX_WARNINGS))
        .fold(0usize, usize::saturating_add);
    let notes = bounded_notes(
        &state.notes,
        &[
            (skills_cut, "skills"),
            (drafts_cut, "drafts"),
            (archived_cut, "archived skills"),
            (warnings_cut, "draft warnings"),
        ],
    );
    (skills, drafts, archived, notes)
}

/// The three inputs of `SkillsPageState::tools_loaded`, in order, bounded the
/// same way.
pub fn tools_input(state: &ToolsState) -> (Vec<RawTool>, Vec<String>, bool) {
    let (tools, tools_cut) = capped(&state.servers, MAX_ROWS, raw_tool);
    let secrets_cut = (state.servers.iter().take(MAX_ROWS))
        .map(|t| t.secrets.len().saturating_sub(MAX_SECRETS))
        .fold(0usize, usize::saturating_add);
    let notes = bounded_notes(
        &state.notes,
        &[(tools_cut, "tools"), (secrets_cut, "tool secret names")],
    );
    (tools, notes, state.probed)
}

/// The result of `Client::skills` into the page, unguarded (the guarded form is
/// [`LoadIds::apply_skills`]): the answer replaces the
/// Skills tab (`notes` included, so a partial answer is a partial page), and an
/// error empties it and says why.
fn apply_skills(page: &mut SkillsPageState, result: &Result<SkillsState, Error>) {
    match result {
        Ok(state) => {
            let (skills, drafts, archived, notes) = skills_input(state);
            page.skills_loaded(skills, drafts, archived, notes);
        }
        Err(error) => page.skills_failed(&error.to_string()),
    }
}

/// The result of `Client::tools` into the page, the same way (guarded:
/// [`LoadIds::apply_tools`]).
fn apply_tools(page: &mut SkillsPageState, result: &Result<ToolsState, Error>) {
    match result {
        Ok(state) => {
            let (tools, notes, probed) = tools_input(state);
            page.tools_loaded(tools, notes, probed);
        }
        Err(error) => page.tools_failed(&error.to_string()),
    }
}
