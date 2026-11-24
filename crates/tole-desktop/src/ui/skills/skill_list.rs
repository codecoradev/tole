//! The Skills tab: what waits for review, what is approved, what is not used
//! and what the tidy-up archived, each card from the model's fields.
//!
//! A draft that the session wrote after reading outside text, or that the
//! checks warned about, says so on its card and has the amber border. Nothing
//! here approves anything: the footnote tells where that is done.

use super::cards::{card, heading, mono, small, tag, title_row};
use super::parts::{caption, line, plain};
use super::words;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::tokens::{size, Tone};
use tole_desktop_model::{ApprovedSkill, ArchivedSkill, BlockedSkill, ReviewItem};

fn review_card(item: &ReviewItem) -> impl IntoElement {
    let care = item.needs_care();
    let mut rows = vec![
        title_row(
            &item.name,
            tag(words::review_tag(item.reason), Tone::Attention),
        )
        .into_any_element(),
        line(&words::review_line(item)).into_any_element(),
    ];
    // What the draft says about itself is the model's text, from a session that
    // may have read outside text: labelled and quoted, never as the card's own.
    if let Some(words_of_draft) = item.description.as_ref().filter(|d| !d.is_empty()) {
        rows.push(caption(words::DRAFT_WORDS_LABEL).into_any_element());
        rows.push(mono(&words::quoted(words_of_draft)).into_any_element());
    }
    if item.tainted {
        rows.push(small(words::TAINTED_WARNING, Tone::Attention).into_any_element());
    }
    rows.extend(
        item.warnings
            .iter()
            .map(|w| small(w, Tone::Attention).into_any_element()),
    );
    if let Some(nearest) = &item.nearest {
        rows.push(caption(&words::nearest_line(nearest)).into_any_element());
    }
    if let Some(at) = &item.created_at {
        rows.push(caption(&words::suggested_line(at)).into_any_element());
    }
    if let Some(id) = &item.draft_id {
        rows.push(mono(&words::draft_id_line(id)).into_any_element());
    }
    if let Some(hash) = &item.hash {
        rows.push(mono(&words::hash_line(hash)).into_any_element());
    }
    card(care, rows)
}

fn approved_card(skill: &ApprovedSkill) -> impl IntoElement {
    let mut rows = vec![
        title_row(&skill.name, tag(words::TAG_APPROVED, Tone::Good)).into_any_element(),
        line(&words::approved_line(skill)).into_any_element(),
    ];
    if skill.unrecorded {
        rows.push(small(words::UNRECORDED_NOTE, Tone::Attention).into_any_element());
    }
    // An approval missing from the history is a loaded skill that needs the
    // owner once more: it gets the border of a card that asks.
    card(skill.unrecorded, rows)
}

fn blocked_card(skill: &BlockedSkill) -> impl IntoElement {
    card(
        false,
        [
            title_row(&skill.name, tag(words::TAG_BLOCKED, Tone::Bad)).into_any_element(),
            line(&words::blocked_reason(&skill.reason)).into_any_element(),
            mono(&skill.status).into_any_element(),
        ],
    )
}

fn archived_card(skill: &ArchivedSkill) -> impl IntoElement {
    card(
        false,
        [
            title_row(&skill.name, tag(words::TAG_ARCHIVED, Tone::Neutral)).into_any_element(),
            line(&words::archived_line(skill)).into_any_element(),
        ],
    )
}

impl ShellView {
    /// The four groups of the Skills tab, in the order the owner acts on them.
    /// "Nothing is waiting" is said only when the model says the answer was
    /// complete and empty: a partial read says nothing of the kind.
    pub(super) fn skill_groups(&self) -> impl IntoElement {
        let page = &self.skills.page;
        let skills = &page.skills;
        let gap = len(size::GUTTER);
        let mut groups = Vec::new();
        if !skills.review.is_empty() {
            let title = words::group_caption(words::GROUP_REVIEW, skills.review.len());
            groups.push(heading(&title, Tone::Attention).into_any_element());
            groups.extend(
                skills
                    .review
                    .iter()
                    .map(|i| review_card(i).into_any_element()),
            );
        } else if page.skills_inbox_is_empty() {
            groups.push(plain(words::NOTHING_TO_REVIEW).into_any_element());
        }
        if !skills.approved.is_empty() {
            let title = words::group_caption(words::GROUP_APPROVED, skills.approved.len());
            groups.push(heading(&title, Tone::Neutral).into_any_element());
            groups.extend(
                skills
                    .approved
                    .iter()
                    .map(|s| approved_card(s).into_any_element()),
            );
        }
        if !skills.blocked.is_empty() {
            let title = words::group_caption(words::GROUP_BLOCKED, skills.blocked.len());
            groups.push(heading(&title, Tone::Bad).into_any_element());
            groups.extend(
                skills
                    .blocked
                    .iter()
                    .map(|s| blocked_card(s).into_any_element()),
            );
        }
        if !skills.archived.is_empty() {
            let title = words::group_caption(words::GROUP_ARCHIVED, skills.archived.len());
            groups.push(heading(&title, Tone::Neutral).into_any_element());
            groups.extend(
                skills
                    .archived
                    .iter()
                    .map(|s| archived_card(s).into_any_element()),
            );
            groups.push(caption(words::TIDY_UP_NOTE).into_any_element());
        }
        div().flex().flex_col().gap(gap).children(groups)
    }
}
