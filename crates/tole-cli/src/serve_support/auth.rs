//! Bearer-token authentication shared by both `tole serve` transports
//! (REST and MCP over Streamable HTTP): constant-time comparison, header
//! matching and a per-IP failed-auth limiter, so the two faces cannot drift
//! apart again (the MCP face used to compare with `==`, never rate-limited
//! failures and had no body ceiling).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// Failed-auth attempts allowed per source IP per window before the IP is
/// refused until the window rolls over (simple fixed-window limiter).
pub const AUTH_WINDOW_SECS: u64 = 60;
pub const MAX_AUTH_FAILURES: u32 = 10;

/// Constant-time equality for equal-length slices; length differences are
/// not secret here (tokens have a fixed shape).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Does an `Authorization` header value carry the bearer `token`? The scheme
/// is case-insensitive (RFC 7235) and surrounding whitespace is ignored.
pub fn bearer_matches(header: Option<&str>, token: &str) -> bool {
    header
        .and_then(|a| a.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| ct_eq(t.trim().as_bytes(), token.as_bytes()))
        .unwrap_or(false)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `(window_start_epoch, failure_count)` per source IP.
#[derive(Default)]
pub struct AuthLimiter {
    failures: Mutex<HashMap<IpAddr, (u64, u32)>>,
}

impl AuthLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Has this IP used up its failures in the current window? The lock is
    /// held only for the lookup, never across I/O.
    pub fn is_limited(&self, ip: IpAddr) -> bool {
        self.is_limited_at(ip, now_secs())
    }

    pub fn record_failure(&self, ip: IpAddr) {
        self.record_failure_at(ip, now_secs());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, (u64, u32)>> {
        // A poisoned lock must not disable the limiter.
        self.failures.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn is_limited_at(&self, ip: IpAddr, now: u64) -> bool {
        matches!(
            self.lock().get(&ip).map(|(window, count)| {
                now.saturating_sub(*window) <= AUTH_WINDOW_SECS && *count >= MAX_AUTH_FAILURES
            }),
            Some(true)
        )
    }

    fn record_failure_at(&self, ip: IpAddr, now: u64) {
        let mut fails = self.lock();
        // Periodic sweep: drop entries whose window has fully expired, which
        // bounds memory growth from unique source IPs.
        fails.retain(|_, (window, _)| now.saturating_sub(*window) <= AUTH_WINDOW_SECS * 4);
        let entry = fails.entry(ip).or_insert((now, 0));
        if now.saturating_sub(entry.0) > AUTH_WINDOW_SECS {
            entry.0 = now;
            entry.1 = 0;
        }
        entry.1 += 1;
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn ct_eq_matches_only_equal_slices() {
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreT"));
        assert!(!ct_eq(b"secret", b"secre"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn bearer_matching_is_case_insensitive_and_exact() {
        assert!(bearer_matches(Some("Bearer t0ken"), "t0ken"));
        assert!(bearer_matches(Some("bearer  t0ken "), "t0ken"));
        assert!(bearer_matches(Some("BEARER t0ken"), "t0ken"));
        assert!(!bearer_matches(Some("Bearer t0kex"), "t0ken"));
        assert!(!bearer_matches(Some("Bearer t0ken2"), "t0ken"));
        assert!(!bearer_matches(Some("Basic t0ken"), "t0ken"));
        assert!(!bearer_matches(Some("t0ken"), "t0ken"));
        assert!(!bearer_matches(None, "t0ken"));
    }

    #[test]
    fn the_limiter_refuses_after_the_cap_and_recovers_after_the_window() {
        let l = AuthLimiter::new();
        let t0 = 1_000;
        for _ in 0..MAX_AUTH_FAILURES - 1 {
            l.record_failure_at(ip(1), t0);
        }
        assert!(!l.is_limited_at(ip(1), t0), "one below the cap");
        l.record_failure_at(ip(1), t0);
        assert!(l.is_limited_at(ip(1), t0));
        assert!(
            l.is_limited_at(ip(1), t0 + AUTH_WINDOW_SECS),
            "still inside"
        );
        assert!(
            !l.is_limited_at(ip(1), t0 + AUTH_WINDOW_SECS + 1),
            "window rolled over"
        );
        // Other IPs are unaffected.
        assert!(!l.is_limited_at(ip(2), t0));
        // A new failure after the window starts a fresh count.
        l.record_failure_at(ip(1), t0 + AUTH_WINDOW_SECS + 5);
        assert!(!l.is_limited_at(ip(1), t0 + AUTH_WINDOW_SECS + 5));
    }

    #[test]
    fn stale_entries_are_swept_so_memory_stays_bounded() {
        let l = AuthLimiter::new();
        for n in 0..50u8 {
            l.record_failure_at(ip(n), 0);
        }
        assert_eq!(l.tracked(), 50);
        l.record_failure_at(ip(200), AUTH_WINDOW_SECS * 4 + 1);
        assert_eq!(l.tracked(), 1, "expired windows are dropped");
    }
}
