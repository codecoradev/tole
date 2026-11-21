//! The Automations page: the table, the scheduler line, the history of the
//! selected automation and the switch.

use super::job::{AutomationId, AutomationRow, RawJob, Switch};
use super::run::{RawRun, RunEntry};
use super::toggle::{Notice, ToggleIntent, ToggleRequest, ToggleResult};
use crate::plain::{printable, Load};

/// Where the scheduler runs, from `scheduler.host` of `GET /cron/jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerHost {
    /// Inside `tole serve --cron`: closing the window does not stop it.
    Serve,
    /// A separate `tole cron tick`.
    Tick,
    Other(String),
}

/// Whether anything runs the automations. Not alive, or not told, is
/// `NotRunning`: the page never implies a schedule is kept when it may not be.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scheduler {
    #[default]
    NotRunning,
    Running(SchedulerHost),
}

impl Scheduler {
    /// From `scheduler.alive` and `scheduler.host`.
    pub fn new(alive: bool, host: Option<&str>) -> Scheduler {
        match (alive, host) {
            (false, _) => Scheduler::NotRunning,
            (true, Some("serve")) => Scheduler::Running(SchedulerHost::Serve),
            (true, Some("tick")) => Scheduler::Running(SchedulerHost::Tick),
            (true, other) => {
                Scheduler::Running(SchedulerHost::Other(printable(other.unwrap_or(""), 40)))
            }
        }
    }
}

/// The page state. Reducers are methods; the adapter sends the requests they
/// return and feeds the answers back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutomationsState {
    pub load: Load,
    pub rows: Vec<AutomationRow>,
    pub scheduler: Scheduler,
    pub selected: Option<AutomationId>,
    /// The history of `selected`, newest first as the server sends it.
    pub history: Vec<RunEntry>,
    pub notice: Option<Notice>,
    /// A plan-mode server answered 409 to a resume: no switch can turn
    /// anything on until the server changes.
    pub resume_blocked: bool,
    pending: Option<ToggleRequest>,
}

impl AutomationsState {
    /// A fresh list from the server replaces the rows. A selection that is
    /// gone is dropped with its history.
    pub fn loaded(&mut self, jobs: Vec<RawJob>, scheduler: Scheduler) {
        self.rows = jobs.into_iter().map(AutomationRow::new).collect();
        self.scheduler = scheduler;
        self.load = Load::Ready;
        if self
            .selected
            .as_ref()
            .is_some_and(|s| self.row(s).is_none())
        {
            self.selected = None;
            self.history.clear();
        }
    }

    /// The list could not be read. The old rows stay, marked as failed; the
    /// scheduler is no longer assumed to be running.
    pub fn load_failed(&mut self, message: &str) {
        self.load = Load::Failed(printable(message, 200));
        self.scheduler = Scheduler::NotRunning;
    }

    pub fn row(&self, id: &AutomationId) -> Option<&AutomationRow> {
        self.rows.iter().find(|r| r.id == *id)
    }

    /// Selects a row; the adapter then asks for its history.
    pub fn select(&mut self, id: &AutomationId) -> bool {
        if self.row(id).is_none() {
            return false;
        }
        self.selected = Some(id.clone());
        self.history.clear();
        true
    }

    /// History for `id`; an answer for a row that is no longer selected is
    /// dropped.
    pub fn history_loaded(&mut self, id: &AutomationId, runs: Vec<RawRun>) {
        if self.selected.as_ref() == Some(id) {
            self.history = runs.into_iter().map(RunEntry::new).collect();
        }
    }

    /// The owner used the switch of `id`. Returns the request to send, or
    /// `None` with a [`Notice`] set when it must not be sent (a job awaiting
    /// the owner's review, a plan-mode server that refuses resumes, a request
    /// already in flight).
    pub fn request_toggle(&mut self, id: &AutomationId) -> Option<ToggleRequest> {
        if self.pending.is_some() {
            return None;
        }
        let row = self.row(id)?;
        let intent = match row.switch {
            Switch::On => ToggleIntent::Pause,
            Switch::Off => ToggleIntent::Resume,
        };
        if intent == ToggleIntent::Resume {
            if !row.can_resume_here() {
                self.notice = Some(Notice::NeedsOwnerReview {
                    name: row.name.clone(),
                });
                return None;
            }
            if self.resume_blocked {
                self.notice = Some(Notice::ServerPlanMode);
                return None;
            }
        }
        self.notice = None;
        let request = ToggleRequest {
            id: id.clone(),
            intent,
        };
        self.pending = Some(request.clone());
        Some(request)
    }

    /// The answer to the request returned by [`Self::request_toggle`]. The
    /// switch changes only on success, and to the state the server reports; a
    /// word it does not know leaves it off.
    pub fn finish_toggle(&mut self, result: ToggleResult) {
        let Some(req) = self.pending.take() else {
            return;
        };
        let Some(row) = self.rows.iter_mut().find(|r| r.id == req.id) else {
            return;
        };
        match result {
            ToggleResult::Done { state } => {
                row.switch = if state == "active" {
                    Switch::On
                } else {
                    Switch::Off
                };
                if row.switch == Switch::Off {
                    row.next = super::job::NextRun::Paused;
                }
                self.notice = None;
            }
            ToggleResult::Refused { status, message } => {
                let notice = Notice::from_refusal(
                    req.intent,
                    status,
                    &message,
                    &row.name,
                    row.awaiting_review,
                );
                match notice {
                    Notice::NeedsOwnerReview { .. } => {
                        row.awaiting_review = true;
                        row.switch = Switch::Off;
                    }
                    Notice::ServerPlanMode => self.resume_blocked = true,
                    _ => {}
                }
                self.notice = Some(notice);
            }
            ToggleResult::Unreachable => self.notice = Some(Notice::NotReached),
        }
    }

