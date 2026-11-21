//! The queue reducer: the Waiting and Decided lists of the Approvals page.
//!
//! What the queue knows, and what it does not:
//!
//! - The list is **Unknown** until a [`ApprovalsAction::Snapshot`] of
//!   [`Scope::All`] arrives that answers the current
//!   [`ApprovalsAction::FetchStarted`], and again after
//!   [`ApprovalsAction::FetchFailed`] or [`ApprovalsAction::ConnectionLost`]
//!   (frames may have been missed). While it is unknown the sidebar badge is
//!   [`Badge::Unknown`](crate::skills::Badge::Unknown) (never a zero) and the
//!   message box of a task with nothing listed is [`ComposerState::Checking`]:
//!   disabled, with its own hint, until a fetch succeeds. The adapter must
//!   retry a failed fetch, or the box stays disabled.
//! - Every fetch has a [`FetchId`] the adapter picks (a counter) and passes to
//!   `FetchStarted`, and back with its `Snapshot` or `FetchFailed`. Only the
//!   reply of the **current** fetch of a scope (the last started, not
//!   invalidated by `ConnectionLost`) is trusted: it makes the list known,
//!   closes what it omits and accounts for refusals, overflow and the stale
//!   flag. Any other reply (a superseded fetch, one that began before a loss,
//!   one that was never started) still merges its rows (it only adds and
//!   updates) and leaves the state as unknown as it was; a failure of such a
//!   fetch is ignored. So a violation of the adapter contract always fails
//!   toward Unknown, never toward a count or an open box. When a second fetch
//!   starts while one runs, the older one's mark of "seen before" is kept.
//! - A request whose status this version does not know may be pending: while
//!   one is listed the badge is `Unknown` and its task is `Checking`, until a
//!   known status arrives (or its wait is over).
//!
//! Adapter contract (the reducer has no clock and no network; the `u64` of an
//! action is `now_ms` of the adapter):
//!
//! - `approval_requested` repeats after every reconnect: rows are keyed by
//!   approval id and the first row of an id stays, so a request never changes
//!   under a person who is reading it.
//! - An `approval_resolved` missed while disconnected is never sent: after a
//!   reconnect the adapter sends `ConnectionLost`, then `FetchStarted(scope,
//!   id)` before it calls `GET /approvals` and a `Snapshot` (or `FetchFailed`)
//!   with the reply and the same id. A snapshot closes a waiting request it
//!   omits only when the queue saw it before the fetch started: a request the
//!   stream announced after `FetchStarted` is newer than the reply and stays.
//!   What the current snapshot lists as pending is the server's word and is
//!   shown: it revives a guess (an expiry by the local clock, which may run
//!   ahead by the skew, gets that much grace) and an id trimmed from the list
//!   as a guess; only an id the server decided with a known status stays
//!   decided. A live frame does not get that trust.
//! - The first server answer wins and is final, except an outcome the local
//!   clock, a missing row or a status this version does not know only guessed
//!   (`provisional`): a later server word replaces it, and a `pending` row of
//!   the server revives a guess that is still inside its wait.
//! - A request waits up to 900 s, then it counts as denied: a
//!   [`ApprovalsAction::Tick`] moves it to Decided as Expired without a click,
//!   also one whose answer was sent and never confirmed. A `409` on an answer
//!   leaves the request as "no longer waiting" (nobody can answer it again)
//!   until the server's word or the next snapshot says otherwise.
//! - [`ApprovalsState::is_stale`] asks for a `GET /approvals` of everything
//!   and is false while one runs: after a `409`, and when room frees while
//!   requests over the cap are uncounted (the fetch lists them, and they get a
//!   card before they expire). Only the current reply of `Scope::All` clears
//!   it. The adapter must send `FetchFailed` when a fetch fails, times out or
//!   is cancelled, or no new one is asked for.
//!
//! Bounds: a request is cut to size and checked on the way in ([`ingest`]; ids
//! outside `[A-Za-z0-9_-]` or too long are refused and counted, and the task
//! they name stays `Checking`), at most [`MAX_WAITING`] waiting requests are
//! kept (more are only counted, and the badge then is `Unknown`),
//! [`MAX_DECIDED`] decided ones, and the ids of trimmed decided requests are
//! remembered ([`MAX_TOMBSTONES`]) so a repeated frame never brings one back as
//! waiting. History is kept and shown in the order it was decided here (a
//! counter, not the clock), above what a snapshot imported; the owner's own
//! recent decisions go last. `apply` never copies the state, and a list row is worked out
//! without building the card.
//!
//! Selection: the selected request is the card the page draws, and it is
//! always one the shown tab lists. A request is selected by the user, or
//! automatically only when it appears while nothing is selected (or the user
//! changes tab). When the selected request stops waiting the selection becomes
//! `None`: it never jumps to another request, so no key can act on a card the
//! user did not read. This holds inside one action too: a snapshot that closes
//! the selected card and lists a new waiting one leaves nothing selected. The
//! page then shows its "no card selected" state (the Waiting list is there to
//! pick from), and never answers on its own. Keys and buttons must call [`ApprovalsState::answer`]
//! with the id of the card the user acted on, never with whatever
//! `selected()` is when the event is handled; with no selection a key answers
//! nothing.

