//! #137: `tole serve --transport mcp` — multi-session MCP over
//! Streamable HTTP.
//!
//! One authenticated MCP connection addresses N durable tole sessions:
//! the `tole_session_*` tools (from [`crate::session_tools`]) ride
//! alongside the regular registry tools. rmcp's
//! `StreamableHttpService` is a tower Service; it is served with hyper
//! directly (no axum — the smallest HTTP stack that can host a tower
//! Service) behind the same bearer-token auth as the REST transport.
//!
//! Wire: hyper accept loop → auth check (401 pre-routing) →
//! StreamableHttpService (MCP JSON-RPC + SSE) → RegistryServer (with
//! session tools) → session_host machinery.

use anyhow::{Context, Result};
use std::sync::Arc;

use tole_cli::session_tools::SessionToolState;
use tole_core::memory::MemoryConfig;

/// Largest request body accepted: the same ceiling as the REST transport.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Why a request body was not accepted.
#[derive(Debug, PartialEq, Eq)]
enum BodyError {
    /// More than `MAX_BODY_BYTES` (declared or streamed): answered with 413.
    TooLarge,
    /// Read error or timeout: answered with 400.
    Unreadable,
}

/// Collect a request body under a time limit AND a size ceiling. Any read
/// error or timeout is an `Err` — never silently an empty body (#277) — and
/// a body over `max_bytes` is rejected as it streams in, so neither a
/// declared nor a chunked oversized body is ever buffered whole.
async fn collect_body_bounded<B>(
    body: B,
    limit: std::time::Duration,
    max_bytes: usize,
) -> Result<hyper::body::Bytes, BodyError>
where
    B: hyper::body::Body,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use http_body_util::{BodyExt, LengthLimitError, Limited};
    match tokio::time::timeout(limit, Limited::new(body, max_bytes).collect()).await {
        Ok(Ok(c)) => Ok(c.to_bytes()),
        Ok(Err(e)) if e.downcast_ref::<LengthLimitError>().is_some() => Err(BodyError::TooLarge),
        _ => Err(BodyError::Unreadable),
    }
}

