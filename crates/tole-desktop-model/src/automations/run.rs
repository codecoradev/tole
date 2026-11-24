//! How a run of an automation ended, and one line of its history.

use crate::plain::printable;
use crate::tokens::Tone;

/// Longest run output kept for display.
const OUTPUT_CHARS: usize = 600;

/// How a run ended. The statuses of the cron ledger are `ok`, `error`,
/// `cancelled`, `budget`, `awaiting_approval` and `interrupted`; a run with no
/// result yet (still going, or never finished) and a word nobody taught this
/// model are their own cases, never read as a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Passed,
    Failed,
    /// The run stopped to ask the owner (an approval is waiting or was parked).
    StoppedToAsk,
    /// The run hit its spending limit.
    StoppedByLimit,
    Cancelled,
    /// The server stopped while it ran.
    Interrupted,
    NoResultYet,
    Unknown(String),
}

impl RunOutcome {
    /// From the `status` of a run (`None`: no result recorded yet).
    pub fn from_status(status: Option<&str>) -> RunOutcome {
        match status {
            None => RunOutcome::NoResultYet,
            Some("ok") => RunOutcome::Passed,
            Some("error") => RunOutcome::Failed,
            Some("awaiting_approval") => RunOutcome::StoppedToAsk,
            Some("budget") => RunOutcome::StoppedByLimit,
            Some("cancelled") => RunOutcome::Cancelled,
            Some("interrupted") => RunOutcome::Interrupted,
            Some(other) => RunOutcome::Unknown(printable(other, 40)),
        }
    }

    /// Only a clean pass is good news; every other outcome, the unknown ones
    /// included, is something to look at.
    pub fn is_pass(&self) -> bool {
        matches!(self, RunOutcome::Passed)
    }

    /// The colour of the result: green only for a pass, red for a failure,
    /// amber for what the owner should look at (a run that stopped to ask, hit
    /// its limit, was interrupted, or ended in a way nobody taught the model),
    /// neutral for a run that was cancelled or has no result yet.
    pub fn tone(&self) -> Tone {
        match self {
            RunOutcome::Passed => Tone::Good,
            RunOutcome::Failed => Tone::Bad,
            RunOutcome::StoppedToAsk
            | RunOutcome::StoppedByLimit
            | RunOutcome::Interrupted
            | RunOutcome::Unknown(_) => Tone::Attention,
            RunOutcome::Cancelled | RunOutcome::NoResultYet => Tone::Neutral,
        }
    }
}

/// One run of `GET /cron/runs`, as the adapter hands it over (mapping of the
/// client's `CronRun`: `fire_id`, `scheduled_for`, `status`, `session_id`,
/// `output`, `output_truncated`). `approval_id` is the adapter's lookup of the
/// pending approval whose session is `session_id`; the serve run row does not
/// carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRun {
    pub fire_id: String,
    pub scheduled_for: i64,
    pub status: Option<String>,
    pub session_id: Option<String>,
    pub approval_id: Option<String>,
    pub output: String,
    pub output_truncated: bool,
}

/// One line of an automation's history ("what happened").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEntry {
    pub fire_id: String,
    /// Unix seconds the run was due; the window formats it in the owner's zone.
    pub scheduled_for: i64,
    pub outcome: RunOutcome,
    pub session_id: Option<String>,
    pub output: String,
    pub output_truncated: bool,
    approval_id: Option<String>,
}

impl RunEntry {
    pub fn new(raw: RawRun) -> RunEntry {
        RunEntry {
            outcome: RunOutcome::from_status(raw.status.as_deref()),
            fire_id: printable(&raw.fire_id, 80),
            scheduled_for: raw.scheduled_for,
            session_id: raw.session_id.map(|s| printable(&s, 80)),
            output: printable(&raw.output, OUTPUT_CHARS),
            output_truncated: raw.output_truncated,
            approval_id: raw.approval_id.map(|s| printable(&s, 80)),
        }
    }

    /// The approval to open ("Open in Approvals"): only a run that stopped to
    /// ask has one, and only when the adapter found it still pending.
    pub fn approval_link(&self) -> Option<&str> {
        match self.outcome {
            RunOutcome::StoppedToAsk => self.approval_id.as_deref(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: Option<&str>, approval: Option<&str>) -> RawRun {
        RawRun {
            fire_id: "f1".into(),
            scheduled_for: 100,
            status: status.map(str::to_owned),
            session_id: Some("s1".into()),
            approval_id: approval.map(str::to_owned),
            output: "Moved 12 files\n".into(),
            output_truncated: false,
        }
    }

    #[test]
    fn each_status_has_its_outcome_and_only_ok_is_a_pass() {
        for (status, want) in [
            ("ok", RunOutcome::Passed),
            ("error", RunOutcome::Failed),
            ("awaiting_approval", RunOutcome::StoppedToAsk),
            ("budget", RunOutcome::StoppedByLimit),
            ("cancelled", RunOutcome::Cancelled),
            ("interrupted", RunOutcome::Interrupted),
        ] {
            assert_eq!(RunOutcome::from_status(Some(status)), want);
        }
        assert_eq!(RunOutcome::from_status(None), RunOutcome::NoResultYet);
        for odd in ["", "OK", "okay", "passed", "ok\n"] {
            let o = RunOutcome::from_status(Some(odd));
            assert!(
                matches!(o, RunOutcome::Unknown(_)) && !o.is_pass(),
                "{odd:?}"
            );
        }
        assert!(!RunOutcome::NoResultYet.is_pass());
    }

    #[test]
    fn only_a_run_that_stopped_to_ask_links_to_its_approval() {
        let asked = RunEntry::new(raw(Some("awaiting_approval"), Some("apr-1")));
        assert_eq!(asked.approval_link(), Some("apr-1"));
        assert_eq!(asked.output, "Moved 12 files");
        let no_id = RunEntry::new(raw(Some("awaiting_approval"), None));
        assert_eq!(no_id.approval_link(), None);
        let passed = RunEntry::new(raw(Some("ok"), Some("apr-1")));
        assert_eq!(passed.approval_link(), None);
    }

    #[test]
    fn only_a_pass_is_green_and_what_nobody_taught_the_model_is_amber() {
        use crate::tokens::Tone;
        assert_eq!(RunOutcome::Passed.tone(), Tone::Good);
        assert_eq!(RunOutcome::Failed.tone(), Tone::Bad);
        for look in [
            RunOutcome::StoppedToAsk,
            RunOutcome::StoppedByLimit,
            RunOutcome::Interrupted,
            RunOutcome::Unknown("odd".into()),
        ] {
            assert_eq!(look.tone(), Tone::Attention, "{look:?}");
        }
        assert_eq!(RunOutcome::Cancelled.tone(), Tone::Neutral);
        assert_eq!(RunOutcome::NoResultYet.tone(), Tone::Neutral);
    }
}
