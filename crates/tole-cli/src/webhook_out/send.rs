//! The sender (#911, design section 4): one drain pass over the outbox.
//!
//! A pass leases at most one due event per destination under the outbox lock
//! (`Outbox::lease_due`), then sends **without any lock**, then appends the
//! result under a short blocking lock. Everything it needs from outside is
//! injected so a pass is deterministic in tests: the poster (the network),
//! the key resolver (environment and files), the clock, and `settle`, which
//! the host uses to record the terminal ledger row. Nothing here reads a
//! config or a clock of its own.
//!
//! The sender treats the outbox as untrusted input: before every send it
//! re-validates the line (`outbox::validate`), takes the destination from the
//! map the host loaded (never from the file) and signs with the key the
//! destination names (never one the file supplies).

use std::time::Duration;

use tole_core::ledger::DeliveryStatus;
use tole_core::web::{AddrPolicy, PostError, PostParams};

use super::outbox::{self, Line};
use super::store::{Dead, Leased, Outbox};
use super::{sign, Destination, Destinations, KeySource};

/// Waits after the first four attempt results, in seconds; later ones use
/// [`LONG_STEP_SECS`].
const BACKOFF_SECS: [i64; 4] = [2, 10, 30, 120];
/// The step after the fourth attempt, with jitter.
pub const LONG_STEP_SECS: i64 = 300;
/// `Retry-After` on 429 and 503 is honoured up to this.
pub const MAX_RETRY_AFTER_SECS: i64 = 600;

/// What a request is made of.
pub struct Request<'a> {
    pub url: &'a str,
    pub body: &'a [u8],
    pub headers: &'a [(String, String)],
    pub timeout: Duration,
    pub policy: AddrPolicy,
}

/// What came back: the status and a short `Retry-After`, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub retry_after: Option<String>,
}

/// The network, behind a seam: the real one is [`NetPoster`].
pub trait Poster {
    fn post(&self, req: &Request<'_>) -> Result<Reply, PostError>;
}

/// `tole_core::web::vetted_post`: https only (a loopback host may opt into
/// http), the resolver vets and pins the address, no redirect is followed and
/// the response body is never read.
pub struct NetPoster;

impl Poster for NetPoster {
    fn post(&self, req: &Request<'_>) -> Result<Reply, PostError> {
        let mut p = PostParams::new(req.url, req.body);
        p.headers = req.headers;
        p.timeout = req.timeout;
        p.policy = req.policy;
        tole_core::web::vetted_post(&p).map(|r| Reply {
            status: r.status,
            retry_after: r.retry_after,
        })
    }
}

/// Where a key comes from at send time. The real one reads the environment
/// variable or the key file the destination names.
pub trait Keys {
    fn key(&self, d: &Destination) -> Result<Vec<u8>, &'static str>;
}

/// Reads the destination's key from its environment variable or its file,
/// again for every attempt, so a rotated key applies without a restart.
pub struct EnvFileKeys;

impl Keys for EnvFileKeys {
    fn key(&self, d: &Destination) -> Result<Vec<u8>, &'static str> {
        let key = match &d.key {
            KeySource::Env(var) => std::env::var(var)
                .ok()
                .filter(|v| !v.is_empty())
                .map(String::into_bytes),
            KeySource::File(path) => {
                if std::path::Path::new(path).is_absolute() {
                    crate::hooks::read_secret_file(path).ok()
                } else {
                    None
                }
            }
        };
        match key {
            Some(k) if k.len() >= crate::hooks::MIN_KEY => Ok(k),
            _ => Err("key_unavailable"),
        }
    }
}

/// What to do with one attempt's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Done,
    /// Try again later; `retry_after` is the receiver's hint in seconds.
    Retry {
        code: String,
        retry_after: Option<i64>,
    },
    /// Never again, with a code.
    Dead(String),
}

