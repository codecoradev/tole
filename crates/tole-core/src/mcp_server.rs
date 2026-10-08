//! D1 (issue #94): tole as an MCP **server** over stdio.
//!
//! The registry's hardened tools — jailed file ops, argv-validated git,
//! detached jobs, the uteke/cora integrations — become callable by ANY
//! MCP client (ZCode, Claude, editor agents), instead of being locked
//! inside the CLI process. The engine is untouched: this module is a
//! thin rmcp `ServerHandler` over an ordinary [`ToolRegistry`].
//!
//! Approval policy in server context (the design decision behind D1):
//! there is no stdin human — stdin IS the protocol channel — so the
//! interactive approver is replaced by explicit pre-authorization:
//!
//! - ReadOnly tools are always callable (no approval by definition).
//! - Write tools require `--allow <glob>` patterns (the existing flag);
//!   without them every Write call settles as a tool error telling the
//!   caller how to re-authorize.
//! - **Destructive tools are structurally absent**: registration behind a
//!   non-interactive approver is refused by the registry (the three-layer
//!   invariant holds — the server cannot weaken it, only skip it).
//!
//! Logging hygiene: stderr is safe (stdio transport speaks on stdout);
//! anything writing to stdout outside the protocol would corrupt it.

use crate::tool::{Risk, ToolRegistry};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, Tool,
};
use rmcp::service::RequestContext;
use rmcp::ErrorData as McpError;
use rmcp::{RoleServer, ServiceExt};
use std::sync::Arc;

/// Resolves the tool registry of the session named by `session_id`
/// (#137: per-session jails). None when the host has no session model
/// (stdio MCP mode — the single registry is the whole surface).
pub type SessionRegistryResolver = Arc<dyn Fn(&str) -> Option<Arc<ToolRegistry>> + Send + Sync>;

/// Counts open sessions (#138): a registry-tool call WITHOUT a
/// `session_id` while 2+ sessions are open is ambiguous — the caller
/// probably meant one of them, so it is refused instead of silently
/// executing against the server-level registry.
pub type SessionCountFn = Arc<dyn Fn() -> usize + Send + Sync>;

/// An MCP server view of a [`ToolRegistry`]. Cloneable (Arc-shared
/// registry) so `call_tool` can move a handle into `spawn_blocking`.
#[derive(Clone)]
pub struct RegistryServer {
    registry: std::sync::Arc<ToolRegistry>,
    /// Multi-session HTTP hosts set this: when a tool call carries a
    /// `session_id` argument (and the name is not a session tool), the
    /// call routes to THAT session's registry (its jail + approver).
    session_resolver: Option<SessionRegistryResolver>,
    /// Open-session count for the ambiguity refusal above.
    session_count: Option<SessionCountFn>,
}

