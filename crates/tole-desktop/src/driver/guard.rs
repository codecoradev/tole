//! Small helpers every driver's threads share: a call into the `Api` that
//! cannot take the thread down, and the rules for probing a refused stream.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use tole_client::Error;

/// Consecutive refusals (a 404, a 429, a `closed` frame) after which a reader
/// parks its session.
pub const PARK_AFTER: u32 = 3;
/// How often a parked session's stream is probed (plus a spread of up to 10 s).
pub const PROBE_EVERY: Duration = Duration::from_secs(30);

/// A call into the `Api`, with a panic turned into `None`.
pub fn guarded<T>(call: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(call)).ok()
}

/// The failure a panicked call is reported as.
pub fn panicked() -> Error {
    Error::Io(io::Error::other("the call panicked"))
}

/// Whether the serve refused a stream (404 unknown session, 429 no free
/// stream slot, any other status), as opposed to a network failure.
pub fn refused(e: &Error) -> bool {
    matches!(e, Error::Status { .. })
}

/// The wait between two probes of a parked session: [`PROBE_EVERY`] plus a
/// spread of up to 10 s that depends on the session and the attempt, so
/// readers that parked together do not probe together.
pub fn probe_wait(session: &str, attempt: u32) -> Duration {
    let mut h = DefaultHasher::new();
    (session, attempt).hash(&mut h);
    PROBE_EVERY + Duration::from_millis(h.finish() % 10_000)
}
