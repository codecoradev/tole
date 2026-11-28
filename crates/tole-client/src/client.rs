//! The blocking [`Client`]: one method per route.

use crate::body::ErasedBody;
use crate::error::{status_error, Error, Result, MAX_ERROR_BODY};
use crate::http::{self, Request, ResponseBody};
use crate::info::Info;
use crate::path::{path_of, pct};
use crate::resume::{ReconnectPolicy, ResumingStream};
use crate::route::Route;
use crate::sse::FrameReader;
use crate::transport::{TcpTransport, Transport};
use crate::types::*;
#[cfg(unix)]
use crate::unix::UnixTransport;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use tole_core::image::MAX_IMAGE_BYTES;

/// Largest JSON body the client reads (an entries page is about 1 MiB).
const MAX_BODY: u64 = 64 * 1024 * 1024;

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
        path: &str,
        body: Option<(&[u8], &str)>,
        timeout: Option<Duration>,
        deadline: Option<Duration>,
    ) -> Result<(u16, ResponseBody)> {
        if let Some(t) = timeout {
            nonzero("timeout", t)?;
        }
        let accept = match route {
            Route::Events | Route::DiscussionEvents => "text/event-stream",
            // The bytes of an image: any type serve recorded.
            Route::ReadImage => "image/*, application/json",
            _ => "application/json",
        };
        let req = Request {
            host: &self.transport.host_header(),
            method: route.method(),
            path,
            token: self.token.as_ref().map(|t| t.0.as_str()),
            body: body.map(|(b, _)| b),
            content_type: body.map_or("application/json", |(_, t)| t),
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
        let (_, buf) = self.exchange(
            route,
            &path_of(route, id, None, query),
            bytes.as_deref().map(|b| (b, "application/json")),
            MAX_BODY,
        )?;
        Ok(serde_json::from_slice(&buf)?)
    }

    /// One request and the whole answer body, read up to `limit` bytes (a
    /// longer body is an [`Error::Protocol`]). A status outside 2xx is an
    /// [`Error::Status`] carrying the server's `{"error": ...}` text. The
    /// request is sent once, never replayed.
    fn exchange(
        &self,
        route: Route,
        path: &str,
        body: Option<(&[u8], &str)>,
        limit: u64,
    ) -> Result<(ResponseBody, Vec<u8>)> {
        let (status, mut resp) = self.open(
            route,
            path,
            body,
            self.request_timeout,
            Some(self.body_deadline),
        )?;
        let mut buf = Vec::new();
        let ok = (200..300).contains(&status);
        // One byte over the cap tells a body that fits from one that does not.
        let cap = limit.min(if ok { limit } else { MAX_ERROR_BODY as u64 });
        resp.by_ref().take(cap + 1).read_to_end(&mut buf)?;
        if !ok {
            return Err(status_error(status, &buf));
        }
        if buf.len() as u64 > limit {
            return Err(Error::Protocol(format!(
                "the answer is longer than the {limit}-byte limit"
            )));
        }
        Ok((resp, buf))
    }

    pub fn health(&self) -> Result<Health> {
        self.call(Route::Health, None, "", None)
    }

    /// `GET /info` (#1024): the server's version, wire version and features.
    pub fn info(&self) -> Result<Info> {
        self.call(Route::Info, None, "", None)
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
        self.stream(Route::Events, id, after, idle_timeout)
    }

    /// One connection of the event stream of discussion `id` (#1094), from the
    /// `seq` after `after`: `changed` frames say the ledger moved (read it with
    /// [`Client::discussion`]), and `closed` ends it. Use
    /// [`Client::follow_discussion`] to reconnect.
    pub fn discussion_events(
        &self,
        id: &str,
        after: u64,
        idle_timeout: Option<Duration>,
    ) -> Result<FrameReader<ResponseBody>> {
        self.stream(Route::DiscussionEvents, id, after, idle_timeout)
    }

    pub(crate) fn stream(
        &self,
        route: Route,
        id: &str,
        after: u64,
        idle_timeout: Option<Duration>,
    ) -> Result<FrameReader<ResponseBody>> {
        let (status, mut resp) = self.open(
            route,
            &path_of(route, Some(id), None, &format!("after={after}")),
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
        ResumingStream::new(self.clone(), Route::Events, id.to_string(), after, policy)
    }

    /// [`Client::follow_events`] for a discussion (#1094): the `changed` frames
    /// of its ledger, each `seq` once and in order across reconnects, until
    /// the server says `closed`.
    pub fn follow_discussion(
        &self,
        id: &str,
        after: u64,
        policy: ReconnectPolicy,
    ) -> ResumingStream {
        ResumingStream::new(
            self.clone(),
            Route::DiscussionEvents,
            id.to_string(),
            after,
            policy,
        )
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

    /// `tole discuss start` as the owner: records the contract and returns at
    /// once; the discussion runs on the server. Follow it with
    /// [`Client::discussion`].
    pub fn start_discussion(&self, req: &DiscussionRequest) -> Result<DiscussionStarted> {
        self.call(Route::StartDiscussion, None, "", Some(req))
    }

    /// `tole discuss resume`: continue one that was interrupted.
    pub fn resume_discussion(&self, id: &str) -> Result<DiscussionResumed> {
        self.call(Route::ResumeDiscussion, Some(id), "", None)
    }

    /// Stop a discussion: it closes with the ledger's escalation.
    pub fn stop_discussion(&self, id: &str) -> Result<DiscussionStopped> {
        self.call(Route::StopDiscussion, Some(id), "", None)
    }

    /// The cron jobs with their schedule, next run and last result, and
    /// whether a scheduler is alive. Creating or editing a job is not a route.
    pub fn cron_jobs(&self) -> Result<CronJobs> {
        self.call(Route::CronJobs, None, "", None)
    }

    /// Pause a job (by its id). Always allowed.
    pub fn pause_cron_job(&self, id: &str) -> Result<CronJobState> {
        self.call(Route::PauseCronJob, Some(id), "", None)
    }

    /// Resume a job (by its id). A `403` ([`Error::Status`]) means the owner
    /// has not reviewed it: that is done at a terminal with `tole cron resume`.
    pub fn resume_cron_job(&self, id: &str) -> Result<CronJobState> {
        self.call(Route::ResumeCronJob, Some(id), "", None)
    }

    /// Run history, newest first: all jobs, or one job's, at most `limit`.
    pub fn cron_runs(&self, job: Option<&str>, limit: Option<usize>) -> Result<CronRuns> {
        let mut query = Vec::new();
        if let Some(j) = job {
            query.push(format!("job={}", pct(j)));
        }
        if let Some(l) = limit {
            query.push(format!("limit={l}"));
        }
        self.call(Route::CronRuns, None, &query.join("&"), None)
    }

    /// The owner's profiles (the helpers), read-only: persona summary, model
    /// tiers, memory room and what each may use. Editing one is not a route.
    pub fn profiles(&self) -> Result<Profiles> {
        self.call(Route::Profiles, None, "", None)
    }

    /// Skills with their approval state, the drafts waiting for review and the
    /// archive, read-only. Approving a skill is not a route.
    pub fn skills(&self) -> Result<SkillsState> {
        self.call(Route::Skills, None, "", None)
    }

    /// The connected (MCP) tools with their trust state from the plugin lock,
    /// read-only; no server is started to answer. Approving one is not a route.
    pub fn tools(&self) -> Result<ToolsState> {
        self.call(Route::Tools, None, "", None)
    }

    /// Steer the running turn without cancelling it: the text reaches the
    /// agent at the turn's next safe point, as an attributed user message.
    /// `409` ([`Error::Status`]) when no turn is running.
    pub fn interrupt(&self, id: &str, req: &InterruptRequest) -> Result<InterruptReply> {
        self.call(Route::Interrupt, Some(id), "", Some(req))
    }

    /// Stop waiting for the running turn without aborting it: it ends
    /// `detached`, resumable, and its [`Client::prompt`] answers so. `409`
    /// when no turn is running.
    pub fn detach(&self, id: &str) -> Result<DetachReply> {
        self.call(Route::Detach, Some(id), "", None)
    }

    /// Fork the session at a settled entry into a new session file (not
    /// live: open it with [`Client::resume_session`]). `409` while a turn
    /// runs, `400` for an `at` that is not a settled entry's `seq`.
    pub fn fork(&self, id: &str, req: &ForkRequest) -> Result<ForkReply> {
        self.call(Route::Fork, Some(id), "", Some(req))
    }

    /// The session's family: its parent chain and every fork below it.
    pub fn tree(&self, id: &str) -> Result<SessionTree> {
        self.call(Route::Tree, Some(id), "", None)
    }

    /// Ask for a compaction: the session's NEXT prompt compacts first.
    /// `409` while a turn runs.
    pub fn compact(&self, id: &str) -> Result<CompactReply> {
        self.call(Route::Compact, Some(id), "", None)
    }

    /// Give the session a name. `400` for a blank, over-long or
    /// control-character name, `409` while a turn runs.
    pub fn set_name(&self, id: &str, req: &NameRequest) -> Result<NameReply> {
        self.call(Route::Name, Some(id), "", Some(req))
    }

    /// Switch the session's model to one of [`Client::models`]; it survives a
    /// resume. `400` for a model the owner does not allow, `409` while a turn
    /// runs.
    pub fn set_model(&self, id: &str, req: &ModelRequest) -> Result<ModelReply> {
        self.call(Route::SetModel, Some(id), "", Some(req))
    }

    /// Set the thinking level, or clear it ([`EffortRequest::effort`] `None`).
    /// `409` while a turn runs.
    pub fn set_effort(&self, id: &str, req: &EffortRequest) -> Result<EffortReply> {
        self.call(Route::Effort, Some(id), "", Some(req))
    }

    /// Upload one image (PNG, JPEG, GIF or WebP, at most
    /// [`MAX_IMAGE_BYTES`]) to a live session; pass the returned `id` in
    /// [`PromptRequest::images`]. A larger image is refused here
    /// ([`Error::InvalidConfig`]) before a byte is sent: the server would
    /// refuse it too, but only after reading it. The request is sent once and
    /// never replayed.
    pub fn upload_image(&self, id: &str, data: &[u8]) -> Result<ImageUploaded> {
        if data.len() as u64 > MAX_IMAGE_BYTES {
            return Err(Error::InvalidConfig(format!(
                "an image is at most {MAX_IMAGE_BYTES} bytes, this one has {}",
                data.len()
            )));
        }
        let (_, buf) = self.exchange(
            Route::UploadImage,
            &path_of(Route::UploadImage, Some(id), None, ""),
            // The server never trusts this header: it sniffs the bytes.
            Some((data, "application/octet-stream")),
            MAX_BODY,
        )?;
        Ok(serde_json::from_slice(&buf)?)
    }

    /// The bytes of an image the session's log records, by its `fnv1a64`
    /// (the `id` of [`Client::upload_image`], or the hash in an entry). At
    /// most [`MAX_IMAGE_BYTES`] are read; a longer answer is an error.
    pub fn image(&self, id: &str, fnv1a64: &str) -> Result<Image> {
        let (resp, data) = self.exchange(
            Route::ReadImage,
            &path_of(Route::ReadImage, Some(id), Some(fnv1a64), ""),
            None,
            MAX_IMAGE_BYTES,
        )?;
        let mime = resp
            .content_type()
            .map(|t| t.split(';').next().unwrap_or(t).trim().to_ascii_lowercase())
            .ok_or_else(|| Error::Protocol("the image answer has no Content-Type".into()))?;
        Ok(Image { mime, data })
    }

    /// Start a chained mission (`tole mission`) on the server. It is its own
    /// session and runs on a thread, one at a time (`409` while another runs,
    /// and on a plan-mode server); follow it with [`Client::mission`].
    pub fn start_mission(&self, req: &MissionRequest) -> Result<MissionStarted> {
        self.call(Route::StartMission, None, "", Some(req))
    }

    /// A mission's state: running, or how it ended with its summary. Also
    /// answers for an earlier mission whose file is in the sessions directory.
    pub fn mission(&self, id: &str) -> Result<Mission> {
        self.call(Route::Mission, Some(id), "", None)
    }

    /// Cancel a running mission: it ends `stopped` at its next checkpoint.
    /// `409` once it has ended.
    pub fn cancel_mission(&self, id: &str) -> Result<MissionCancelled> {
        self.call(Route::CancelMission, Some(id), "", None)
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
    fn the_token_never_prints() {
        let c = Client::connect_tcp("http://127.0.0.1:1", Some("sekrit".into()), false).unwrap();
        assert!(!format!("{c:?}").contains("sekrit"));
    }
}