impl RegistryServer {
    /// Wrap a registry. In server mode the registry must be built with a
    /// NON-interactive approver (e.g.
    /// `AllowlistApprover::allow_only(patterns)`): interactive prompts
    /// would try to read the protocol's stdin. Destructive tools are then
    /// refused at registration and structurally absent from the server.
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry: std::sync::Arc::new(registry),
            session_resolver: None,
            session_count: None,
        }
    }

    /// Attach the per-session registry resolver (#137). Without it, tool
    /// calls always run against the server-level registry.
    pub fn with_session_resolver(mut self, resolver: SessionRegistryResolver) -> Self {
        self.session_resolver = Some(resolver);
        self
    }

    /// Attach the open-session count for the ambiguity refusal: without
    /// it (or at ≤1 open session) a no-id call keeps hitting the
    /// server-level registry (single-session / stdio behavior).
    pub fn with_session_count(mut self, count: SessionCountFn) -> Self {
        self.session_count = Some(count);
        self
    }

    /// Register additional tools AFTER construction (#137: the
    /// multi-session MCP host adds tole_session_* tools that carry their
    /// own SharedSessions state — they are not part of a plain registry
    /// build). Same risk rules apply: Destructive registration behind a
    /// non-interactive approver is refused by the registry itself.
    pub fn with_extra_tools(
        mut registry: ToolRegistry,
        tools: Vec<Box<dyn crate::tool::Tool>>,
    ) -> Self {
        for t in tools {
            registry
                .register(t)
                .expect("with_extra_tools: duplicate tool name");
        }
        Self {
            registry: std::sync::Arc::new(registry),
            session_resolver: None,
            session_count: None,
        }
    }

    fn registered_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .registry
            .specs()
            .into_iter()
            .filter_map(|s| {
                // specs() nests under function.name (OpenAI wire shape).
                s["function"]["name"].as_str().map(str::to_string)
            })
            .collect();
        names.sort();
        names
    }

    fn tool_by_name(&self, name: &str) -> Option<Tool> {
        let t = self.registry.get(name)?;
        if t.risk() == Risk::Destructive {
            // Belt and suspenders: a non-interactive server registry
            // refuses Destructive registration outright; never list one.
            return None;
        }
        let schema = t
            .spec()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
        let input_schema = Arc::new(schema.as_object().cloned().unwrap_or_default());
        Some(Tool::new(
            name.to_owned(),
            t.describe(&serde_json::Value::Null),
            input_schema,
        ))
    }

    fn execute_checked(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let session_id = args
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let is_session_tool = name.starts_with("tole_session_");
        let (registry, args) = match (&self.session_resolver, session_id) {
            // #137 multi-session routing, #226/#227 semantics: a
            // `session_id` argument names the caller's intended context —
            // resolve the SESSION registry FIRST and execute there.
            // Session registries may EXTEND the server-level registry
            // (host-added per-session tools), so server-side presence is
            // NOT required to route. An id that does not resolve is a
            // loud error, never a silent fallback to the server registry
            // (#74: that fallback let a bogus id bypass the #138
            // ambiguity refusal and execute in the server cwd jail, or
            // run a same-named server tool instead of the session's).
            // The session tools themselves (tole_session_*) stay on the
            // server-level registry — they carry the session map.
            (Some(resolve), Some(sid)) if !is_session_tool => {
                let reg = resolve(&sid).ok_or_else(|| format!("unknown session: {sid}"))?;
                // Strip the routing key before the tool sees the args.
                let mut a = args;
                if let serde_json::Value::Object(map) = &mut a {
                    map.remove("session_id");
                }
                (reg, a)
            }
            // Ambiguity refusal (#138-documented, #226/#227-tightened):
            // ANY non-session-tool call without `session_id` while 2+
            // sessions are open is refused — the caller may have meant
            // one of them (a session-only extension tool would otherwise
            // die as an opaque "unknown tool" on the server registry,
            // and a server tool may not be what the caller meant).
            (Some(_), None)
                if !is_session_tool
                    && self
                        .session_count
                        .as_ref()
                        .map(|count| count())
                        .unwrap_or(0)
                        > 1 =>
            {
                return Err(format!(
                    "ambiguous '{name}' call: 2+ sessions are open — pass the 'session_id' \
                     argument to route to the intended session"
                ));
            }
            _ => (self.registry.clone(), args),
        };
        let tool = registry
            .get(name)
            .ok_or_else(|| format!("unknown tool: {name}"))?;
        // Server-mode policy, enforced BEFORE and outside the gate (the
        // gate knows nothing about server mode). Structural guard, NOT an
        // approver decision (CodeCora scan finding): `RegistryServer::new`
        // accepts any registry, including one built with a permissive
        // approver that would Allow a Destructive call. Hiding it from
        // tools/list is not enough — it must be uncallable, period, and
        // the approver is never consulted for it.
        if tool.risk() == Risk::Destructive {
            return Err("destructive tools are never exposed in server mode".into());
        }
        // Everything else goes through the one authorization gate (#303).
        // Pre-hooks: server faces never carry them (the CLI refuses
        // --on-pretool for mcp/serve/acp; session registries wire only
        // turn-end hooks), so the gate's pre-hook step is a no-op here.
        let authorized = crate::gate::authorize(&registry, name, &args, crate::gate::Mode::Fresh)
            .map_err(|denied| match denied {
            crate::gate::Denied::UnknownTool => format!("unknown tool: {name}"),
            // Approver denial (a pre-hook denial is unreachable on
            // server faces): same message either way.
            _ => "denied by approval policy — this MCP server only pre-authorizes \
                      Write tools listed in --allow patterns"
                .to_string(),
        })?;
        authorized.execute(args)
    }
}