/// Retry on a connect or TLS error, DNS failure, timeout, 5xx, 408 and 429.
/// No retry on another 4xx (the receiver understood and refused), a 3xx
/// (redirects are never followed), or an address the policy refuses. Codes are
/// a closed set: nothing a receiver says reaches them.
pub fn classify(r: &Result<Reply, PostError>) -> Verdict {
    match r {
        Ok(resp) if (200..300).contains(&resp.status) => Verdict::Done,
        Ok(resp) => {
            let code = format!("http_{}", resp.status);
            if matches!(resp.status, 408 | 429) || (500..600).contains(&resp.status) {
                Verdict::Retry {
                    code,
                    retry_after: retry_after_secs(resp),
                }
            } else {
                Verdict::Dead(code)
            }
        }
        Err(PostError::Dns) => retry("dns"),
        Err(PostError::Timeout) => retry("timeout"),
        Err(PostError::Transport(_)) => retry("transport"),
        Err(PostError::RefusedAddress) => Verdict::Dead("refused_address".into()),
        Err(PostError::Redirect(_)) => Verdict::Dead("redirect_refused".into()),
        // The URL, the headers or the scheme: nothing a later attempt changes.
        Err(_) => Verdict::Dead("bad_destination".into()),
    }
}

fn retry(code: &str) -> Verdict {
    Verdict::Retry {
        code: code.into(),
        retry_after: None,
    }
}

/// `Retry-After` as delta-seconds only (a date is ignored), on 429 and 503.
fn retry_after_secs(r: &Reply) -> Option<i64> {
    if !matches!(r.status, 429 | 503) {
        return None;
    }
    let secs: i64 = r.retry_after.as_deref()?.trim().parse().ok()?;
    (secs >= 0).then_some(secs.min(MAX_RETRY_AFTER_SECS))
}

/// Seconds to wait after the `n`th attempt result (1-based): 2, 10, 30, 120,
/// then 300 with +/- 20 % jitter. The jitter is a hash of the event id and
/// `n`, so it is deterministic and needs no random source. A `Retry-After`
/// hint raises the wait, never lowers it, and is capped.
pub fn retry_delay(n: u32, id: &str, retry_after: Option<i64>) -> i64 {
    let base = match BACKOFF_SECS.get(n.saturating_sub(1) as usize) {
        Some(s) => *s,
        None => {
            let h = tole_core::webhook::short_digest(format!("{id}/{n}").as_bytes());
            let r = u64::from_str_radix(&h[..4], 16).unwrap_or(0) as i64; // 0..65535
            let pct = r * 41 / 65_536 - 20; // -20..=20
            LONG_STEP_SECS + LONG_STEP_SECS * pct / 100
        }
    };
    base.max(retry_after.unwrap_or(0)).min(MAX_RETRY_AFTER_SECS)
}

/// A terminal result for the host to record (the ledger `delivery` row of a
/// cron event). `fire_id` is `None` for an event with no run (an approval).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub dest: String,
    pub id: String,
    pub fire_id: Option<String>,
    pub status: DeliveryStatus,
    /// `None` when sent; otherwise a code from the closed set.
    pub code: Option<String>,
}

/// What one pass did, for tests and `tole doctor`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassStats {
    pub sent: usize,
    pub retried: usize,
    pub dead: usize,
    /// The outbox lock was held by another drainer: the pass did nothing.
    pub busy: bool,
}

