//! The queue reducer: the Waiting and Decided lists of the Approvals page.
//!
//! - `approval_requested` repeats after every reconnect: rows are keyed by
//!   approval id and the first row of an id stays, so a request never changes
//!   under a person who is reading it.
//! - An `approval_resolved` missed while disconnected is never sent: after a
//!   reconnect the adapter feeds `GET /approvals` as a [`ApprovalsAction::Snapshot`].
//! - The first answer wins and is final, except an outcome the local clock or
//!   a missing row only guessed (`provisional`): the server's word replaces it.
//! - A request waits up to 900 s, then it counts as denied: a
//!   [`ApprovalsAction::Tick`] moves it to Decided as Expired without a click.
//!
//! The reducer has no clock: the `u64` of an action is `now_ms` of the adapter.

use super::card::{ApprovalCard, CardState};
use super::kind::{Care, Wording};
use super::request::{ApprovalRequest, Decision, DecisionIntent, Outcome};
use super::strings;
use super::wait::Wait;

/// Most decided requests kept; the oldest are dropped first. Waiting requests
/// are never dropped: hiding one would hide a request that needs an answer.
pub const MAX_DECIDED: usize = 200;

/// Whether the message box of a task works. A turn that stopped for an answer
/// cannot take a new message (board S4); the task view reads this, so it
/// cannot disagree with the queue. A delegated child's request has no task
/// here and does not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerState {
    Open,
    /// `requests` of this task wait; an answer sent and not confirmed counts.
    WaitingForAnswer {
        requests: usize,
    },
}

impl ComposerState {
    pub fn is_disabled(self) -> bool {
        self != ComposerState::Open
    }

    /// The placeholder of a disabled box.
    pub fn hint(self) -> Option<&'static str> {
        self.is_disabled().then_some(strings::COMPOSER_WAITING_HINT)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Waiting,
    Decided,
}

/// What a snapshot covers: a waiting request missing from a snapshot that
/// covers it is no longer waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `GET /approvals`.
    All,
    /// `GET /approvals?session=<id>`.
    Session(String),
}