/// Serve MCP over Streamable HTTP. Blocks until the listener errors.
#[allow(clippy::too_many_arguments)]
pub async fn run_mcp_http(
    bind: &str,
    port: u16,
    token: &str,
    allow_patterns: Vec<String>,
    plan_mode: bool,
    memory: Option<MemoryConfig>,
    workspace: Option<&String>,
    sessions_dir: Option<std::path::PathBuf>,
    turnend: Vec<String>,
) -> Result<()> {
    use hyper_util::rt::TokioIo;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    eprintln!(
        "tole serve (mcp): listening on http://{addr} ({} allow pattern(s), plan_mode={})",
        allow_patterns.len(),
        plan_mode
    );

    // The jail-of-jails root: explicit `--workspace` wins (canonicalized
    // like the run host resolves it); default stays the server cwd.
    let workspace_root = crate::resolve_workspace_root(workspace)
        .context("resolving --workspace for the session jail root")?;
    // The MCP service: the session-tool state is shared across the
    // service factory's instances (one Arc per connection).
    let session_state = Arc::new(SessionToolState::new(
        allow_patterns.clone(),
        plan_mode,
        memory,
        workspace_root,
        sessions_dir,
        turnend,
    ));

    // The registry: the standard server-mode tools + the session tools,
    // plus the per-session resolver — a tool call carrying `session_id`
    // routes to THAT session's registry (its jail + approver).
    let registry = crate::build_server_registry_for_mcp(plan_mode)?;
    let session_tools = session_state.tools();
    let resolver_sessions = Arc::clone(&session_state.sessions);
    let resolver: tole_core::mcp_server::SessionRegistryResolver = Arc::new(move |sid: &str| {
        let sessions = tole_cli::session_host::lock_sessions(&resolver_sessions);
        sessions.map.get(sid).map(|st| Arc::clone(&st.registry))
    });
    // Session count for the ambiguity refusal (#138-documented): a
    // registry-tool call WITHOUT session_id while 2+ sessions are open
    // is refused instead of silently hitting the server-level registry.
    let count_sessions = Arc::clone(&session_state.sessions);
    let session_count: tole_core::mcp_server::SessionCountFn = Arc::new(move || {
        let sessions = tole_cli::session_host::lock_sessions(&count_sessions);
        sessions.map.len()
    });
    let server = tole_core::mcp_server::RegistryServer::with_extra_tools(registry, session_tools)
        .with_session_resolver(resolver)
        .with_session_count(session_count);

    let session_manager: Arc<LocalSessionManager> = Arc::new(LocalSessionManager::default());
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        session_manager,
        Default::default(),
    );

    // Connection cap + IO timeouts (issue #190 — parity with the REST
    // transport's #136 Wave-2 hardening): the accept loop must not pin
    // unbounded tasks/fds for slowloris clients. A semaphore permit is
    // held for the whole connection (released on close); at capacity
    // the connection is refused, exactly like the REST face. SSE
    // streams are never cut mid-turn — only the header phase and each
    // request-body read are time-bounded.
    const MAX_CONNECTIONS: usize = 32;
    const SERVE_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let conn_sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    let token = token.to_string();
    // Failed-auth limiter shared by every connection (same policy as REST).
    let limiter = Arc::new(tole_cli::auth::AuthLimiter::new());
    loop {
        // A transient accept error (EMFILE, ECONNABORTED, ...) must not end
        // the server and drop every session: report it and keep serving,
        // with a short pause so resource exhaustion does not spin.
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("tole serve (mcp): accept failed (continuing): {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let peer_ip = peer.ip();
        let limiter = Arc::clone(&limiter);
        let Ok(permit) = conn_sem.clone().try_acquire_owned() else {
            // At capacity: close immediately, same semantics as REST.
            drop(stream);
            continue;
        };
        let svc = svc.clone();
        let token = token.clone();
        tokio::spawn(async move {
            // Hold the permit for the whole connection; dropped on close.
            let _permit = permit;
            let io = TokioIo::new(stream);
            let svc_for_conn = svc.clone();
            let token_for_conn = token.clone();
            let limiter_for_conn = Arc::clone(&limiter);
            let hyper_service =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let svc = svc_for_conn.clone();
                    let token = token_for_conn.clone();
                    let limiter = Arc::clone(&limiter_for_conn);
                    async move {
                        use http_body_util::BodyExt;
                        // Bearer-token gate BEFORE the MCP service sees
                        // anything (#137 auth surface): constant-time
                        // comparison, and a per-IP limiter on failures —
                        // both shared with the REST transport. The header
                        // read itself is bounded by `header_read_timeout`
                        // on the connection below.
                        let limited = limiter.is_limited(peer_ip);
                        let auth = !limited
                            && tole_cli::auth::bearer_matches(
                                req.headers()
                                    .get(hyper::header::AUTHORIZATION)
                                    .and_then(|v| v.to_str().ok()),
                                &token,
                            );
                        // Unified response body type for both arms.
                        type RespBody = http_body_util::combinators::BoxBody<
                            hyper::body::Bytes,
                            std::io::Error,
                        >;
                        fn box_full(bytes: hyper::body::Bytes) -> RespBody {
                            http_body_util::Full::new(bytes)
                                .map_err(|never| match never {})
                                .boxed()
                        }
                        if !auth {
                            let (status, msg) = if limited {
                                (429, "{\"error\":\"too many failed auth attempts\"}")
                            } else {
                                limiter.record_failure(peer_ip);
                                (401, "{\"error\":\"unauthorized\"}")
                            };
                            let resp = hyper::Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(box_full(hyper::body::Bytes::from(msg)))
                                .map_err(|e| std::io::Error::other(e.to_string()))?;
                            return Ok::<_, std::io::Error>(resp);
                        }
                        // Tower → hyper bridge: collect the request body,
                        // call the service (Error = Infallible), adapt the
                        // response body back to a hyper body. The collect
                        // is time-bounded (#190) — an unauthenticated or
                        // trickling client can't hold the task forever;
                        // SSE RESPONSE streaming below is untouched.
                        use tower_service::Service as _;
                        let (parts, incoming) = req.into_parts();
                        // A declared body over the ceiling is refused before
                        // a single byte is read (chunked bodies are caught
                        // by the limit while streaming, below).
                        let declared = parts
                            .headers
                            .get(hyper::header::CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<usize>().ok());
                        if declared.is_some_and(|n| n > MAX_BODY_BYTES) {
                            let resp = hyper::Response::builder()
                                .status(413)
                                .header("content-type", "application/json")
                                .body(box_full(hyper::body::Bytes::from(
                                    "{\"error\":\"request body too large\"}",
                                )))
                                .map_err(|e| std::io::Error::other(e.to_string()))?;
                            return Ok::<_, std::io::Error>(resp);
                        }
                        // A read error/timeout REJECTS the request: a
                        // truncated body must never execute as a valid
                        // empty one (#277).
                        let body_bytes =
                            match collect_body_bounded(incoming, SERVE_IO_TIMEOUT, MAX_BODY_BYTES)
                                .await
                            {
                                Ok(b) => b,
                                Err(e) => {
                                    let (status, msg) = if e == BodyError::TooLarge {
                                        (413, "{\"error\":\"request body too large\"}")
                                    } else {
                                        (400, "{\"error\":\"request body read failed\"}")
                                    };
                                    let resp = hyper::Response::builder()
                                        .status(status)
                                        .header("content-type", "application/json")
                                        .body(box_full(hyper::body::Bytes::from(msg)))
                                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                                    return Ok::<_, std::io::Error>(resp);
                                }
                            };
                        let full_req: http::Request<http_body_util::Full<hyper::body::Bytes>> =
                            http::Request::from_parts(parts, http_body_util::Full::new(body_bytes));
                        let mut svc = svc;
                        let resp = match svc.call(full_req).await {
                            Ok(r) => r,
                            Err(infallible) => match infallible {},
                        };
                        let (rp, rbody) = resp.into_parts();
                        // STREAM the body (CodeCora round-2): collecting
                        // an SSE response before sending would withhold
                        // all bytes for the entire tool call — long
                        // tole_session_prompt turns would trip client
                        // idle timeouts and drop incremental updates.
                        // BodyStream forwards frames as they arrive.
                        let body: RespBody = http_body_util::BodyStream::new(rbody)
                            .map_err(|inf| match inf {})
                            .boxed();
                        Ok(hyper::Response::from_parts(rp, body))
                    }
                });
            // hyper's raw http1 builder stays (H1-only: no h2c
            // sniffing on this authenticated transport). The
            // header_read_timeout REQUIRES a wired timer — without one
            // hyper panics per connection ("no timer set"; caught by
            // the pre-tag live smoke test, invisible to CI). Wire
            // hyper_util's TokioTimer (its `tokio` feature, already
            // enabled) BEFORE serve_connection.
            let _ = hyper::server::conn::http1::Builder::new()
                .header_read_timeout(SERVE_IO_TIMEOUT)
                .timer(hyper_util::rt::TokioTimer::new())
                .serve_connection(io, hyper_service)
                .await;
        });
    }
}