/// One drain pass. Returns when every event it leased has a result line.
///
/// `clock` is read again before each send. An I/O error on the outbox ends the
/// pass: a lease that was not answered runs out and the event is retried.
pub fn drain_pass(
    outbox: &Outbox,
    dests: &Destinations,
    poster: &dyn Poster,
    keys: &dyn Keys,
    clock: &dyn Fn() -> i64,
    settle: &mut dyn FnMut(Settled),
) -> std::io::Result<PassStats> {
    let mut stats = PassStats::default();
    let max_timeout = dests
        .names()
        .filter_map(|n| dests.get(n))
        .map(|d| d.timeout.as_secs() as i64)
        .max()
        .unwrap_or(0);
    let now = clock();
    let leasing = outbox.lease_due(
        now,
        &|d| dests.get(d).map(|x| x.timeout.as_secs() as i64 + 5),
        max_timeout,
    )?;
    if !leasing.got_lock {
        stats.busy = true;
        return Ok(stats);
    }
    for Dead {
        dest,
        id,
        fire_id,
        code,
    } in leasing.dead
    {
        stats.dead += 1;
        settle(Settled {
            dest,
            id,
            fire_id,
            status: DeliveryStatus::Failed,
            code: Some(code.into()),
        });
    }
    for ev in leasing.leased {
        send_one(outbox, dests, poster, keys, clock, settle, &ev, &mut stats)?;
    }
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
fn send_one(
    outbox: &Outbox,
    dests: &Destinations,
    poster: &dyn Poster,
    keys: &dyn Keys,
    clock: &dyn Fn() -> i64,
    settle: &mut dyn FnMut(Settled),
    ev: &Leased,
    stats: &mut PassStats,
) -> std::io::Result<()> {
    let append = |line: Line| {
        outbox
            .append(&[line])
            .map_err(|_| std::io::Error::other("outbox append failed"))
    };
    let dead = |code: &str, stats: &mut PassStats, settle: &mut dyn FnMut(Settled)| {
        stats.dead += 1;
        settle(Settled {
            dest: ev.dest.clone(),
            id: ev.id.clone(),
            fire_id: ev.fire_id.clone(),
            status: DeliveryStatus::Failed,
            code: Some(code.into()),
        });
    };
    let now = clock();
    // The lease is ours only until it runs out; an earlier destination may
    // have taken its time. Skipped, the event is leased again on a later pass.
    if now >= ev.lease_until {
        return Ok(());
    }
    // The destination comes from the loaded map, never from the file.
    let Some(d) = dests.get(&ev.dest) else {
        append(Line::Dead {
            dest: ev.dest.clone(),
            id: ev.id.clone(),
            at: now,
            code: outbox::BAD_LINE.into(),
        })?;
        dead(outbox::BAD_LINE, stats, settle);
        return Ok(());
    };
    if let Err(code) = outbox::validate(&ev.dest, &ev.id, &ev.body, Some(d), now) {
        append(Line::Dead {
            dest: ev.dest.clone(),
            id: ev.id.clone(),
            at: now,
            code: code.into(),
        })?;
        dead(code, stats, settle);
        return Ok(());
    }
    let n = ev.attempts + 1;
    let verdict = match keys.key(d) {
        Err(code) => Verdict::Retry {
            code: code.into(),
            retry_after: None,
        },
        Ok(key) => {
            let event = event_type(&ev.body).unwrap_or_default();
            let headers = sign::request_headers(&key, now, &ev.id, &event, ev.body.as_bytes());
            let req = Request {
                url: &d.url,
                body: ev.body.as_bytes(),
                headers: &headers,
                timeout: d.timeout,
                policy: d.policy,
            };
            classify(&poster.post(&req))
        }
    };
    let at = clock();
    match verdict {
        Verdict::Done => {
            append(Line::Attempt {
                dest: ev.dest.clone(),
                id: ev.id.clone(),
                n,
                at,
                outcome: "sent".into(),
                next_at: 0,
            })?;
            append(Line::Done {
                dest: ev.dest.clone(),
                id: ev.id.clone(),
                at,
            })?;
            stats.sent += 1;
            settle(Settled {
                dest: ev.dest.clone(),
                id: ev.id.clone(),
                fire_id: ev.fire_id.clone(),
                status: DeliveryStatus::Sent,
                code: None,
            });
        }
        Verdict::Retry { code, retry_after } => {
            append(Line::Attempt {
                dest: ev.dest.clone(),
                id: ev.id.clone(),
                n,
                at,
                outcome: code,
                next_at: at.saturating_add(retry_delay(n, &ev.id, retry_after)),
            })?;
            stats.retried += 1;
        }
        Verdict::Dead(code) => {
            append(Line::Attempt {
                dest: ev.dest.clone(),
                id: ev.id.clone(),
                n,
                at,
                outcome: code.clone(),
                next_at: 0,
            })?;
            append(Line::Dead {
                dest: ev.dest.clone(),
                id: ev.id.clone(),
                at,
                code: code.clone(),
            })?;
            dead(&code, stats, settle);
        }
    }
    Ok(())
}

/// `type` of a body.
fn event_type(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
#[path = "send_tests.rs"]
mod tests;
