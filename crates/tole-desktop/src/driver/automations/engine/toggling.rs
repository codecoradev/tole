//! The switch: what is sent, what is shown meanwhile and what an answer does.

use tole_client::{CronJobState, Error};
use tole_desktop_model::{AutomationId, Load, Notice, Switch, ToggleRequest};

use super::{CallId, Effect, Engine, Out};
use crate::adapter::automations::toggle_result;

impl Engine {
    /// The owner used the switch of `raw` (the exact id) to make it `enabled`.
    ///
    /// Nothing is sent, and the view changes only by a notice, when: the page
    /// is not a list that was read; the job is not on the page; the model
    /// refuses (a job awaiting the owner's review, a plan-mode server). Nothing
    /// is sent and nothing changes when a switch call is already out (the
    /// press is not queued: the page shows the busy switch) or the row is
    /// already in the wanted state (the same press twice is one call).
    pub(super) fn set_enabled(&mut self, raw: &str, enabled: bool, out: &mut Out) -> bool {
        if self.toggle.is_some() {
            return false;
        }
        let id = AutomationId::new(raw);
        let mut shared = self.store.lock();
        if shared.page.load != Load::Ready {
            // A switch on rows the page cannot vouch for could say the opposite
            // of what the owner means; and the serve may not be there at all.
            shared.page.notice = Some(Notice::NotReached);
            return true;
        }
        let Some(is_on) = shared.page.row(&id).map(|r| r.switch == Switch::On) else {
            shared.page.notice = Some(Notice::Gone);
            return true;
        };
        if is_on == enabled {
            return false;
        }
        let Some(call) = self.calls.next() else {
            shared.page.notice = Some(Notice::NotReached);
            return true;
        };
        if let Some(request) = shared.page.request_toggle(&id) {
            shared.toggling = Some(id);
            self.toggle = Some(call);
            out.effects.push(Effect::Toggle(call, request));
        }
        // Without a request the model set the notice that says why.
        true
    }

    /// The answer of switch call `call`. Applied to the page only when it is
    /// the call that is out (the model changes the row to the state the serve
    /// names, or sets the notice for a refusal). Whatever it was, the list is
    /// read again at once and a list call that is out is dropped: see the
    /// module docs of the engine.
    pub(super) fn toggled(
        &mut self,
        call: CallId,
        request: ToggleRequest,
        result: Result<CronJobState, Error>,
    ) -> bool {
        let mine = self.toggle == Some(call);
        if mine {
            self.toggle = None;
            let mut shared = self.store.lock();
            shared.toggling = None;
            shared
                .page
                .finish_toggle(toggle_result(&request.id, &result));
        }
        self.jobs.redo();
        mine
    }
}