#[cfg(test)]
mod body_tests {
    use super::{collect_body_bounded, BodyError};
    use hyper::body::{Body, Frame};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// Yields one data frame, then errors or stalls forever.
    struct Broken {
        sent: bool,
        stall: bool,
    }
    impl Body for Broken {
        type Data = hyper::body::Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if !self.sent {
                self.sent = true;
                return Poll::Ready(Some(Ok(Frame::data(hyper::body::Bytes::from("{\"par")))));
            }
            if self.stall {
                Poll::Pending
            } else {
                Poll::Ready(Some(Err(std::io::Error::other("reset"))))
            }
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn read_error_is_rejected_not_empty() {
        let r = rt().block_on(collect_body_bounded(
            Broken {
                sent: false,
                stall: false,
            },
            Duration::from_secs(5),
            1024,
        ));
        assert_eq!(
            r,
            Err(BodyError::Unreadable),
            "a body read error must reject"
        );
    }

    #[test]
    fn read_timeout_is_rejected_not_empty() {
        let r = rt().block_on(collect_body_bounded(
            Broken {
                sent: false,
                stall: true,
            },
            Duration::from_millis(50),
            1024,
        ));
        assert_eq!(
            r,
            Err(BodyError::Unreadable),
            "a body read timeout must reject"
        );
    }

    #[test]
    fn complete_body_is_returned() {
        let r = rt().block_on(collect_body_bounded(
            http_body_util::Full::new(hyper::body::Bytes::from("ok")),
            Duration::from_secs(5),
            1024,
        ));
        assert_eq!(r.unwrap().as_ref(), b"ok");
    }

    #[test]
    fn a_body_over_the_ceiling_is_rejected_while_streaming() {
        let big = hyper::body::Bytes::from(vec![b'x'; 2048]);
        let r = rt().block_on(collect_body_bounded(
            http_body_util::Full::new(big),
            Duration::from_secs(5),
            1024,
        ));
        assert_eq!(r, Err(BodyError::TooLarge));
        // Exactly at the ceiling is fine.
        let exact = hyper::body::Bytes::from(vec![b'x'; 1024]);
        let r = rt().block_on(collect_body_bounded(
            http_body_util::Full::new(exact),
            Duration::from_secs(5),
            1024,
        ));
        assert_eq!(r.unwrap().len(), 1024);
    }
}
