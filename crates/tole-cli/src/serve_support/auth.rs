//! Authentication shared by both `tole serve` transports (REST and MCP over
//! Streamable HTTP): constant-time comparison, header matching and a per-IP
//! failed-auth limiter, so the two faces cannot drift apart again (the MCP
//! face used to compare with `==`, never rate-limited failures and had no
//! body ceiling).
//!
//! [`Authenticator`] is the ONE check every route goes through (#742): the
//! static bearer token (#682) and, with `--auth basic`, HTTP Basic against a
//! PBKDF2 password hash. A transport asks `check(authorization_header)` and
//! nothing else; there is no second path that could leave a route open.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use ring::rand::SecureRandom;

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

/// PHC-style scheme name of a stored password hash.
pub const HASH_SCHEME: &str = "pbkdf2-sha256";
/// Iterations of a newly made hash (OWASP, PBKDF2-HMAC-SHA256).
pub const DEFAULT_ITERATIONS: u32 = 600_000;
/// A stored hash below this is refused at startup: a weak hash is a
/// misconfiguration, not a choice.
pub const MIN_ITERATIONS: u32 = 10_000;
/// Above this a hash is refused too: every failed attempt would cost that
/// much CPU, so an absurd value is a denial of service on the owner.
const MAX_ITERATIONS: u32 = 10_000_000;
const SALT_LEN: usize = 16;
const HASH_LEN: usize = 32;
/// Shortest password `serve-hash` accepts.
pub const MIN_PASSWORD_LEN: usize = 8;
/// Longest `user:password` a Basic header may carry; anything longer is not
/// hashed (bounds the work an unauthenticated client can ask for).
const MAX_BASIC_CREDENTIAL: usize = 512;
/// Longest `--basic-user`.
const MAX_USER_LEN: usize = 128;
/// Largest hash file.
const MAX_HASH_FILE: u64 = 4096;
/// How long a verified `Authorization` header is remembered, so a client
/// that sends it on every request does not pay PBKDF2 each time.
const VERIFIED_TTL: Duration = Duration::from_secs(300);
const VERIFIED_MAX: usize = 64;

/// A salted PBKDF2-HMAC-SHA256 password hash, stored as
/// `$pbkdf2-sha256$i=<iterations>$<salt>$<hash>` (unpadded standard base64).
/// Parsing and verification errors never carry the string.
#[derive(Clone)]
pub struct PasswordHash {
    iterations: std::num::NonZeroU32,
    salt: Vec<u8>,
    hash: Vec<u8>,
    encoded: String,
}

impl PasswordHash {
    pub fn parse(s: &str) -> Result<Self, String> {
        const BAD: &str = "malformed password hash (expected \
            $pbkdf2-sha256$i=<iterations>$<salt>$<hash>, as `tole serve-hash` prints it)";
        let s = s.trim();
        let mut it = s.split('$');
        let (empty, scheme, iters, salt, hash, rest) = (
            it.next(),
            it.next(),
            it.next(),
            it.next(),
            it.next(),
            it.next(),
        );
        if empty != Some("") || scheme != Some(HASH_SCHEME) || rest.is_some() {
            return Err(BAD.into());
        }
        let iterations: u32 = iters
            .and_then(|i| i.strip_prefix("i="))
            .and_then(|i| i.parse().ok())
            .ok_or(BAD)?;
        if iterations < MIN_ITERATIONS {
            return Err(format!(
                "password hash has too few iterations (at least {MIN_ITERATIONS}); make a new \
                 one with `tole serve-hash`"
            ));
        }
        if iterations > MAX_ITERATIONS {
            return Err(format!(
                "password hash has too many iterations (at most {MAX_ITERATIONS})"
            ));
        }
        let salt = STANDARD_NO_PAD.decode(salt.ok_or(BAD)?).map_err(|_| BAD)?;
        let hash = STANDARD_NO_PAD.decode(hash.ok_or(BAD)?).map_err(|_| BAD)?;
        if salt.len() < SALT_LEN || hash.len() != HASH_LEN {
            return Err(BAD.into());
        }
        Ok(Self {
            iterations: std::num::NonZeroU32::new(iterations).ok_or(BAD)?,
            salt,
            hash,
            encoded: s.to_string(),
        })
    }

