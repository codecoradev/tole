//! How long a request waits (#1032): up to 900 s, then it expires to denied.
//!
//! The model has no clock; every call takes `now_ms` from the adapter. The
//! server is the authority on expiry: the model only counts down, so the card
//! shows the time and stops offering "Allow once" when it is up, and a later
//! answer from the server replaces what the local clock decided.

/// The most a request waits before it counts as "Don't allow" (the core's
/// `EXPIRY_SECS`). A longer time in a row is cut to it: never more generous
/// than the rule.
pub const EXPIRY_SECS: u64 = 900;
/// How far the two clocks may disagree before the server's timestamp is not
/// believed.
const SKEW_MS: u64 = 60_000;

/// The moment a request stops waiting, on the clock of the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    deadline_ms: u64,
}

impl Wait {
    /// The wait of a request first seen at `now_ms`. It runs from the server's
    /// `created_at_ms` when that agrees with the local clock (within the
    /// skew); otherwise, and without it (a delegated child's row, whose
    /// `expires_in_secs` is the time left), from `now_ms`. Never longer than
    /// the wait from `now_ms`.
    pub fn new(created_at_ms: Option<u64>, expires_in_secs: Option<u64>, now_ms: u64) -> Wait {
        let len = expires_in_secs.unwrap_or(EXPIRY_SECS).min(EXPIRY_SECS) * 1000;
        let from_now = now_ms.saturating_add(len);
        let agrees = |c: &u64| *c <= now_ms + SKEW_MS && c.saturating_add(len + SKEW_MS) >= now_ms;
        let deadline = created_at_ms.filter(agrees).map(|c| c.saturating_add(len));
        Wait {
            deadline_ms: deadline.map_or(from_now, |d| d.min(from_now)),
        }
    }

    pub fn deadline_ms(self) -> u64 {
        self.deadline_ms
    }

    /// Whole seconds left, rounded up; 0 once the time is up.
    pub fn remaining_secs(self, now_ms: u64) -> u64 {
        self.deadline_ms.saturating_sub(now_ms).div_ceil(1000)
    }

    pub fn expired(self, now_ms: u64) -> bool {
        now_ms >= self.deadline_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000_000;

    #[test]
    fn counts_down_from_the_server_creation_time() {
        let w = Wait::new(Some(NOW - 60_000), Some(900), NOW);
        assert_eq!(w.remaining_secs(NOW), 840);
        assert_eq!(w.remaining_secs(NOW + 839_001), 1);
        assert!(!w.expired(NOW + 839_999) && w.expired(NOW + 840_000));
        assert_eq!(w.remaining_secs(NOW + 900_000), 0);
    }

    #[test]
    fn the_wait_is_never_longer_than_the_rule_and_clocks_that_disagree_count_from_now() {
        let left = |created, secs| Wait::new(created, secs, NOW).remaining_secs(NOW);
        assert_eq!(
            (left(None, Some(86_400)), left(Some(NOW), Some(u64::MAX))),
            (900, 900)
        );
        assert_eq!(
            (left(None, Some(120)), left(None, None)),
            (120, EXPIRY_SECS)
        );
        for created in [NOW + 3_600_000, NOW - 3_600_000, NOW + 30_000] {
            assert_eq!(left(Some(created), Some(900)), 900);
        }
    }
}
