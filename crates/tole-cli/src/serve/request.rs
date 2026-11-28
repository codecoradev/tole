//! Reading one request off a connection: the request line, headers and body, under a
//! size ceiling and a total deadline per request.

use super::SERVE_IO_TIMEOUT;
use anyhow::Result;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};
use tole_cli::serve_support::device_tokens::Scope;
use tole_cli::serve_support::transport::Conn;

/// Total budget for reading one request (request line, headers and body).
/// The per-read timeout alone lets a client that trickles one byte every
/// few seconds pin a connection slot indefinitely (slow-loris); this
/// deadline bounds the whole read.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(15);

/// Body budget of an authenticated image upload (#558): up to 10 MiB from a
/// phone link needs more than [`REQUEST_READ_DEADLINE`]. Only granted once the
/// headers carried the right token.
const UPLOAD_READ_DEADLINE: Duration = Duration::from_secs(120);

pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) authorization: Option<String>,
    /// The scope `read_request` already found for this header (`Some(None)`: refused; only an
    /// image upload is checked while reading), so it is hashed once.
    pub(super) verdict: Option<Option<Scope>>,
    /// `Last-Event-ID`: where an SSE client left off (`EventSource` sends it
    /// on reconnect).
    pub(super) last_event_id: Option<String>,
    /// Raw body bytes (an image upload is binary; JSON routes decode it).
    pub(super) body: Vec<u8>,
    /// Signature and delivery headers of a webhook (#582).
    pub(super) hook: tole_core::webhook::Headers,
}

