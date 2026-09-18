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

/// An MCP server view of a [`ToolRegistry`].
pub struct RegistryServer {
    registry: ToolRegistry,
}

impl RegistryServer {
    /// Wrap a registry. In server mode the registry must be built with a
    /// NON-interactive approver (e.g.
    /// `AllowlistApprover::allow_only(patterns)`): interactive prompts
    /// would try to read the protocol's stdin. Destructive tools are then
    /// refused at registration and structurally absent from the server.
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
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
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| format!("unknown tool: {name}"))?;
        match tool.risk() {
            Risk::ReadOnly => {}
            Risk::Write | Risk::Destructive => match self.registry.decide(name, &args) {
                Some(crate::approval::Verdict::Allow) => {}
                _ => {
                    return Err(
                        "denied by approval policy — this MCP server only pre-authorizes \
                         Write tools listed in --allow patterns (Destructive tools are \
                         never exposed in server mode)"
                            .into(),
                    )
                }
            },
        }
        tool.execute(args)
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
        match self.execute_checked(&name, args) {
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
}
