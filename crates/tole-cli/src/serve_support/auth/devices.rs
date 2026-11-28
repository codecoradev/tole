//! The device-token half of [`Authenticator`] (#1320): the REST face's one check with a scope, and
//! the re-check an open event stream makes. A child module of `auth`, so it reads the private
//! fields and helpers there.

use super::{bearer_token, now_secs, Authenticator};
use crate::serve_support::device_tokens::{Live, Scope};

impl Authenticator {
    /// Also accept the device tokens of `live` (#1320) in [`Self::authenticate`].
    pub fn with_device_tokens(mut self, live: Live) -> Self {
        self.devices = Some(live);
        self
    }

    /// The scope of the credential in this `Authorization` header, `None` when it is not valid.
    /// The shared token and Basic are [`Scope::Full`]. A device token costs one `stat` of the
    /// token file and may write a last-use line (at most one per token per 15 minutes), both
    /// blocking: the REST face runs one thread per connection, so that is fine here.
    /// A wrong, unknown and revoked token are all `None`, with the same work.
    pub fn authenticate(&self, header: Option<&str>) -> Option<Scope> {
        if self.check(header) {
            return Some(Scope::Full);
        }
        let live = self.devices.as_ref()?;
        let Some(who) = live.lookup(bearer_token(header)?) else {
            // A file that cannot be used refuses every device token: say so, once per change.
            let now = live.error();
            let mut last = self.store_error.lock().unwrap_or_else(|p| p.into_inner());
            if *last != now {
                if let Some(why) = &now {
                    eprintln!("tole serve: device tokens are refused until this is fixed: {why}");
                }
                *last = now;
            }
            return None;
        };
        live.touch(&who.name, now_secs());
        Some(who.scope)
    }

    /// Is this still an active device token? An event stream opened with one asks again on every
    /// pass, so a revoked token ends it.
    pub fn device_token_valid(&self, header: Option<&str>) -> bool {
        let live = self.devices.as_ref();
        live.zip(bearer_token(header))
            .is_some_and(|(l, t)| l.lookup(t).is_some())
    }
}
