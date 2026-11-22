//! Reading the queue of approvals of this task: once after the stream opens,
//! after every reconnect, after an answer that may not have landed and after
//! a suspend.
//!
//! The model keeps `approvals_stale` after a reconnect until the queue is
//! applied (`approvals_refreshed`). A read that fails, or is refused for being
//! too long, is asked again after `approvals_backoff` (2 s doubling to 60 s).
//! The adapter stops after `MAX_APPROVALS_RETRIES` tries because the stream
//! announces new requests anyway; this driver goes on at the longest wait
//! instead, since a card that is not on the screen is a turn that waits for
//! nothing. A read counts only if it started after the last reason to read
//! (`token`): a list cut before a reconnect could bring back a request that was
//! decided meanwhile.

use tole_client::{ApprovalList, Error};

use super::{millis, Effect, Engine, Now, Out};
use crate::adapter::task::{approvals_backoff, approvals_refreshed, APPROVALS_RETRY_MAX};
use crate::driver::task::calls::{Call, Lane};

#[derive(Default)]
pub(super) struct Refresh {
    want: bool,
    /// Failed or refused reads since the last good one.
    attempt: u32,
    not_before: u64,
    /// Bumped by every reason to read: only the read of this token counts.
    token: u64,
}

impl Refresh {
    /// The session is closed: no reason is left.
    pub(super) fn cancel(&mut self) {
        self.want = false;
    }

    /// A reason to read the queue (again), now.
    pub(super) fn want(&mut self, now: Now) {
        self.want = true;
        self.attempt = 0;
        self.not_before = now.mono;
        self.token = self.token.wrapping_add(1);
    }
}

impl Engine {
    pub(super) fn refresh_next(&mut self, now: Now, out: &mut Out) {
        let r = &self.refresh;
        if !(r.want && now.mono >= r.not_before) || self.hist.reading() {
            return;
        }
        if self.take_lane(Lane::Read) {
            let token = self.refresh.token;
            out.effects
                .push(Effect::Call(Lane::Read, Call::Approvals { token }));
        }
    }

    pub(super) fn refresh_reply(
        &mut self,
        token: u64,
        result: Result<ApprovalList, Error>,
        now: Now,
    ) -> bool {
        self.free_lane(Lane::Read);
        if token != self.refresh.token {
            return false; // stale: the read for the newer reason follows
        }
        let event = result
            .ok()
            .and_then(|list| approvals_refreshed(&list.approvals, &self.session));
        match event {
            Some(event) => {
                self.refresh.want = false;
                self.refresh.attempt = 0;
                self.apply(vec![event])
            }
            None => {
                let wait = approvals_backoff(self.refresh.attempt).unwrap_or(APPROVALS_RETRY_MAX);
                self.refresh.attempt = self.refresh.attempt.saturating_add(1);
                self.refresh.not_before = now.mono.saturating_add(millis(wait));
                false
            }
        }
    }
}