mod answer;
mod fetch;
mod flow;
mod ingest;
mod view;

use std::collections::{HashMap, HashSet, VecDeque};

use super::kind::{Care, Wording};
use super::request::{ApprovalRequest, Decision, Outcome};
use super::strings;
use super::wait::Wait;

/// Most decided requests kept; the oldest are dropped first. Waiting requests
/// are never dropped: hiding one would hide a request that needs an answer.
pub const MAX_DECIDED: usize = 200;
/// Most waiting requests kept in full. A further one is only counted
/// (`waiting_at_least`), and the badge says `Unknown` while any is uncounted.
pub const MAX_WAITING: usize = 200;
/// Most ids of trimmed decided requests remembered, oldest first.
pub const MAX_TOMBSTONES: usize = 2_000;
/// Most requests over [`MAX_WAITING`] tracked by id; past it only a flag.
const MAX_OVERFLOW: usize = 1_000;
/// Most sessions whose own list is remembered as known.
const MAX_KNOWN_SESSIONS: usize = 256;
/// Most refused rows remembered one by one.
const MAX_REFUSALS: usize = 256;

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
    /// The requests of this task are not known (not loaded, the load failed
    /// or the connection was lost): the box does not claim that nothing waits.
    /// It opens when a fetch succeeds.
    Checking,
}

impl ComposerState {
    pub fn is_disabled(self) -> bool {
        self != ComposerState::Open
    }

    /// The placeholder of a disabled box.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            ComposerState::Open => None,
            ComposerState::WaitingForAnswer { .. } => Some(strings::COMPOSER_WAITING_HINT),
            ComposerState::Checking => Some(strings::COMPOSER_CHECKING_HINT),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Waiting,
    Decided,
}

/// The identity of one fetch, chosen by the adapter (a counter): passed to
/// [`ApprovalsAction::FetchStarted`] and back with the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FetchId(pub u64);

/// What a snapshot or a fetch covers: a waiting request missing from a
/// snapshot that covers it is no longer waiting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// A whole list from the server: the reply of the fetch with this id.
    Snapshot(Vec<ApprovalRequest>, Scope, FetchId, u64),
    /// The adapter is about to call `GET /approvals` for this scope. Frames
    /// that arrive from now on are newer than the reply.
    FetchStarted(Scope, FetchId),
    /// The fetch with this id failed: what is known of its scope is unknown
    /// again. Ignored when it is not the current fetch of the scope.
    FetchFailed(Scope, FetchId),
    /// The stream dropped: frames may have been missed, so nothing is known
    /// until the next successful fetch.
    ConnectionLost,
    /// Time passed.
    Tick(u64),
    Select(Option<String>),
    SelectTab(Tab),
    /// The decision call of this id did not complete (`now_ms` is the time of
    /// the failure); `true` when it was a `409` (someone answered first). The
    /// request waits again only while it is still pending and in time,
    /// otherwise it is Expired; a conflict makes the list stale.
    AnswerFailed(String, bool, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerError {
    Unknown,
    /// Decided, or an answer is already on its way.
    NotWaiting,
    /// Its time is up: it counts as "Don't allow" and cannot be allowed.
    Expired,
}

/// The order of decided requests, oldest first: what a snapshot imported
/// (`false`, by the time the request was made) before what was decided here
/// (`true`, by a counter, so a clock that steps back changes nothing).
type Rank = (bool, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Waiting {
        answering: Option<Decision>,
    },
    Decided {
        outcome: Outcome,
        /// How recent (see [`Rank`]): orders the history and what it drops first.
        rank: Rank,
        provisional: bool,
        /// The time ran out with an answer on its way and no reply.
        answer_lost: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    request: ApprovalRequest,
    wait: Wait,
    phase: Phase,
    /// When the queue first saw it (or last revived it): a snapshot only
    /// closes what it saw before its fetch started.
    seq: u64,
}

/// A waiting request beyond [`MAX_WAITING`]: only enough kept to count it,
/// to tell its task's composer and to let it expire.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Overflow {
    session: Option<String>,
    wait: Wait,
    seq: u64,
}

/// A pending-looking row that was refused: a request nobody can see or answer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refusal {
    seq: u64,
    /// The task it names, when its session id is usable.
    session: Option<String>,
    /// Its session id is unusable (or the refusals overflowed): any task may
    /// be the one.
    any_session: bool,
}

