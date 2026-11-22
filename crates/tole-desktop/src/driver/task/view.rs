//! What the UI reads from the task driver: a cloned, plain-data view of the
//! model and of what the driver knows, with no toolkit type in it.

use tole_desktop_model::task::{
    visible, ApprovalInfo, Item, Link, Phase, SendMode, StatusRow, Visible,
};

use super::store::Shared;

/// Whether the task is connected to its serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// No stream has been up yet (the history is being read, or the serve has
    /// not answered).
    Connecting,
    Live,
    /// The stream dropped; it is opened again with a growing wait. Reads over
    /// REST (Stop, the cards) go on, but they do not make the link live again:
    /// nothing is sent until a stream item has.
    Lost,
    /// The driver ended (a thread died, or it was shut down).
    Stopped,
}

/// One task, as of one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskView {
    /// The status sentence's inputs: phase, counts, steps, `can_stop` and
    /// `send` (the model's rules, then the driver's: nothing can be sent while
    /// the history is read, while the previous prompt call is still out, or
    /// after the driver stopped).
    pub status: StatusRow,
    pub link: LinkState,
    /// The serve keeps refusing the stream (404 until the task is resumed,
    /// 429, `closed`): events are not followed, the stream is probed slowly.
    pub parked: bool,
    /// The history is still being read: the rows are the first pages only.
    pub loading: bool,
    /// The whole history was read (only then may a page say "nothing was
    /// changed").
    pub history_complete: bool,
    /// The rows of the window the UI last set with `Handle::set_viewport`.
    pub rows: Vec<Item>,
    /// Which rows those are, and the padding before and after them.
    pub window: Visible,
    pub total_rows: usize,
    /// Waiting for the owner, oldest first.
    pub approvals: Vec<ApprovalInfo>,
    /// The ids of `approvals` whose answer is queued or on its way: show their
    /// buttons as busy.
    pub deciding: Vec<String>,
    /// The ids of `approvals` that can only be answered elsewhere (the serve's
    /// id of the request is not certain, or two requests look the same once
    /// made printable): the card should say so and offer no buttons. Stop is
    /// offered while they wait.
    pub undecidable: Vec<String>,
    /// The last message was given up on (its call never answered) with no sign
    /// that the serve got it: it may or may not have been sent, and it may
    /// still run. Its words are back in `draft`; say so before a resend.
    pub last_send_uncertain: bool,
    /// Commands the driver refused so far (a send while a turn runs, an answer
    /// for a card it cannot answer, ...). `Handle` commands return `Ok` once
    /// queued: a UI keeps the text it typed until the transcript shows it, and
    /// reads a rise of this number as "the last command did nothing".
    pub ignored: u64,
    /// The queue was lost with the stream and not read again yet: the cards
    /// may be out of date.
    pub approvals_stale: bool,
    /// The composer's text, as the driver left it (a send that failed puts the
    /// owner's words back here).
    pub draft: String,
}

impl TaskView {
    pub(super) fn of(shared: &Shared, alive: bool) -> TaskView {
        let (task, facts) = (&shared.task, &shared.facts);
        let mut status = task.status();
        let blocked = |status: &mut StatusRow, phase: Phase| {
            status.send = SendMode::Disabled(phase);
        };
        let stuck = status.phase == Phase::WaitingApproval && !facts.undecidable.is_empty();
        if stuck {
            status.can_stop = true; // the cards cannot be answered here: Stop cancels
        }
        if !alive {
            status.phase = Phase::Offline;
            status.can_stop = false;
            blocked(&mut status, Phase::Offline);
        } else if facts.loading || !facts.stream_up {
            // Answers and entries come on the stream: nothing is sent without
            // one (Stop is the model's, and works on what a REST read found).
            blocked(&mut status, Phase::Offline);
        } else if (status.send == SendMode::Send && facts.prompting)
            || (status.send == SendMode::Steer && facts.steering)
        {
            blocked(&mut status, Phase::Working);
        }
        // The link is the stream's: a status read over REST sets the model's
        // own link back to live, and says nothing of whether answers can come.
        let link = if !alive {
            LinkState::Stopped
        } else if facts.stream_up {
            LinkState::Live
        } else if facts.connected || task.run.link == Link::Lost {
            LinkState::Lost
        } else {
            LinkState::Connecting
        };
        let vp = shared.viewport;
        let window = visible(&task.layout, vp.scroll_top, vp.height, vp.overscan);
        let rows = (task.transcript.items.get(window.start..window.end))
            .map_or_else(Vec::new, <[Item]>::to_vec);
        TaskView {
            status,
            link,
            parked: facts.parked,
            loading: facts.loading,
            history_complete: task.history_complete,
            rows,
            window,
            total_rows: task.transcript.items.len(),
            approvals: task.run.approvals.clone(),
            deciding: facts.deciding.clone(),
            undecidable: facts.undecidable.clone(),
            last_send_uncertain: facts.unsent,
            ignored: facts.ignored,
            approvals_stale: task.run.approvals_stale,
            draft: task.composer.text.clone(),
        }
    }
}