    pub fn dismiss_notice(&mut self) {
        self.notice = None;
    }

    /// (on, paused) for the page headline ("2 on, 1 paused").
    pub fn counts(&self) -> (usize, usize) {
        let on = self.rows.iter().filter(|r| r.switch == Switch::On).count();
        (on, self.rows.len() - on)
    }
}

#[cfg(test)]
mod tests {
    use super::super::job::raw_job;
    use super::*;

    fn page() -> AutomationsState {
        let mut s = AutomationsState::default();
        s.loaded(
            vec![
                raw_job("own", "cli", "active", false),
                raw_job("off", "cli", "paused", false),
                raw_job("tole", "agent", "paused", true),
            ],
            Scheduler::new(true, Some("serve")),
        );
        s
    }

    fn id(s: &str) -> AutomationId {
        AutomationId::new(s)
    }

    #[test]
    fn the_switch_flips_only_after_the_server_agrees() {
        let mut s = page();
        assert_eq!(s.counts(), (1, 2));
        let req = s.request_toggle(&id("off")).unwrap();
        assert_eq!(req.intent, ToggleIntent::Resume);
        assert_eq!(s.row(&id("off")).unwrap().switch, Switch::Off);
        assert_eq!(s.request_toggle(&id("own")), None, "one request at a time");
        s.finish_toggle(ToggleResult::Done {
            state: "active".into(),
        });
        assert_eq!(s.row(&id("off")).unwrap().switch, Switch::On);
        let req = s.request_toggle(&id("own")).unwrap();
        assert_eq!(req.intent, ToggleIntent::Pause);
        s.finish_toggle(ToggleResult::Unreachable);
        assert_eq!(s.row(&id("own")).unwrap().switch, Switch::On);
        assert_eq!(s.notice, Some(Notice::NotReached));
    }

    #[test]
    fn a_state_word_the_server_never_sends_does_not_turn_a_job_on() {
        let mut s = page();
        s.request_toggle(&id("off")).unwrap();
        s.finish_toggle(ToggleResult::Done {
            state: "running".into(),
        });
        assert_eq!(s.row(&id("off")).unwrap().switch, Switch::Off);
    }

    #[test]
    fn a_job_awaiting_review_is_not_sent_to_serve() {
        let mut s = page();
        assert_eq!(s.request_toggle(&id("tole")), None);
        assert_eq!(
            s.notice,
            Some(Notice::NeedsOwnerReview {
                name: "job tole".into()
            })
        );
        assert_eq!(s.request_toggle(&id("missing")), None);
    }

    /// Serve says 403: the row is marked as awaiting review so the next click
    /// is not sent, even though the page believed it could resume.
    #[test]
    fn a_403_on_resume_marks_the_job_as_awaiting_review() {
        let mut s = page();
        s.request_toggle(&id("off")).unwrap();
        s.finish_toggle(ToggleResult::Refused {
            status: 403,
            message: "not reviewed: x".into(),
        });
        assert!(s.row(&id("off")).unwrap().awaiting_review);
        assert!(matches!(s.notice, Some(Notice::NeedsOwnerReview { .. })));
        assert_eq!(s.request_toggle(&id("off")), None);
    }

    #[test]
    fn a_plan_mode_server_blocks_every_resume_but_not_a_pause() {
        let mut s = page();
        s.request_toggle(&id("off")).unwrap();
        s.finish_toggle(ToggleResult::Refused {
            status: 409,
            message: "plan mode".into(),
        });
        assert!(s.resume_blocked && s.notice == Some(Notice::ServerPlanMode));
        s.dismiss_notice();
        assert_eq!(s.request_toggle(&id("off")), None);
        assert_eq!(s.notice, Some(Notice::ServerPlanMode));
        assert_eq!(
            s.request_toggle(&id("own")).unwrap().intent,
            ToggleIntent::Pause
        );
    }

    #[test]
    fn history_follows_the_selection_and_stale_answers_are_dropped() {
        let run = |status: &str| RawRun {
            fire_id: "f".into(),
            scheduled_for: 1,
            status: Some(status.into()),
            session_id: None,
            approval_id: None,
            output: String::new(),
            output_truncated: false,
        };
        let mut s = page();
        assert!(s.select(&id("own")) && !s.select(&id("nope")));
        s.history_loaded(&id("off"), vec![run("ok")]);
        assert!(s.history.is_empty());
        s.history_loaded(&id("own"), vec![run("ok")]);
        assert_eq!(s.history.len(), 1);
        s.loaded(
            vec![raw_job("off", "cli", "paused", false)],
            Scheduler::default(),
        );
        assert!(s.selected.is_none() && s.history.is_empty());
    }

    #[test]
    fn the_scheduler_is_running_only_when_it_says_so() {
        assert_eq!(Scheduler::new(false, Some("serve")), Scheduler::NotRunning);
        assert_eq!(
            Scheduler::new(true, Some("tick")),
            Scheduler::Running(SchedulerHost::Tick)
        );
        assert!(matches!(
            Scheduler::new(true, None),
            Scheduler::Running(SchedulerHost::Other(_))
        ));
        let mut s = page();
        s.load_failed("no route");
        assert_eq!(s.scheduler, Scheduler::NotRunning);
        assert_eq!(s.load, Load::Failed("no route".into()));
    }
}