impl ServerHandler for RegistryServer {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .registered_names()
            .into_iter()
            .filter_map(|name| self.tool_by_name(&name))
            .collect();
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        // Tool execution is SYNCHRONOUS and can block for a long time
        // (git runs with a 120s budget; run_command 420s). It must run
        // on a blocking thread, not pin the async runtime's workers
        // (cora full-scan #29): one slow tool call would stall every
        // other request this runtime is serving.
        let server = self.clone();
        let executed = tokio::task::spawn_blocking(move || server.execute_checked(&name, args))
            .await
            .map_err(|e| McpError::internal_error(format!("tool task join failed: {e}"), None))?;
        match executed {
            Ok(result) => Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
            )])
            .into()),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e)]).into()),
        }
    }
}

/// Serve the registry over stdio (the standard MCP spawn pattern: the
/// caller launches `tole mcp` and speaks JSON-RPC on stdin/stdout).
/// Blocks until the client disconnects.
pub async fn serve_stdio(registry: ToolRegistry) -> Result<(), String> {
    use rmcp::transport::stdio;
    let service = RegistryServer::new(registry)
        .serve(stdio())
        .await
        .map_err(|e| format!("mcp server: initialize failed: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| format!("mcp server: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::AllowlistApprover;
    use crate::tool::{Risk, Tool};
    use rmcp::service::serve_client;
    use rmcp::RoleClient;
    use serde_json::json;

    struct EchoTool;
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn describe(&self, _input: &serde_json::Value) -> String {
            "echoes input".into()
        }
        fn spec(&self) -> Option<serde_json::Value> {
            Some(json!({"type": "object", "properties": {"msg": {"type": "string"}}}))
        }
        fn execute(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(json!({"echoed": input}))
        }
    }

    struct WriteTool;
    impl Tool for WriteTool {
        fn name(&self) -> &str {
            "fake_write"
        }
        fn risk(&self) -> Risk {
            Risk::Write
        }
        fn describe(&self, _input: &serde_json::Value) -> String {
            "fake write".into()
        }
        fn execute(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(json!({"wrote": input}))
        }
    }

    struct BombTool;
    impl Tool for BombTool {
        fn name(&self) -> &str {
            "bomb"
        }
        fn risk(&self) -> Risk {
            Risk::Destructive
        }
        fn describe(&self, _input: &serde_json::Value) -> String {
            "boom".into()
        }
        fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("destructive must never execute in server mode")
        }
    }

    fn server_registry(allow_write: bool) -> ToolRegistry {
        let patterns = if allow_write {
            vec!["fake_write".to_string()]
        } else {
            vec![]
        };
        let mut reg = ToolRegistry::with_approver(AllowlistApprover::allow_only(patterns));
        reg.register(Box::new(EchoTool)).unwrap();
        reg.register(Box::new(WriteTool)).unwrap();
        reg
    }

    async fn connect(server: RegistryServer) -> rmcp::service::RunningService<RoleClient, ()> {
        let (client_out, server_in) = tokio::io::duplex(8192);
        let (server_out, client_in) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            // HOLD the running service for the connection's lifetime: dropping
            // it closes the transport the moment the handshake returns
            // (the BrokenPipe the first test run tripped over).
            match server.serve((server_in, server_out)).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(e) => eprintln!("MCP_SERVER_ERR: {e:?}"),
            }
        });
        serve_client((), (client_in, client_out))
            .await
            .expect("client initialize")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_lists_only_non_destructive_tools() {
        // Destructive registration is refused by the non-interactive
        // server registry — the structural invariant holds over MCP.
        let mut reg = server_registry(false);
        assert!(
            reg.register(Box::new(BombTool)).is_err(),
            "server-mode registry must refuse Destructive tools"
        );
        let client = connect(RegistryServer::new(reg)).await;
        let listed = client.peer().list_tools(None).await.unwrap();
        let names: Vec<String> = listed.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, vec!["echo_tool", "fake_write"]);
        assert!(!names.iter().any(|n| n == "bomb"));
        client.cancel().await.ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_lists_schemas_from_spec() {
        let client = connect(RegistryServer::new(server_registry(false))).await;
        let listed = client.peer().list_tools(None).await.unwrap();
        let echo = listed
            .tools
            .iter()
            .find(|t| t.name == "echo_tool")
            .expect("echo_tool listed");
        assert!(
            echo.input_schema["properties"].get("msg").is_some(),
            "input schema must mirror spec().properties"
        );
        client.cancel().await.ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_write_without_allow_is_a_tool_error() {
        let client = connect(RegistryServer::new(server_registry(false))).await;
        let res = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("fake_write")
                    .with_arguments(json!({"x": 1}).as_object().expect("object").clone()),
            )
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("denied by approval policy"), "{text}");
        client.cancel().await.ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_write_with_allow_executes() {
        let client = connect(RegistryServer::new(server_registry(true))).await;
        let res = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("fake_write")
                    .with_arguments(json!({"x": 1}).as_object().expect("object").clone()),
            )
            .await
            .unwrap();
        assert_ne!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains('"') && text.contains("x"), "{text}");
        client.cancel().await.ok();
    }

    /// CodeCora scan regression: an embedder CAN pass a registry whose
    /// approver allows Destructive (interactive-permissive). The server
    /// must still refuse to execute it — hiding it from tools/list is not
    /// enough.
    #[tokio::test(flavor = "multi_thread")]
    async fn destructive_uncallable_even_with_permissive_registry() {
        struct PermissiveApprover;
        impl crate::approval::Approver for PermissiveApprover {
            fn decide(&self, _req: &crate::approval::ToolRequest<'_>) -> crate::approval::Verdict {
                crate::approval::Verdict::Allow
            }
            fn interactive(&self) -> bool {
                true // the only way a Destructive tool registers at all
            }
        }
        let mut reg = ToolRegistry::with_approver(PermissiveApprover);
        reg.register(Box::new(BombTool))
            .expect("permissive registry accepts it");
        let client = connect(RegistryServer::new(reg)).await;
        // Hidden from listing...
        let listed = client.peer().list_tools(None).await.unwrap();
        assert!(!listed.tools.iter().any(|t| t.name == "bomb"));
        // ...and uncallable.
        let res = client
            .peer()
            .call_tool(CallToolRequestParams::new("bomb"))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("never exposed in server mode"), "{text}");
        client.cancel().await.ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_unknown_tool_is_a_tool_error() {
        let client = connect(RegistryServer::new(server_registry(false))).await;
        let res = client
            .peer()
            .call_tool(CallToolRequestParams::new("nope"))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        client.cancel().await.ok();
    }

    // #138-documented ambiguity refusal (implemented 2026-10-05): a
    // registry-tool call without session_id while 2+ sessions are open
    // must refuse instead of silently hitting the server-level registry.
    fn session_server(open_sessions: usize) -> RegistryServer {
        let session_reg = Arc::new(server_registry(false));
        let known: Vec<String> = (0..open_sessions).map(|i| format!("s{i}")).collect();
        let resolver_sessions = known.clone();
        let resolver: SessionRegistryResolver = Arc::new(move |sid: &str| {
            if resolver_sessions.iter().any(|s| s == sid) {
                Some(Arc::clone(&session_reg))
            } else {
                None
            }
        });
        let count: SessionCountFn = Arc::new(move || known.len());
        RegistryServer::new(server_registry(false))
            .with_session_resolver(resolver)
            .with_session_count(count)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_id_call_with_two_sessions_is_refused() {
        let client = connect(session_server(2)).await;
        let res = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("echo_tool")
                    .with_arguments(json!({"msg": "hi"}).as_object().expect("object").clone()),
            )
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("ambiguous"), "{text}");
        assert!(text.contains("session_id"), "{text}");
        client.cancel().await.ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_id_call_with_one_session_still_runs_on_server_registry() {
        let client = connect(session_server(1)).await;
        let res = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("echo_tool")
                    .with_arguments(json!({"msg": "hi"}).as_object().expect("object").clone()),
            )
            .await
            .unwrap();
        assert_ne!(res.is_error, Some(true));
        client.cancel().await.ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_session_id_routes_even_with_two_sessions() {
        let client = connect(session_server(2)).await;
        let res = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("echo_tool").with_arguments(
                    json!({"session_id": "s1", "msg": "hi"})
                        .as_object()
                        .expect("object")
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_ne!(res.is_error, Some(true));
        // The routing key is stripped before the tool sees the args.
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(!text.contains("session_id"), "{text}");
        client.cancel().await.ok();
    }

    /// Issue #227 (scan #73) regression: a session registry may EXTEND
    /// the server registry — a tool that exists only in the session's
    /// registry is callable WITH its session_id instead of falling
    /// through to an opaque "unknown tool" on the server registry.
    struct SessionOnlyTool;
    impl crate::tool::Tool for SessionOnlyTool {
        fn name(&self) -> &str {
            "session_only_tool"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn describe(&self, _input: &serde_json::Value) -> String {
            "lives only in the session registry".into()
        }
        fn execute(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(json!({"from": "session registry"}))
        }
    }

    fn extension_session_server(open_sessions: usize) -> RegistryServer {
        // The SESSION registry carries a tool the server registry lacks.
        let mut sreg = server_registry(false);
        sreg.register(Box::new(SessionOnlyTool)).unwrap();
        let session_reg = Arc::new(sreg);
        let known: Vec<String> = (0..open_sessions).map(|i| format!("s{i}")).collect();
        let resolver_sessions = known.clone();
        let resolver: SessionRegistryResolver = Arc::new(move |sid: &str| {
            if resolver_sessions.iter().any(|s| s == sid) {
                Some(Arc::clone(&session_reg))
            } else {
                None
            }
        });
        let count: SessionCountFn = Arc::new(move || known.len());
        RegistryServer::new(server_registry(false))
            .with_session_resolver(resolver)
            .with_session_count(count)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_only_tool_executes_via_explicit_session_id() {
        let client = connect(extension_session_server(2)).await;
        let res = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("session_only_tool")
                    .with_arguments(json!({"session_id": "s0"}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_ne!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("session registry"), "{text}");
        client.cancel().await.ok();
    }

    /// Issue #227 (scan #74) regression: a BOGUS session_id must fail
    /// closed — never fall through to the server-level registry (which
    /// would execute a same-named server tool or leak the server jail,
    /// silently bypassing the #138 ambiguity refusal).
    #[tokio::test(flavor = "multi_thread")]
    async fn bogus_session_id_fails_closed_no_server_fallback() {
        let client = connect(extension_session_server(2)).await;
        // echo_tool EXISTS server-side: the old code would execute the
        // server copy on an unresolvable id. It must refuse instead.
        let res = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("echo_tool").with_arguments(
                    json!({"session_id": "bogus", "msg": "hi"})
                        .as_object()
                        .expect("object")
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("unknown session: bogus"), "{text}");
        assert!(!text.contains("echoed"), "{text}");
        client.cancel().await.ok();
    }

    /// And a session-only tool called WITHOUT session_id while 2+
    /// sessions are open gets the ambiguity refusal — not an opaque
    /// server-registry "unknown tool".
    #[tokio::test(flavor = "multi_thread")]
    async fn session_only_tool_without_id_with_two_sessions_is_refused() {
        let client = connect(extension_session_server(2)).await;
        let res = client
            .peer()
            .call_tool(CallToolRequestParams::new("session_only_tool"))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("ambiguous"), "{text}");
        client.cancel().await.ok();
    }

    // -----------------------------------------------------------------
    // Gate characterization (issue #303, PR 1 of 3). These pin what
    // `execute_checked` does TODAY so the gate refactor can prove it
    // changed nothing. They are not requirements.
    // -----------------------------------------------------------------

    use crate::approval::{Approver, ToolRequest, Verdict};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Approver that counts how often it is consulted.
    struct CountingApprover {
        verdict: Verdict,
        calls: Arc<AtomicUsize>,
    }
    impl Approver for CountingApprover {
        fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict
        }
        fn interactive(&self) -> bool {
            true // lets a Destructive tool register (permissive embedder)
        }
    }

    /// Tool that counts executions, with a configurable risk.
    struct ProbeTool {
        name: &'static str,
        risk: Risk,
        runs: Arc<AtomicUsize>,
    }
    impl crate::tool::Tool for ProbeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn risk(&self) -> Risk {
            self.risk
        }
        fn execute(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "probe_ran": input }))
        }
    }

    fn probe_server(
        verdict: Verdict,
        tools: &[(&'static str, Risk)],
    ) -> (RegistryServer, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let consulted = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::with_approver(CountingApprover {
            verdict,
            calls: consulted.clone(),
        });
        for (name, risk) in tools {
            reg.register(Box::new(ProbeTool {
                name,
                risk: *risk,
                runs: runs.clone(),
            }))
            .unwrap();
        }
        (RegistryServer::new(reg), consulted, runs)
    }

    #[test]
    fn gate_char_mcp_unknown_tool_message() {
        let (srv, consulted, _) = probe_server(Verdict::Allow, &[]);
        let err = srv.execute_checked("nope", json!({})).unwrap_err();
        assert_eq!(err, "unknown tool: nope");
        assert_eq!(consulted.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn gate_char_mcp_destructive_refused_even_if_approver_allows() {
        let (srv, consulted, runs) = probe_server(Verdict::Allow, &[("boom", Risk::Destructive)]);
        let err = srv.execute_checked("boom", json!({})).unwrap_err();
        assert_eq!(err, "destructive tools are never exposed in server mode");
        // Structural refusal: the approver is never even asked.
        assert_eq!(consulted.load(Ordering::SeqCst), 0);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn gate_char_mcp_write_denied_exact_message_and_no_execution() {
        let (srv, consulted, runs) = probe_server(Verdict::Deny, &[("w", Risk::Write)]);
        let err = srv.execute_checked("w", json!({})).unwrap_err();
        assert_eq!(
            err,
            "denied by approval policy — this MCP server only pre-authorizes Write tools listed in --allow patterns"
        );
        assert_eq!(consulted.load(Ordering::SeqCst), 1);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn gate_char_mcp_write_allowed_executes_after_one_consultation() {
        let (srv, consulted, runs) = probe_server(Verdict::Allow, &[("w", Risk::Write)]);
        let out = srv.execute_checked("w", json!({"k": 1})).unwrap();
        assert_eq!(out, json!({"probe_ran": {"k": 1}}));
        assert_eq!(consulted.load(Ordering::SeqCst), 1);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gate_char_mcp_readonly_executes_without_consulting_approver() {
        let (srv, consulted, runs) = probe_server(Verdict::Deny, &[("r", Risk::ReadOnly)]);
        let out = srv.execute_checked("r", json!({"k": 2})).unwrap();
        assert_eq!(out, json!({"probe_ran": {"k": 2}}));
        assert_eq!(consulted.load(Ordering::SeqCst), 0);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// INTENTIONAL behavior change vs the PR 1 characterization (#303 part
    /// 3 of 3): `execute_checked` now authorizes through the gate, so a
    /// pre-hook configured on the registry IS enforced. A configured
    /// deny-hook that one path silently ignores is a bypass. No in-repo
    /// server face can attach pre-hooks (the CLI refuses --on-pretool for
    /// mcp/serve/acp; see docs/orchestration.md), so only embedders that
    /// pass a hook-carrying registry to the public `RegistryServer::new`
    /// are affected, in the stricter direction.
    #[test]
    fn gate_char_mcp_pre_hooks_are_enforced_via_the_gate() {
        let dir = std::env::temp_dir().join(format!("tole-gate-char-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("hook-ran");
        let _ = std::fs::remove_file(&marker);
        let script = dir.join("deny.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\necho nope\nexit 2\n", marker.display()),
        )
        .unwrap();
        let consulted = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::with_approver(CountingApprover {
            verdict: Verdict::Allow,
            calls: consulted.clone(),
        });
        for (name, risk) in [("w", Risk::Write), ("r", Risk::ReadOnly)] {
            reg.register(Box::new(ProbeTool {
                name,
                risk,
                runs: runs.clone(),
            }))
            .unwrap();
        }
        let mut hooks = crate::hooks::ToolHooks::from_cli(&[], &[]);
        hooks.pre = vec![crate::hooks::ProcessHook::new(&format!(
            "/bin/sh {}",
            script.display()
        ))];
        reg.set_hooks(hooks);
        let srv = RegistryServer::new(reg);
        // Write: approver allows, the denying pre-hook refuses; the tool
        // never runs and the message is the standard policy denial.
        assert_eq!(
            srv.execute_checked("w", json!({})),
            Err(
                "denied by approval policy — this MCP server only pre-authorizes \
                 Write tools listed in --allow patterns"
                    .to_string()
            )
        );
        assert_eq!(consulted.load(Ordering::SeqCst), 1);
        assert!(marker.exists(), "pre-hook must run for a Write call");
        // ReadOnly is never gated, so it neither consults hooks nor is denied.
        assert!(srv.execute_checked("r", json!({})).is_ok());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Server-face registries (`tole_cli::session_host::open_session`)
    /// wire ONLY turn-end hooks, built from `from_cli(&[], &[])` — never
    /// pre-hooks — and the CLI refuses `--on-pretool` for mcp/serve/acp.
    /// Since the gate consults pre-hooks, pin that exactly that wiring
    /// leaves the authorization outcome unchanged and runs no hook here.
    #[test]
    fn server_face_hook_wiring_has_no_pre_hooks_and_does_not_affect_gate() {
        let dir = std::env::temp_dir().join(format!("tole-gate-mcp-te-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("turnend-ran");
        let _ = std::fs::remove_file(&marker);
        let mut hooks = crate::hooks::ToolHooks::from_cli(&[], &[]);
        assert!(
            hooks.pre.is_empty(),
            "server wiring must carry no pre-hooks"
        );
        hooks.turnend = vec![crate::hooks::turnend_hook(&format!(
            "/bin/sh -c 'touch {}; exit 2'",
            marker.display()
        ))];
        let consulted = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::with_approver(CountingApprover {
            verdict: Verdict::Allow,
            calls: consulted.clone(),
        });
        reg.register(Box::new(ProbeTool {
            name: "w",
            risk: Risk::Write,
            runs: runs.clone(),
        }))
        .unwrap();
        reg.set_hooks(hooks);
        let srv = RegistryServer::new(reg);
        assert!(srv.execute_checked("w", json!({})).is_ok());
        assert_eq!(consulted.load(Ordering::SeqCst), 1);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(!marker.exists(), "turn-end hook must not run on this path");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