/// Why a request could not be read; decides the response (if any).
pub(super) enum ReadError {
    /// Connection-level failure or timeout: nothing useful to answer.
    Io,
    /// Syntactically unacceptable request: answered with 400.
    Bad(&'static str),
    /// Declared body over the ceiling. Carries the `Authorization` header
    /// so the caller authenticates (and rate-limits) before answering: a
    /// 413 only for the right token, otherwise the same 401 a small body
    /// gets. The ceiling depends on the token, so answering 413 before
    /// auth would be a token-validity oracle outside the limiter (#558).
    TooLarge {
        authorization: Option<String>,
        verdict: Option<Option<Scope>>,
    },
    /// `/hooks/<route>` for a route that does not exist (404, body unread).
    NoHookRoute,
    /// A webhook body over its route's cap (413, body unread). Webhooks do
    /// not use the token, so this is no token oracle.
    HookTooLarge,
}

/// Ceiling for the request line + all header bytes combined: a client
/// that streams an endless line without a newline must not buffer
/// gigabytes pre-auth (CodeCora scan-3: unbounded buffering).
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Ceiling for a request body.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Ceiling for an authenticated image upload body (#558): the core's
/// per-image cap. Unauthenticated requests keep [`MAX_BODY_BYTES`].
const MAX_UPLOAD_BYTES: usize = tole_core::image::MAX_IMAGE_BYTES as usize;

/// A reader that re-arms the socket read timeout before every read so the
/// remaining time until `deadline` — not just the per-read timeout —
/// bounds the whole request read.
pub(super) struct DeadlineReader<'a> {
    pub(super) stream: &'a Conn,
    pub(super) deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.stream
            .set_read_timeout(Some(remaining.min(SERVE_IO_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

pub(super) fn read_request(
    stream: &Conn,
    auth: &tole_cli::auth::Authenticator,
    limited: bool,
    hooks: &std::collections::BTreeMap<String, tole_cli::hooks::Route>,
) -> Result<Request, ReadError> {
    let mut reader = BufReader::new(DeadlineReader {
        stream,
        deadline: Instant::now() + REQUEST_READ_DEADLINE,
    });
    let mut request_line = String::new();
    // take() bounds the read: an over-long line arrives truncated with no
    // trailing newline, which the caller treats as a protocol error.
    reader
        .by_ref()
        .take(MAX_HEADER_BYTES as u64)
        .read_line(&mut request_line)
        .map_err(|_| ReadError::Io)?;
    if !request_line.ends_with('\n') {
        return Err(ReadError::Io);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(ReadError::Bad("malformed request line"))?;
    let path = parts
        .next()
        .ok_or(ReadError::Bad("malformed request line"))?;
    let (method, path) = (method.to_string(), path.to_string());
    let mut authorization = None;
    let mut last_event_id = None;
    let mut content_length: Option<usize> = None;
    let mut expect_continue = false;
    let mut hook = tole_core::webhook::Headers::default();
    // The request line counts against the same ceiling as the headers:
    // the documented limit is 32 KiB for both combined.
    let mut header_bytes = request_line.len();
    loop {
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ReadError::Io);
        }
        let mut line = String::new();
        let n = reader
            .by_ref()
            .take((MAX_HEADER_BYTES - header_bytes) as u64)
            .read_line(&mut line)
            .map_err(|_| ReadError::Io)?;
        if n == 0 {
            return Err(ReadError::Io);
        }
        header_bytes += n;
        if !line.ends_with('\n') {
            return Err(ReadError::Io);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        // Header names are case-insensitive (RFC 9110 §5.1); values keep
        // their original case (the bearer token is case-sensitive).
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            // Two Content-Length headers (or an unparsable one) are a
            // request-smuggling hazard: reject instead of last-wins.
            if content_length.is_some() {
                return Err(ReadError::Bad("duplicate Content-Length"));
            }
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| ReadError::Bad("invalid Content-Length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // Chunked bodies are not supported; guessing a framing is
            // exactly what smuggling relies on.
            return Err(ReadError::Bad("Transfer-Encoding is not supported"));
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("last-event-id") {
            last_event_id = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-tole-timestamp") {
            hook.timestamp = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-tole-signature") {
            hook.signature = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-tole-delivery") {
            hook.delivery = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-hub-signature-256") {
            hook.github_signature = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-github-delivery") {
            hook.github_delivery = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-github-event")
            || name.eq_ignore_ascii_case("x-tole-event")
        {
            hook.event = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }
    }
    let content_length = content_length.unwrap_or(0);
    // A webhook: an unknown route or a body over the route's cap is
    // answered before a byte of the body is read (#582).
    if let Some(route) = hook_target(&path) {
        let cap = hooks.get(route).ok_or(ReadError::NoHookRoute)?.max_body;
        if content_length > cap {
            return Err(ReadError::HookTooLarge);
        }
    }
    // The larger upload ceiling only for an image upload that already
    // carries a valid credential: pre-auth buffering stays at MAX_BODY_BYTES.
    // An IP over its failure budget is not even checked (no hashing for it).
    let verdict = (method == "POST" && upload_target(&path).is_some() && !limited)
        .then(|| auth.authenticate(authorization.as_deref()));
    // Only a token that may upload gets the larger body ceiling.
    let upload = verdict == Some(Some(Scope::Full));
    let ceiling = if upload {
        MAX_UPLOAD_BYTES
    } else {
        MAX_BODY_BYTES
    };
    if content_length > ceiling {
        // Refuse rather than buffer it.
        return Err(ReadError::TooLarge {
            authorization,
            verdict,
        });
    }
    if upload {
        reader.get_mut().deadline = Instant::now() + UPLOAD_READ_DEADLINE;
    }
    if content_length > 0 && expect_continue {
        // curl sends `Expect: 100-continue` for bodies over ~1 KiB and
        // waits ~1s for this interim response before sending the body.
        let mut w = stream;
        let _ = w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = w.flush();
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).map_err(|_| ReadError::Io)?;
    }
    Ok(Request {
        method,
        path,
        authorization,
        verdict,
        last_event_id,
        body,
        hook,
    })
}

/// The route of a `/hooks/<route>` path.
pub(super) fn hook_target(path: &str) -> Option<&str> {
    path.strip_prefix("/hooks/")
        .map(|r| r.split('?').next().unwrap_or(r))
}

/// `/sessions/{id}/images` -> `id` (the upload route).
pub(super) fn upload_target(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/sessions/")?.strip_suffix("/images")?;
    (!id.is_empty() && !id.contains('/') && !id.contains('?')).then_some(id)
}

/// `/sessions/{id}/images/{hash}` -> `(id, hash)` (the read route).
pub(super) fn image_target(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/sessions/")?;
    let (id, hash) = rest.split_once("/images/")?;
    let ok = |s: &str| !s.is_empty() && !s.contains('/') && !s.contains('?');
    (ok(id) && ok(hash)).then_some((id, hash))
}

/// `{collection}{id}/events[?query]` -> `(id, query)`: `/sessions/` and
/// `/discussions/` have an event stream.
pub(super) fn events_target<'a>(path: &'a str, collection: &str) -> Option<(&'a str, &'a str)> {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let id = path.strip_prefix(collection)?.strip_suffix("/events")?;
    (!id.is_empty() && !id.contains('/')).then_some((id, query))
}