/// The ids of decided requests trimmed from the list, oldest first, and
/// whether each was only a guess when it went.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Tombstones {
    ids: HashMap<String, bool>,
    order: VecDeque<String>,
}

/// An outstanding fetch: the `seq` the queue had when it began, and its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    seq: u64,
    id: FetchId,
}

/// One row of a list. `id` and `session_id` passed the id charset check on
/// the way in, so they are safe to draw.
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
    entries: HashMap<String, Entry>,
    selected: Option<String>,
    tab: Tab,
    stale: bool,
    /// The whole list is known (a snapshot of everything since the last loss).
    known_all: bool,
    /// Sessions whose own list is known without the whole one.
    known_sessions: HashSet<String>,
    /// The current fetch of each scope being fetched.
    marks: HashMap<Scope, Mark>,
    seq: u64,
    /// Counts the requests decided here (see [`Rank`]).
    decided_seq: u64,
    overflow: HashMap<String, Overflow>,
    /// `seq` of the last time more requests overflowed than [`MAX_OVERFLOW`]
    /// can name; a full snapshot that was started after it rebuilds the list.
    saturated: Option<u64>,
    /// Pending-looking rows refused (a bad id or field) and not yet accounted
    /// for by a full snapshot started after them.
    refusals: Vec<Refusal>,
    tombstones: Tombstones,
}

impl ApprovalsState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one action; `true` when something a view shows changed. The
    /// state is changed in place, never copied.
    pub fn apply(&mut self, action: ApprovalsAction) -> bool {
        let changed = match action {
            ApprovalsAction::Requested(request, now) => self.requested(request, now),
            ApprovalsAction::Resolved(request, now) => self.resolved(request, now),
            ApprovalsAction::Snapshot(rows, scope, id, now) => self.snapshot(rows, scope, id, now),
            ApprovalsAction::FetchStarted(scope, id) => self.fetch_started(scope, id),
            ApprovalsAction::FetchFailed(scope, id) => self.fetch_failed(&scope, id),
            ApprovalsAction::ConnectionLost => self.connection_lost(),
            ApprovalsAction::Tick(now) => self.expire(now),
            ApprovalsAction::Select(id) => self.select(id),
            ApprovalsAction::SelectTab(tab) => {
                let moved = std::mem::replace(&mut self.tab, tab) != tab;
                moved | self.drop_selection() | self.auto_select()
            }
            ApprovalsAction::AnswerFailed(id, conflict, now) => {
                self.answer_failed(&id, conflict, now)
            }
        };
        let dropped = self.drop_selection();
        let refresh = self.refresh_if_room();
        changed | dropped | refresh
    }

    /// The tab shown; Waiting until the user picks one.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The adapter should refresh with `GET /approvals`: an answer was refused
    /// as a conflict, or room freed while requests over the cap are uncounted.
    /// False while a fetch of everything runs. Cleared by its snapshot.
    pub fn is_stale(&self) -> bool {
        self.stale && !self.marks.contains_key(&Scope::All)
    }

    /// Rows refused and not yet accounted for by a full snapshot started
    /// after them (an id outside the charset, a field too long to be true): a
    /// protocol problem the adapter may log. While any is counted the badge is
    /// `Unknown` and the tasks they may belong to are `Checking`.
    pub fn refused(&self) -> usize {
        self.refusals.len()
    }
}

fn waiting(e: &Entry) -> bool {
    matches!(e.phase, Phase::Waiting { .. })
}
