//! The Skills tab: waiting for review, approved, blocked, archived by the
//! tidy-up.

use crate::plain::printable;

/// A skill of `GET /skills` (mapping of the client's `SkillRow`: `name`,
/// `state`, `status`, `source_text`, `approved_at`, `hash`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSkill {
    pub name: String,
    pub state: String,
    pub status: String,
    pub source_text: String,
    pub approved_at: Option<String>,
    pub hash: Option<String>,
}

/// A draft of `GET /skills` (the client's `SkillDraft`, field for field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDraft {
    pub id: String,
    pub name: String,
    pub description: String,
    pub created_at: String,
    pub hash: String,
    pub tainted: bool,
    pub warnings: Vec<String>,
    pub nearest: Option<String>,
}

/// An archived skill of `GET /skills` (the client's `ArchivedSkill`: `skill`,
/// `at`, `reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawArchived {
    pub skill: String,
    pub at: String,
    pub reason: String,
}

/// Why something waits for the owner's review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewReason {
    /// Tole wrote it as a draft after a task.
    SuggestedByTole,
    /// Approved once; its files differ now, so Tole stopped using it.
    ChangedSinceApproval,
    /// Found in a skills folder, never approved.
    NotApprovedYet,
}

/// Something that waits for the owner to read it and decide. A draft carries
/// its description, warnings and whether the session read untrusted text; a
/// skill that changed or was never approved has only a name and a hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewItem {
    pub name: String,
    pub reason: ReviewReason,
    pub draft_id: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub hash: Option<String>,
    /// The session that proposed it read untrusted text.
    pub tainted: bool,
    pub warnings: Vec<String>,
    /// The existing skill it most resembles.
    pub nearest: Option<String>,
}

/// An approved skill. `unrecorded`: it loads, but its approval is missing from
/// the history ledger (approving again at a terminal records it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedSkill {
    pub name: String,
    pub approved_at: Option<String>,
    /// Where it came from, in the words of `tole skills` ("source ... pinned
    /// ..."); `None` when no source was recorded. Not an author: the route
    /// does not say who wrote a skill, nor when it was last used.
    pub source: Option<String>,
    pub unrecorded: bool,
}

/// Why a skill is not used and is not simply waiting for a first approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockedReason {
    /// The approval ledger disagrees with the approvals file: ignored.
    Refused,
    Unreadable,
    /// A state this model does not know, or one that contradicts its own
    /// status sentence.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedSkill {
    pub name: String,
    pub reason: BlockedReason,
    /// The sentence `tole skills` prints, for "Show technical details".
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedSkill {
    pub name: String,
    pub at: String,
    pub reason: String,
}

/// The Skills tab as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillsView {
    pub review: Vec<ReviewItem>,
    pub approved: Vec<ApprovedSkill>,
    pub blocked: Vec<BlockedSkill>,
    pub archived: Vec<ArchivedSkill>,
    /// What the server could not read ("skills not read: ..."). While any is
    /// present the lists above may be incomplete: never read them as "nothing
    /// is waiting".
    pub notes: Vec<String>,
}

impl SkillsView {
    /// Drafts first (oldest first, as sent), then skills that need a decision.
    pub fn new(
        skills: Vec<RawSkill>,
        drafts: Vec<RawDraft>,
        archived: Vec<RawArchived>,
        notes: Vec<String>,
    ) -> SkillsView {
        let mut view = SkillsView {
            notes: notes.iter().map(|n| printable(n, 200)).collect(),
            archived: archived
                .into_iter()
                .map(|a| ArchivedSkill {
                    name: printable(&a.skill, 80),
                    at: printable(&a.at, 40),
                    reason: printable(&a.reason, 300),
                })
                .collect(),
            ..SkillsView::default()
        };
        view.review = drafts.into_iter().map(draft).collect();
        for s in skills {
            let name = printable(&s.name, 80);
            match classify(&s.state, &s.status) {
                Class::Approved => view.approved.push(ApprovedSkill {
                    unrecorded: s.status.starts_with("approved, unrecorded"),
                    approved_at: s.approved_at.map(|t| printable(&t, 40)),
                    source: Some(printable(&s.source_text, 300)).filter(|t| !t.is_empty()),
                    name,
                }),
                Class::Review(reason) => view.review.push(ReviewItem {
                    name,
                    reason,
                    draft_id: None,
                    description: None,
                    created_at: None,
                    hash: s.hash.map(|h| printable(&h, 100)),
                    tainted: false,
                    warnings: Vec::new(),
                    nearest: None,
                }),
                Class::Blocked(reason) => view.blocked.push(BlockedSkill {
                    name,
                    reason,
                    status: printable(&s.status, 300),
                }),
            }
        }
        view
    }

    /// True when the server said part of the answer could not be read.
    pub fn is_partial(&self) -> bool {
        !self.notes.is_empty()
    }
}