    /// Hash `password` with a fresh random salt.
    pub fn create(password: &[u8], iterations: u32) -> Result<String, String> {
        if password.len() < MIN_PASSWORD_LEN {
            return Err(format!(
                "the password must be at least {MIN_PASSWORD_LEN} characters"
            ));
        }
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
            return Err(format!(
                "iterations must be between {MIN_ITERATIONS} and {MAX_ITERATIONS}"
            ));
        }
        let mut salt = [0u8; SALT_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| "the system random source failed".to_string())?;
        let mut out = [0u8; HASH_LEN];
        let n = std::num::NonZeroU32::new(iterations).ok_or("iterations must not be 0")?;
        ring::pbkdf2::derive(
            ring::pbkdf2::PBKDF2_HMAC_SHA256,
            n,
            &salt,
            password,
            &mut out,
        );
        Ok(format!(
            "${HASH_SCHEME}$i={iterations}${}${}",
            STANDARD_NO_PAD.encode(salt),
            STANDARD_NO_PAD.encode(out)
        ))
    }

    /// Constant-time check (ring compares the derived key in constant time).
    pub fn verify(&self, password: &[u8]) -> bool {
        ring::pbkdf2::verify(
            ring::pbkdf2::PBKDF2_HMAC_SHA256,
            self.iterations,
            &self.salt,
            password,
            &self.hash,
        )
        .is_ok()
    }

    /// The stored form, for the secret set (it is held by role, like the token).
    pub fn encoded(&self) -> &str {
        &self.encoded
    }
}

/// The single serve user: a name and the hash of its password.
#[derive(Clone)]
pub struct BasicCredentials {
    user: String,
    hash: PasswordHash,
}

impl BasicCredentials {
    pub fn new(user: &str, hash: PasswordHash) -> Result<Self, String> {
        let user = user.trim();
        if user.is_empty() {
            return Err("the basic user name is empty".into());
        }
        if user.len() > MAX_USER_LEN || user.contains(':') || user.chars().any(char::is_control) {
            return Err(format!(
                "the basic user name must be at most {MAX_USER_LEN} characters, without ':' \
                 or control characters"
            ));
        }
        Ok(Self {
            user: user.to_string(),
            hash,
        })
    }

    /// Check a decoded `user:password`. The hash is ALWAYS computed, whether
    /// or not the user matches, and both outcomes fold into one bool, so the
    /// answer (and its timing) does not tell a wrong user from a wrong password.
    fn verify(&self, credential: &[u8]) -> bool {
        let (user, password) = match credential.iter().position(|b| *b == b':') {
            Some(i) => (&credential[..i], &credential[i + 1..]),
            None => (credential, &[][..]),
        };
        let user_ok = ct_eq(user, self.user.as_bytes());
        let password_ok = self.hash.verify(password);
        user_ok & password_ok
    }
}

