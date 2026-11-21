//! The card of one approval request (boards S4, S5 and S11): the same card in
//! the task and in the detail pane of the Approvals page.
//!
//! [`ApprovalCard::build`] returns everything the card shows, already in the
//! words of [`super::strings`]. It only ever gets more careful than the
//! server's codes: the care level is the highest of the kind, the effect, the
//! risk tier and "outside the folder", and an unknown code is the highest.

use super::kind::{effect_care, risk_care, Care, Wording};
use super::request::{ApprovalRequest, Decision, Outcome};
use super::strings as s;
use super::summary::{bound, safe_line, Preview, MAX_DETAIL_CHARS, MAX_DISPLAY_CHARS};
use super::wait::Wait;

/// The two buttons of a card, in the order they are drawn. There is no
/// "always allow" button for any request, whatever its risk tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    DontAllow,
    AllowOnce,
}

pub const BUTTONS: [Button; 2] = [Button::DontAllow, Button::AllowOnce];

impl Button {
    pub fn decision(self) -> Decision {
        match self {
            Button::DontAllow => Decision::DontAllow,
            Button::AllowOnce => Decision::AllowOnce,
        }
    }
}

/// What the card is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Waiting,
    /// An answer was sent and the server has not confirmed it.
    Answering(Decision),
    Decided(Outcome),
}

/// One row of "Show technical details".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailRow {
    pub label: &'static str,
    pub value: String,
    /// The value was cut (head and tail kept; the text marks the cut).
    pub cut: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCard {
    pub id: String,
    pub session_id: Option<String>,
    pub wording: Wording,
    pub care: Care,
    /// The kind, and where it is if the server did not confirm the folder.
    pub title: String,
    /// A file action the server did not confirm inside the working folder.
    pub outside_folder: bool,
    /// The line under the title: a path, a command or a tool name, safe to
    /// draw; `subject_cut` when the server cut it (the text marks the cut).
    pub subject: String,
    pub subject_cut: bool,
    pub where_line: &'static str,
    pub effect_line: &'static str,
    /// "Tole wrote this text, you did not type it", for a file Tole will write.
    pub warning: Option<&'static str>,
    pub preview: Preview,
    /// Says that the preview is not the whole of it.
    pub preview_note: Option<&'static str>,
    pub waits_line: Option<String>,
    pub left_short: Option<String>,
    pub origin: Option<String>,
    pub also_told: Vec<String>,
    pub details: Vec<DetailRow>,
    /// The fine print under the buttons (and the note about deleting).
    pub notes: Vec<&'static str>,
    pub state: CardState,
    /// Why the card is not answerable; `None` while it waits.
    pub state_note: Option<&'static str>,
}

impl ApprovalCard {
    /// Builds the card of `request` in `state`; `wait` and `now_ms` give the
    /// time left. A card is answerable only for a request that is still
    /// pending and whose time is not up: a status other than `pending` shows
    /// as decided whatever `state` says (the server's answer wins over a
    /// local guess such as an expiry).
    pub fn build(request: &ApprovalRequest, state: CardState, wait: Wait, now_ms: u64) -> Self {
        let summary = request.summary_or_unknown();
        let wording = summary.wording();
        let outside = summary.outside_folder();
        let consistent = summary.consistent();
        let mut care = wording.care().max(effect_care(&summary.effect));
        care = care.max(request.risk.as_deref().map_or(Care::Notice, risk_care));
        if outside || !consistent {
            care = Care::Danger;
        }
        let state = match Outcome::of_status(&request.status) {
            Some(o) => CardState::Decided(o),
            None => state,
        };
        let decided = matches!(state, CardState::Decided(_));
        let preview = summary.preview();
        let secs = wait.remaining_secs(now_ms);
        let writes_text = matches!(wording, Wording::CreateFile | Wording::ReplaceFile);
        let mut notes = vec![s::FINE_PRINT];
        if wording == Wording::DeleteFile {
            notes.push(s::DELETE_NOTE);
        }
        let state_note = match state {
            CardState::Waiting if wait.expired(now_ms) => Some(s::EXPIRED_NOTE),
            CardState::Waiting => None,
            CardState::Answering(_) => Some(s::ANSWERING),
            CardState::Decided(o) => s::outcome_note(o),
        };
        let origin = request.child_node.as_deref().map(safe_line);
        let reported = |v: Option<String>| v.unwrap_or_else(|| s::NOT_REPORTED.to_owned());
        let row = |label, value| DetailRow {
            label,
            value,
            cut: false,
        };
        let (arguments, arguments_cut) = match request.description.as_deref() {
            Some(d) => bound(d, MAX_DETAIL_CHARS),
            None => (s::NOT_REPORTED.to_owned(), false),
        };
        let details = vec![
            row(s::DETAIL_TOOL, safe_line(&request.tool)),
            DetailRow {
                label: s::DETAIL_ARGUMENTS,
                value: arguments,
                cut: arguments_cut,
            },
            row(
                s::DETAIL_RISK,
                reported(request.risk.as_deref().map(|r| s::risk_label(r).into())),
            ),
            row(
                s::DETAIL_FINGERPRINT,
                reported(request.fingerprint.as_deref().map(safe_line)),
            ),
            row(s::DETAIL_KIND_CODE, safe_line(&summary.kind)),
            row(s::DETAIL_EFFECT_CODE, safe_line(&summary.effect)),
            row(s::DETAIL_REQUEST_ID, safe_line(&request.id)),
        ];
        let subject = bound(&summary.target, MAX_DISPLAY_CHARS);
        ApprovalCard {
            id: request.id.clone(),
            session_id: request.session_id.clone(),
            wording,
            care,
            title: match outside {
                true => format!("{}{}", s::title(wording), s::OUTSIDE_SUFFIX),
                false => s::title(wording).to_owned(),
            },
            outside_folder: outside,
            subject: subject.0,
            subject_cut: summary.target_truncated || subject.1,
            where_line: s::where_line(wording, outside),
            effect_line: match consistent {
                true => s::effect_line(&summary.effect),
                false => s::EFFECT_MISMATCH,
            },
            warning: (writes_text && !decided && !preview.lines.is_empty())
                .then_some(s::WARNING_MODEL_TEXT),
            preview_note: preview.is_cut().then_some(s::PREVIEW_CUT),
            preview,
            waits_line: (!decided).then(|| s::waits_line(secs)),
            left_short: (!decided).then(|| s::left_short(secs)),
            origin: origin
                .map(|n| s::from_helper(&n))
                .or_else(|| request.is_child().then(|| s::from_helper(""))),
            also_told: request
                .notified
                .iter()
                .map(|n| s::also_told(&safe_line(n)))
                .collect(),
            details,
            notes,
            state,
            state_note,
        }
    }

    /// Whether the two buttons work: only while the card waits and its time is
    /// not up.
    pub fn answerable(&self) -> bool {
        self.state == CardState::Waiting && self.state_note.is_none()
    }
}
