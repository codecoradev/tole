//! The blocking [`Client`]: one method per route.

use crate::error::{Error, Result};
use crate::http::{self, Request, ResponseBody};
use crate::resume::{ReconnectPolicy, ResumingStream};
use crate::sse::FrameReader;
use crate::transport::{TcpTransport, Transport};
use crate::types::*;
#[cfg(unix)]
use crate::unix::UnixTransport;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// Largest JSON body the client reads (an entries page is about 1 MiB).
const MAX_BODY: u64 = 64 * 1024 * 1024;

/// The serve routes this client has a method for. A change on the server that
/// removes one fails the contract test, which probes every entry of [`ALL`].
///
/// [`ALL`]: Route::ALL
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Health,
    Sessions,
    SessionIndex,
    CreateSession,
    Prompt,
    Cancel,
    Status,
    Entries,
    Events,
    Approvals,
    Decide,
    Models,
    Discussions,
    Discussion,
    DiscussionEstimate,
}

impl Route {
    pub const ALL: [Route; 15] = [
        Route::Health,
        Route::Sessions,
        Route::SessionIndex,
        Route::CreateSession,
        Route::Prompt,
        Route::Cancel,
        Route::Status,
        Route::Entries,
        Route::Events,
        Route::Approvals,
        Route::Decide,
        Route::Models,
        Route::Discussions,
        Route::Discussion,
        Route::DiscussionEstimate,
    ];

    pub fn method(self) -> &'static str {
        match self {
            Route::CreateSession
            | Route::Prompt
            | Route::Cancel
            | Route::Decide
            | Route::DiscussionEstimate => "POST",
            _ => "GET",
        }
    }

    /// The path, with `{id}` where the session or approval id goes.
    pub fn template(self) -> &'static str {
        match self {
            Route::Health => "/health",
            Route::Sessions => "/sessions",
            Route::SessionIndex => "/sessions/index",
            Route::CreateSession => "/sessions",
            Route::Prompt => "/sessions/{id}/prompt",
            Route::Cancel => "/sessions/{id}/cancel",
            Route::Status => "/sessions/{id}/status",
            Route::Entries => "/sessions/{id}/entries",
            Route::Events => "/sessions/{id}/events",
            Route::Approvals => "/approvals",
            Route::Decide => "/approvals/{id}/decision",
            Route::Models => "/models",
            Route::Discussions => "/discussions",
            Route::Discussion => "/discussions/{id}",
            Route::DiscussionEstimate => "/discussions/estimate",
        }
    }
}

/// Filters of `GET /sessions/index`.
#[derive(Debug, Clone, Default)]
pub struct IndexQuery {
    pub limit: Option<usize>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
}

/// A bearer token that never prints.
#[derive(Clone)]
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A connection to one `tole serve`. Cheap to clone; every call opens its own
/// connection, so a clone can be used from another thread.
#[derive(Clone)]
pub struct Client {
    transport: Arc<dyn Transport>,
    token: Option<Secret>,
    request_timeout: Option<Duration>,
    body_deadline: Duration,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("host", &self.transport.host_header())
            .field("token", &self.token)
            .finish()
    }
}

fn nonzero(what: &str, d: Duration) -> Result<()> {
    if d.is_zero() {
        return Err(Error::InvalidConfig(format!("the {what} cannot be zero")));
    }
    Ok(())
}