/// Every action carries `now_ms` (the adapter's clock) where it needs the time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalsAction {
    /// An `approval_requested` frame, or a row of a list.
    Requested(ApprovalRequest, u64),
    /// An `approval_resolved` frame, or the reply of a decision.
    Resolved(ApprovalRequest, u64),
    /// A whole list from the server.
    Snapshot(Vec<ApprovalRequest>, Scope, u64),
    /// Time passed.
    Tick(u64),
    Select(Option<String>),
    SelectTab(Tab),
    /// The decision call of this id failed; `true` when it was a `409` (someone
    /// answered first). The request waits again; a conflict makes the list stale.
    AnswerFailed(String, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerError {
    Unknown,
    /// Decided, or an answer is already on its way.
    NotWaiting,
    /// Its time is up: it counts as "Don't allow" and cannot be allowed.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Waiting {
        answering: Option<Decision>,
    },
    Decided {
        outcome: Outcome,
        at_ms: u64,
        provisional: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    request: ApprovalRequest,
    wait: Wait,
    phase: Phase,
}

/// One row of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub session_id: Option<String>,
    pub wording: Wording,
    pub care: Care,
    /// The short title; it says so when the file is outside the folder.
    pub title: String,
    pub outside_folder: bool,
    pub subject: String,
    /// "14 min left", on a waiting row.
    pub left: Option<String>,
    /// On a decided row.
    pub outcome: Option<Outcome>,
    pub outcome_label: Option<&'static str>,
    pub selected: bool,
    pub from_helper: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApprovalsState {
    entries: Vec<Entry>,
    selected: Option<String>,
    tab: Tab,
    stale: bool,
}

impl ApprovalsState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one action; `true` when the state changed.
    pub fn apply(&mut self, action: ApprovalsAction) -> bool {
        let before = self.clone();
        match action {
            ApprovalsAction::Requested(request, now) => self.requested(request, now),
            ApprovalsAction::Resolved(request, now) => self.resolved(request, now),
            ApprovalsAction::Snapshot(rows, scope, now) => self.snapshot(rows, &scope, now),
            ApprovalsAction::Tick(now) => self.expire(now),
            ApprovalsAction::Select(id) => {
                self.selected = id.filter(|i| self.entries.iter().any(|e| e.request.id == *i));
            }
            ApprovalsAction::SelectTab(tab) => self.tab = tab,
            ApprovalsAction::AnswerFailed(id, conflict) => {
                if let Some(Phase::Waiting { answering }) = self.find(&id).map(|e| &mut e.phase) {
                    *answering = None;
                }
                self.stale |= conflict;
            }
        }
        *self != before
    }

    /// The user pressed a button. On `Ok` the adapter sends the intent; the
    /// card shows "sending" and no buttons until the server confirms with
    /// [`ApprovalsAction::Resolved`] or the call fails with
    /// [`ApprovalsAction::AnswerFailed`]. Refused when the request is not
    /// waiting, is being answered, or its time is up (it is then moved to
    /// Decided as Expired): a stale click never allows anything.
    pub fn answer(
        &mut self,
        id: &str,
        decision: Decision,
        now: u64,
    ) -> Result<DecisionIntent, AnswerError> {
        let e = self.find(id).ok_or(AnswerError::Unknown)?;
        if !matches!(e.phase, Phase::Waiting { answering: None }) {
            return Err(AnswerError::NotWaiting);
        }
        if e.wait.expired(now) {
            self.expire(now);
            return Err(AnswerError::Expired);
        }
        e.phase = Phase::Waiting {
            answering: Some(decision),
        };
        let fingerprint = e.request.fingerprint.clone();
        Ok(DecisionIntent {
            id: id.to_owned(),
            decision,
            fingerprint,
        })
    }

    /// How many requests wait for an answer: the sidebar badge
    /// (`ShellState::approvals_pending`).
    pub fn waiting_count(&self) -> usize {
        self.entries.iter().filter(|e| waiting(e)).count()
    }

    /// Whether the message box of `session_id` works.
    pub fn composer(&self, session_id: &str) -> ComposerState {
        let n = (self.entries.iter())
            .filter(|e| waiting(e) && e.request.session_id.as_deref() == Some(session_id))
            .count();
        if n == 0 {
            ComposerState::Open
        } else {
            ComposerState::WaitingForAnswer { requests: n }
        }
    }

    /// The tab shown; Waiting until the user picks one.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// An answer was refused as a conflict and no snapshot came since: the
    /// adapter should refresh with `GET /approvals`.
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// The rows of a tab: waiting ones soonest first, decided ones newest first.
    pub fn rows(&self, tab: Tab, now: u64) -> Vec<Row> {
        let mut list: Vec<&Entry> = (self.entries.iter())
            .filter(|e| waiting(e) == (tab == Tab::Waiting))
            .collect();
        list.sort_by_key(|e| match e.phase {
            Phase::Decided { at_ms, .. } => -i128::from(at_ms),
            Phase::Waiting { .. } => i128::from(e.wait.deadline_ms()),
        });
        list.into_iter().map(|e| self.row(e, now)).collect()
    }

    /// The card of a request, or of the selected one.
    pub fn card(&self, id: &str, now: u64) -> Option<ApprovalCard> {
        let e = self.entries.iter().find(|e| e.request.id == id)?;
        Some(ApprovalCard::build(&e.request, card_state(e), e.wait, now))
    }

    pub fn selected_card(&self, now: u64) -> Option<ApprovalCard> {
        self.card(self.selected.as_deref()?, now)
    }

    fn row(&self, e: &Entry, now: u64) -> Row {
        let card = ApprovalCard::build(&e.request, card_state(e), e.wait, now);
        let outcome = match e.phase {
            Phase::Decided { outcome, .. } => Some(outcome),
            Phase::Waiting { .. } => None,
        };
        Row {
            id: e.request.id.clone(),
            session_id: e.request.session_id.clone(),
            wording: card.wording,
            care: card.care,
            title: match card.outside_folder {
                true => format!(
                    "{}{}",
                    strings::list_title(card.wording),
                    strings::LIST_OUTSIDE_SUFFIX
                ),
                false => strings::list_title(card.wording).to_owned(),
            },
            outside_folder: card.outside_folder,
            subject: card.subject,
            left: card.left_short.filter(|_| outcome.is_none()),
            outcome,
            outcome_label: outcome.map(strings::outcome_label),
            selected: self.selected.as_deref() == Some(e.request.id.as_str()),
            from_helper: e.request.is_child(),
        }
    }

    fn requested(&mut self, request: ApprovalRequest, now: u64) {
        if let Some(outcome) = Outcome::of_status(&request.status) {
            return self.decide(request, outcome, now, false); // a decision, not a request
        }
        if self.entries.iter().any(|e| e.request.id == request.id) {
            return; // a repeat after a reconnect: the first row stays
        }
        if self.selected.is_none() {
            self.selected = Some(request.id.clone());
        }
        self.push(request, Phase::Waiting { answering: None }, now);
    }

    fn resolved(&mut self, request: ApprovalRequest, now: u64) {
        // A frame that says "pending" is not a decision.
        if let Some(outcome) = Outcome::of_status(&request.status) {
            self.decide(request, outcome, now, false);
        }
    }

    fn push(&mut self, request: ApprovalRequest, phase: Phase, now: u64) {
        let wait = Wait::new(request.created_at_ms, request.expires_in_secs, now);
        self.entries.push(Entry {
            request,
            wait,
            phase,
        });
    }

    /// Moves a request to Decided. The server's decision replaces a guess;
    /// any other decided request keeps its first answer, and a guess never
    /// replaces another decision.
    fn decide(&mut self, request: ApprovalRequest, outcome: Outcome, now: u64, guess: bool) {
        let phase = Phase::Decided {
            outcome,
            at_ms: now,
            provisional: guess,
        };
        match self.find(&request.id) {
            Some(e)
                if matches!(
                    e.phase,
                    Phase::Decided {
                        provisional: false,
                        ..
                    }
                ) => {}
            Some(e) if guess && !waiting(e) => {}
            Some(e) => e.phase = phase,
            None => self.push(request, phase, now),
        }
        self.trim_decided();
    }

    fn trim_decided(&mut self) {
        let over =
            (self.entries.iter().filter(|e| !waiting(e)).count()).saturating_sub(MAX_DECIDED);
        for _ in 0..over {
            let oldest = (self.entries.iter().enumerate().filter(|(_, e)| !waiting(e)))
                .min_by_key(|(_, e)| match e.phase {
                    Phase::Decided { at_ms, .. } => at_ms,
                    Phase::Waiting { .. } => u64::MAX,
                })
                .map(|(i, _)| i);
            if let Some(gone) = oldest.map(|i| self.entries.remove(i)) {
                if self.selected.as_deref() == Some(gone.request.id.as_str()) {
                    self.selected = None;
                }
            }
        }
    }

    fn snapshot(&mut self, rows: Vec<ApprovalRequest>, scope: &Scope, now: u64) {
        let seen: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        for request in rows {
            self.requested(request, now); // also takes a decided row
        }
        let in_scope = |e: &Entry| match scope {
            Scope::All => true,
            Scope::Session(id) => e.request.session_id.as_deref() == Some(id.as_str()),
        };
        let gone: Vec<ApprovalRequest> = (self.entries.iter())
            .filter(|e| waiting(e) && !seen.contains(&e.request.id) && in_scope(e))
            .map(|e| e.request.clone())
            .collect();
        for request in gone {
            self.decide(request, Outcome::Closed, now, true);
        }
        self.stale = false;
    }

    /// Requests whose time is up are Decided as Expired, not yet confirmed by
    /// the server (a later `Resolved` replaces it). One whose answer is on its
    /// way waits for the server's reply.
    fn expire(&mut self, now: u64) {
        for e in &mut self.entries {
            if matches!(e.phase, Phase::Waiting { answering: None }) && e.wait.expired(now) {
                e.phase = Phase::Decided {
                    outcome: Outcome::Expired,
                    at_ms: now,
                    provisional: true,
                };
            }
        }
        self.trim_decided();
    }

    fn find(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.request.id == id)
    }
}

fn waiting(e: &Entry) -> bool {
    matches!(e.phase, Phase::Waiting { .. })
}

fn card_state(e: &Entry) -> CardState {
    match e.phase {
        Phase::Waiting { answering: None } => CardState::Waiting,
        Phase::Waiting { answering: Some(d) } => CardState::Answering(d),
        Phase::Decided { outcome, .. } => CardState::Decided(outcome),
    }
}
