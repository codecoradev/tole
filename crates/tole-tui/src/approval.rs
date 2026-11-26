//! Approval requests: what a UI shows and decides on, and where the verdict
//! goes. [`ApprovalRequest`] is the question, [`Pending`] a request waiting
//! for its answer (dropping it unanswered denies), [`Prompter`] the handle an
//! approval source uses to put requests in front of the UI.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use tole_core::action_summary::ActionSummary;
use tole_core::approval::Verdict;
use tole_core::tool::Risk;

use crate::one_line;

/// What a UI shows and decides on for one approval, whoever asked (#526): an
/// in-process turn ([`Prompter::ask`]) or a remote source such as a server's
/// `/approvals` queue ([`Prompter::post`]). It carries the risk tier, so the
/// rule that a `Destructive` call is never "allowed always" lives in one place
/// instead of in each UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    text: String,
    risk: Risk,
    summary: Option<ActionSummary>,
}

impl ApprovalRequest {
    /// `text` is reduced to one safe line (control characters removed).
    pub fn new(text: impl AsRef<str>, risk: Risk) -> Self {
        Self {
            text: one_line(text.as_ref()),
            risk,
            summary: None,
        }
    }

    /// A request worded by the shared summary (#1041): the plain-language line
    /// first (what it does, its effect), then `detail` exactly as the prompt
    /// showed it before (`<tool>: <the tool's whole text> → <target>`), never
    /// cut. The structured fields are those every other face shows.
    pub fn from_summary(summary: ActionSummary, detail: &str, risk: Risk) -> Self {
        Self {
            text: one_line(&format!("{} :: {detail}", summary.sentence())),
            risk,
            summary: Some(summary),
        }
    }

    /// The structured summary, when the source had one.
    pub fn summary(&self) -> Option<&ActionSummary> {
        self.summary.as_ref()
    }

    /// What is being asked, as one line.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn risk(&self) -> Risk {
        self.risk
    }

    pub fn is_destructive(&self) -> bool {
        self.risk == Risk::Destructive
    }

    /// Whether a UI may offer to allow this kind of call from now on. Never for
    /// `Destructive`: it is asked every time (the approver never allowlists it).
    pub fn can_allow_always(&self) -> bool {
        !self.is_destructive()
    }
}

/// An approval waiting for an answer: the request plus where the verdict goes.
/// The verdict is delivered exactly once, and **dropping the `Pending`
/// unanswered denies** (fail closed), so a cancelled turn, a vanished UI or a
/// dropped connection can never leave a call allowed.
pub struct Pending {
    pub(crate) request: ApprovalRequest,
    reply: Option<Box<dyn FnOnce(Verdict) + Send>>,
}

impl Pending {
    pub fn new(request: ApprovalRequest, reply: impl FnOnce(Verdict) + Send + 'static) -> Self {
        Self {
            request,
            reply: Some(Box::new(reply)),
        }
    }

    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Answer and consume.
    pub(crate) fn resolve(mut self, verdict: Verdict) {
        if let Some(reply) = self.reply.take() {
            reply(verdict);
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            reply(Verdict::Deny);
        }
    }
}

/// Handed to an approval source; puts requests in front of the UI.
#[derive(Clone)]
pub struct Prompter(Arc<Mutex<Sender<Pending>>>);

impl Prompter {
    /// Hand a request to the UI without waiting; `reply` gets the verdict once.
    /// Returns `false` when the UI is gone, in which case `reply` has already
    /// been called with `Deny`.
    pub fn post(
        &self,
        request: ApprovalRequest,
        reply: impl FnOnce(Verdict) + Send + 'static,
    ) -> bool {
        let pending = Pending::new(request, reply);
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(pending)
            .is_ok()
    }

    /// Ask and block until the UI answers (the in-process turn thread).
    pub fn ask(&self, request: ApprovalRequest) -> Verdict {
        let (tx, rx) = mpsc::channel();
        self.post(request, move |v| {
            let _ = tx.send(v);
        });
        rx.recv().unwrap_or(Verdict::Deny)
    }
}

pub fn approval_channel() -> (Prompter, Receiver<Pending>) {
    let (tx, rx) = mpsc::channel();
    (Prompter(Arc::new(Mutex::new(tx))), rx)
}