/// Percent-encode one path segment or query value.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Client {
    pub fn new(transport: impl Transport + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
            token: None,
            request_timeout: None,
            body_deadline: Duration::from_secs(60),
        }
    }

    /// Plain HTTP to `url` (`http://host:port`). See [`TcpTransport::new`] for
    /// the loopback rule that `trusted_private_network` relaxes.
    pub fn connect_tcp(
        url: &str,
        token: Option<String>,
        trusted_private_network: bool,
    ) -> Result<Self> {
        let mut c = Self::new(TcpTransport::new(url, trusted_private_network)?);
        c.token = token.map(Secret);
        Ok(c)
    }

    /// HTTP over the Unix socket of `tole serve --socket <path>` (macOS,
    /// Linux). A socket in a directory that is not owner-only is refused
    /// ([`Error::InsecureSocket`]); see [`UnixTransport::new`].
    #[cfg(unix)]
    pub fn connect_unix(
        path: impl Into<std::path::PathBuf>,
        token: Option<String>,
    ) -> Result<Self> {
        let mut c = Self::new(UnixTransport::new(path)?);
        c.token = token.map(Secret);
        Ok(c)
    }

    /// Send `Authorization: Bearer <token>` on every request.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(Secret(token.into()));
        self
    }

    /// Bound each blocking read of a JSON call. Unset by default: a prompt
    /// answers when its turn ends, which can take minutes. A zero timeout is
    /// refused ([`Error::InvalidConfig`]).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self> {
        nonzero("request timeout", timeout)?;
        self.request_timeout = Some(timeout);
        Ok(self)
    }

    /// Bound how long a JSON answer may take once its first byte (the status
    /// line) is in: the rest of the head and the body must follow within it
    /// (default 60 s). It does not limit the wait for that first byte: a
    /// prompt waits for its turn before the server answers (see
    /// [`Client::with_request_timeout`] for a per-read bound). It exists for a
    /// server that answers and then sends a few bytes at a time. The same
    /// bound applies to writing the request (a peer that stops reading cannot
    /// block it). A very large
    /// value (`Duration::MAX`) means no deadline; zero is refused
    /// ([`Error::InvalidConfig`]).
    pub fn with_body_deadline(mut self, deadline: Duration) -> Result<Self> {
        nonzero("body deadline", deadline)?;
        self.body_deadline = deadline;
        Ok(self)
    }

    fn open(
        &self,
        route: Route,
        id: Option<&str>,
        query: &str,
        body: Option<&[u8]>,
        timeout: Option<Duration>,
        deadline: Option<Duration>,
    ) -> Result<(u16, ResponseBody)> {
        if let Some(t) = timeout {
            nonzero("timeout", t)?;
        }
        let mut path = route.template().replace("{id}", &pct(id.unwrap_or("")));
        if !query.is_empty() {
            path.push('?');
            path.push_str(query);
        }
        let accept = if route == Route::Events {
            "text/event-stream"
        } else {
            "application/json"
        };
        let req = Request {
            host: &self.transport.host_header(),
            method: route.method(),
            path: &path,
            token: self.token.as_ref().map(|t| t.0.as_str()),
            body,
            accept,
            read_timeout: timeout,
            deadline,
        };
        http::send(self.transport.connect()?, &req)
    }

    fn call<T: DeserializeOwned>(
        &self,
        route: Route,
        id: Option<&str>,
        query: &str,
        body: Option<&dyn ErasedBody>,
    ) -> Result<T> {
        let bytes = body.map(|b| b.to_json()).transpose()?;
        let (status, mut resp) = self.open(
            route,
            id,
            query,
            bytes.as_deref(),
            self.request_timeout,
            Some(self.body_deadline),
        )?;
        let mut buf = Vec::new();
        resp.by_ref().take(MAX_BODY).read_to_end(&mut buf)?;
        if !(200..300).contains(&status) {
            let body: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
            let message = body.get("error").and_then(Value::as_str).map_or_else(
                || String::from_utf8_lossy(&buf).into_owned(),
                str::to_string,
            );
            return Err(Error::Status {
                status,
                message,
                body,
            });
        }
        Ok(serde_json::from_slice(&buf)?)
    }

    pub fn health(&self) -> Result<Health> {
        self.call(Route::Health, None, "", None)
    }

    /// Sessions live in this serve.
    pub fn sessions(&self) -> Result<SessionList> {
        self.call(Route::Sessions, None, "", None)
    }

    /// Past and live sessions of the workspace, newest first.
    pub fn session_index(&self, q: &IndexQuery) -> Result<SessionIndex> {
        let mut query = Vec::new();
        if let Some(l) = q.limit {
            query.push(format!("limit={l}"));
        }
        if let Some(c) = &q.cursor {
            query.push(format!("cursor={}", pct(c)));
        }
        self.call(Route::SessionIndex, None, &query.join("&"), None)
    }

    /// Create a session, or reopen one (`NewSession::resume`).
    pub fn create_session(&self, req: &NewSession) -> Result<SessionOpened> {
        self.call(Route::CreateSession, None, "", Some(req))
    }

    pub fn resume_session(&self, id: &str) -> Result<SessionOpened> {
        self.create_session(&NewSession::resume(id))
    }

    /// Run one turn. Blocks until it stops (see [`PromptReply::stop_reason`]);
    /// watch [`Client::events`] meanwhile for the streamed text.
    pub fn prompt(&self, id: &str, req: &PromptRequest) -> Result<PromptReply> {
        self.call(Route::Prompt, Some(id), "", Some(req))
    }

    pub fn cancel(&self, id: &str) -> Result<CancelReply> {
        self.call(Route::Cancel, Some(id), "", None)
    }

    pub fn status(&self, id: &str) -> Result<SessionStatus> {
        self.call(Route::Status, Some(id), "", None)
    }

    /// One page of the transcript: entries with `seq > after`.
    pub fn entries(&self, id: &str, after: u64, limit: Option<usize>) -> Result<EntriesPage> {
        let mut query = format!("after={after}");
        if let Some(l) = limit {
            query.push_str(&format!("&limit={l}"));
        }
        self.call(Route::Entries, Some(id), &query, None)
    }

    /// The approval queue, of one session or of all.
    pub fn approvals(&self, session: Option<&str>) -> Result<ApprovalList> {
        let query = session
            .map(|s| format!("session={}", pct(s)))
            .unwrap_or_default();
        self.call(Route::Approvals, None, &query, None)
    }

    pub fn decide(&self, approval_id: &str, decision: Decision) -> Result<DecisionReply> {
        self.decide_with(approval_id, decision, None)
    }

    /// Decide a row of [`Client::approvals`]. Use this for the delegated
    /// children's rows (`dap-*` ids): an allow for those must carry the row's
    /// `fingerprint`, which this takes from the row. It also sends the
    /// `confirm` that serve demands for them.
    pub fn decide_row(&self, row: &Approval, decision: Decision) -> Result<DecisionReply> {
        let fingerprint = row.extra.get("fingerprint").and_then(Value::as_str);
        self.decide_with(&row.id, decision, fingerprint)
    }

    fn decide_with(
        &self,
        approval_id: &str,
        decision: Decision,
        fingerprint: Option<&str>,
    ) -> Result<DecisionReply> {
        let body = DecisionRequest::new(approval_id, decision, fingerprint);
        self.call(Route::Decide, Some(approval_id), "", Some(&body))
    }

    pub fn models(&self) -> Result<Models> {
        self.call(Route::Models, None, "", None)
    }

    /// One event stream connection, from the entry after `after`. It ends when
    /// the connection does; use [`Client::follow_events`] to reconnect.
    /// `idle_timeout` bounds the silence tolerated (the server sends a
    /// heartbeat every 15 s by default).
    pub fn events(
        &self,
        id: &str,
        after: u64,
        idle_timeout: Option<Duration>,
    ) -> Result<FrameReader<ResponseBody>> {
        let (status, mut resp) = self.open(
            Route::Events,
            Some(id),
            &format!("after={after}"),
            None,
            idle_timeout,
            // The head (and a refusal's body) must follow the first byte
            // within the body deadline, like a call: a peer that drips them
            // must not stall the caller or a reconnect loop.
            Some(self.body_deadline),
        )?;
        if status == 200 {
            // The stream itself is unbounded by design: only the per-read
            // idle bound applies from here.
            resp.unlimit();
        } else {
            let mut buf = Vec::new();
            resp.by_ref().take(64 * 1024).read_to_end(&mut buf)?;
            let body: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
            let message = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("event stream refused")
                .to_string();
            return Err(Error::Status {
                status,
                message,
                body,
            });
        }
        Ok(FrameReader::new(resp))
    }

    /// The event stream with automatic reconnect: after a drop it resumes with
    /// `after=<last seq>`, so no entry is lost or delivered twice. Other
    /// frames are best effort: see [`StreamItem::Reconnected`](crate::StreamItem::Reconnected).
    pub fn follow_events(&self, id: &str, after: u64, policy: ReconnectPolicy) -> ResumingStream {
        ResumingStream::new(self.clone(), id.to_string(), after, policy)
    }

    /// The owner's discussions, as `tole discuss list --json` shows them.
    pub fn discussions(&self) -> Result<DiscussionList> {
        self.call(Route::Discussions, None, "", None)
    }

    /// One discussion: rounds, the gate's report, disagreements, the claim
    /// checks and the tokens used (never a raw contribution).
    pub fn discussion(&self, id: &str) -> Result<Discussion> {
        self.call(Route::Discussion, Some(id), "", None)
    }

    /// The dry run of `tole discuss start`: validates the request and says
    /// what it may cost. Writes nothing and calls no model.
    pub fn estimate_discussion(&self, req: &DiscussionRequest) -> Result<DiscussionEstimate> {
        self.call(Route::DiscussionEstimate, None, "", Some(req))
    }
}

/// Object-safe view of a serializable request body.
trait ErasedBody {
    fn to_json(&self) -> Result<Vec<u8>>;
}

impl<T: Serialize> ErasedBody for T {
    fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_unique_and_templated() {
        for (i, a) in Route::ALL.iter().enumerate() {
            for b in &Route::ALL[i + 1..] {
                assert_ne!((a.method(), a.template()), (b.method(), b.template()));
            }
            assert!(a.template().starts_with('/'));
        }
    }

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(pct("srv-1_a.b~"), "srv-1_a.b~");
        assert_eq!(pct("../x y"), "..%2Fx%20y");
        assert_eq!(pct("a\r\nb"), "a%0D%0Ab");
    }

    #[test]
    fn the_token_never_prints() {
        let c = Client::connect_tcp("http://127.0.0.1:1", Some("sekrit".into()), false).unwrap();
        assert!(!format!("{c:?}").contains("sekrit"));
    }
}