fn draft(d: RawDraft) -> ReviewItem {
    ReviewItem {
        name: printable(&d.name, 80),
        reason: ReviewReason::SuggestedByTole,
        draft_id: Some(printable(&d.id, 80)),
        description: Some(printable(&d.description, 300)),
        created_at: Some(printable(&d.created_at, 40)),
        hash: Some(printable(&d.hash, 100)),
        tainted: d.tainted,
        warnings: d.warnings.iter().map(|w| printable(w, 300)).collect(),
        nearest: d.nearest.map(|n| printable(&n, 80)),
    }
}

enum Class {
    Approved,
    Review(ReviewReason),
    Blocked(BlockedReason),
}

/// The two sentences serve gives an approved skill (`skill::state`): exactly
/// `approved`, or `approved, unrecorded in the ledger (...)`. Anything else that
/// merely starts with the word (`approved-ish`) is not approved.
fn approved_status(status: &str) -> bool {
    status == "approved" || status.starts_with("approved, unrecorded in the ledger")
}

/// The `state` word of a skill decides, but a skill is approved only when its
/// status sentence says so too. A word nobody taught this function, or a pair
/// that disagrees, is blocked: never approved by accident.
fn classify(state: &str, status: &str) -> Class {
    match state {
        "approved" if approved_status(status) => Class::Approved,
        "approved" => Class::Blocked(BlockedReason::Unknown(format!(
            "state and status disagree: {}",
            printable(status, 80)
        ))),
        "changed" => Class::Review(ReviewReason::ChangedSinceApproval),
        "not_approved" => Class::Review(ReviewReason::NotApprovedYet),
        "refused" => Class::Blocked(BlockedReason::Refused),
        "unreadable" => Class::Blocked(BlockedReason::Unreadable),
        other => Class::Blocked(BlockedReason::Unknown(printable(other, 40))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, state: &str, status: &str) -> RawSkill {
        RawSkill {
            name: name.into(),
            state: state.into(),
            status: status.into(),
            source_text: String::new(),
            approved_at: Some("2026-11-01".into()),
            hash: Some("sha256:ab".into()),
        }
    }

    fn draft_raw(name: &str) -> RawDraft {
        RawDraft {
            id: "d1".into(),
            name: name.into(),
            description: "How to lay out the report\n".into(),
            created_at: "2026-11-17".into(),
            hash: "sha256:cd".into(),
            tainted: true,
            warnings: vec!["mentions a command".into()],
            nearest: Some("report".into()),
        }
    }

    #[test]
    fn skills_land_in_the_group_their_state_names() {
        let view = SkillsView::new(
            vec![
                skill("a", "approved", "approved"),
                skill(
                    "b",
                    "changed",
                    "changed since approval: refused until approved again",
                ),
                skill("c", "not_approved", "not approved: refused"),
                skill("d", "refused", "refused: approval ignored"),
                skill("e", "unreadable", "unreadable: bad file"),
            ],
            vec![draft_raw("weekly")],
            vec![RawArchived {
                skill: "old".into(),
                at: "t".into(),
                reason: "unused".into(),
            }],
            vec![],
        );
        let names = |r: &[ReviewItem]| {
            r.iter()
                .map(|i| (i.name.clone(), i.reason))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&view.review),
            [
                ("weekly".into(), ReviewReason::SuggestedByTole),
                ("b".into(), ReviewReason::ChangedSinceApproval),
                ("c".into(), ReviewReason::NotApprovedYet),
            ]
        );
        assert_eq!(view.approved.len(), 1);
        assert_eq!(view.blocked.len(), 2);
        assert_eq!(view.archived[0].name, "old");
        assert!(
            view.review[0].tainted
                && view.review[0].description.as_deref() == Some("How to lay out the report")
        );
    }

    /// Approved needs the word and the sentence; a lookalike or a new state
    /// is blocked, and an approval missing from the ledger still says so.
    #[test]
    fn nothing_is_approved_by_accident() {
        for (state, status) in [
            ("approved", "changed since approval"),
            ("approved", ""),
            ("approved", "approved-ish"),
            ("approved", "approved: refused by the sandbox"),
            ("Approved", "approved"),
            ("", "approved"),
            ("trusted", "approved"),
        ] {
            let view = SkillsView::new(vec![skill("x", state, status)], vec![], vec![], vec![]);
            assert!(
                view.approved.is_empty() && view.blocked.len() == 1,
                "{state}/{status}"
            );
            assert!(matches!(view.blocked[0].reason, BlockedReason::Unknown(_)));
        }
        let status = "approved, unrecorded in the ledger (loads; approve again to record it)";
        let view = SkillsView::new(vec![skill("x", "approved", status)], vec![], vec![], vec![]);
        assert!(view.approved[0].unrecorded);
    }

    #[test]
    fn a_partial_answer_is_not_an_empty_inbox() {
        let view = SkillsView::new(vec![], vec![], vec![], vec!["skills not read: x".into()]);
        assert!(view.review.is_empty() && view.is_partial());
        assert!(!SkillsView::default().is_partial());
    }
}