/// The `user:password` of an `Authorization: Basic` value, decoded (the
/// scheme is case-insensitive, RFC 7617). `None` for anything else.
fn basic_credential(header: &str) -> Option<Vec<u8>> {
    let (scheme, rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let rest = rest.trim();
    // Base64 is 4/3 the size of its input.
    if rest.len() > MAX_BASIC_CREDENTIAL * 4 / 3 + 4 {
        return None;
    }
    let raw = STANDARD.decode(rest).ok()?;
    (raw.len() <= MAX_BASIC_CREDENTIAL).then_some(raw)
}

/// Is `bind` a loopback address (`localhost`, `127.0.0.0/8`, `::1`)?
pub fn is_loopback_bind(bind: &str) -> bool {
    bind == "localhost"
        || bind
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The Basic credentials from the command line, the environment and a file:
/// the user from `--basic-user` else `TOLE_SERVE_BASIC_USER`, the hash from
/// `--basic-hash-file` (a regular file, no link or FIFO, at most 4 KiB) else
/// `TOLE_SERVE_BASIC_HASH`. There is no hash flag: a command-line value is
/// readable by every local process. Errors never carry the hash. The hash
/// becomes a secret held by role, whatever carried it.
pub fn resolve_basic(
    user_flag: Option<String>,
    hash_file: Option<&Path>,
    user_env: Option<String>,
    hash_env: Option<String>,
) -> Result<BasicCredentials, String> {
    let user = [user_flag, user_env]
        .into_iter()
        .flatten()
        .find(|u| !u.trim().is_empty())
        .ok_or("--auth basic needs a user: set --basic-user or TOLE_SERVE_BASIC_USER")?;
    let from_file = match hash_file {
        Some(p) => {
            let bytes = tole_core::delegation_report::read_plain_file(p, MAX_HASH_FILE)
                .map_err(|e| format!("--basic-hash-file {}: {e}", p.display()))?
                .ok_or_else(|| format!("--basic-hash-file {}: does not exist", p.display()))?;
            Some(
                String::from_utf8(bytes)
                    .map_err(|_| format!("--basic-hash-file {}: not UTF-8", p.display()))?,
            )
        }
        None => None,
    };
    let encoded = [from_file, hash_env]
        .into_iter()
        .flatten()
        .find(|h| !h.trim().is_empty())
        .ok_or(
            "--auth basic needs a password hash: set --basic-hash-file or TOLE_SERVE_BASIC_HASH \
             (make one with `tole serve-hash`)",
        )?;
    let hash = PasswordHash::parse(&encoded)?;
    tole_core::secrets::add_role_secret("TOLE_SERVE_BASIC_HASH", hash.encoded());
    BasicCredentials::new(&user, hash)
}

/// The one authentication check of `tole serve`, both transports: the static
/// bearer token and/or the Basic user. `check` takes the raw `Authorization`
/// header and returns whether it carries a valid credential of either kind.
pub struct Authenticator {
    token: Option<String>,
    basic: Option<BasicCredentials>,
    /// SHA-256 of an `Authorization` value that verified, with its expiry.
    /// The key is a digest of the whole header, so a lookup says nothing
    /// about a guess; failures are never remembered.
    verified: Mutex<HashMap<[u8; 32], Instant>>,
}

impl Authenticator {
    /// Bearer token only (`--auth token`, the default).
    pub fn token(token: String) -> Self {
        Self::new(Some(token), None)
    }

    pub fn new(token: Option<String>, basic: Option<BasicCredentials>) -> Self {
        Self {
            token,
            basic,
            verified: Mutex::new(HashMap::new()),
        }
    }

    /// Does this `Authorization` header carry a valid credential?
    pub fn check(&self, header: Option<&str>) -> bool {
        if let Some(t) = &self.token {
            if bearer_matches(header, t) {
                return true;
            }
        }
        let (Some(basic), Some(h)) = (&self.basic, header) else {
            return false;
        };
        // Not Basic at all (a wrong bearer, say): no hash work to spend.
        let Some(credential) = basic_credential(h) else {
            return false;
        };
        let digest = ring::digest::digest(&ring::digest::SHA256, h.as_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(digest.as_ref());
        let now = Instant::now();
        if self.lock_verified().get(&key).is_some_and(|u| *u > now) {
            return true;
        }
        let ok = basic.verify(&credential);
        if ok {
            let mut v = self.lock_verified();
            v.retain(|_, until| *until > now);
            if v.len() >= VERIFIED_MAX {
                v.clear();
            }
            v.insert(key, now + VERIFIED_TTL);
        }
        ok
    }

    /// The `WWW-Authenticate` challenge of a 401, only when Basic is on (the
    /// token-only 401 stays as it always was).
    pub fn challenge(&self) -> Option<&'static str> {
        self.basic
            .is_some()
            .then_some("Basic realm=\"tole\", charset=\"UTF-8\"")
    }

    pub fn basic_enabled(&self) -> bool {
        self.basic.is_some()
    }

    fn lock_verified(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Instant>> {
        self.verified.lock().unwrap_or_else(|p| p.into_inner())
    }
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
