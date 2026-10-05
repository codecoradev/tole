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
    loop {
        let (stream, _peer) = listener.accept().await?;
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
            let hyper_service =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let svc = svc_for_conn.clone();
                    let token = token_for_conn.clone();
                    async move {
                        use http_body_util::BodyExt;
                        // Bearer-token gate BEFORE the MCP service sees
                        // anything (#137 auth surface), under the IO
                        // timeout so a trickling client can't pin the
                        // task (#190, mirrors the REST face).
                        let auth = tokio::time::timeout(SERVE_IO_TIMEOUT, async {
                            req.headers()
                                .get(hyper::header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .and_then(|v| v.strip_prefix("Bearer "))
                                .map(|t| t == token)
                                .unwrap_or(false)
                        })
                        .await
                        .unwrap_or(false);
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
                            let resp = hyper::Response::builder()
                                .status(401)
                                .header("content-type", "application/json")
                                .body(box_full(hyper::body::Bytes::from(
                                    "{\"error\":\"unauthorized\"}",
                                )))
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
                        let body_bytes: hyper::body::Bytes =
                            tokio::time::timeout(SERVE_IO_TIMEOUT, incoming.collect())
                                .await
                                .map(|c| c.map(|c| c.to_bytes()).unwrap_or_default())
                                .unwrap_or_default();
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
            let _ = hyper::server::conn::http1::Builder::new()
                // Slowloris hardening (#190, hyper 1.x): bound how long
                // the connection may take to deliver request headers.
                // (REST parity; the body read is separately bounded by
                // SERVE_IO_TIMEOUT above, and SSE responses are not cut.)
                .header_read_timeout(SERVE_IO_TIMEOUT)
                .serve_connection(io, hyper_service)
                .await;
        });
    }
}
