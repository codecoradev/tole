//! #1314: every MCP tool error leaves the server redacted with the process
//! secret set: a tool that fails with a text that holds a secret answers with
//! the marker, over a real (in-memory) MCP connection.

use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::serve_client;
use rmcp::ServiceExt;
use serde_json::json;
use tole_core::approval::AllowlistApprover;
use tole_core::mcp_server::RegistryServer;
use tole_core::tool::{Risk, Tool, ToolRegistry};

const LEAKED: &str = "sk-leak-0123456789abcdefXYZ";

struct FailTool;
impl Tool for FailTool {
    fn name(&self) -> &str {
        "fail_tool"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn describe(&self, _input: &serde_json::Value) -> String {
        "always fails".into()
    }
    fn execute(&self, _input: serde_json::Value) -> Result<serde_json::Value, String> {
        Err(format!("cannot open /data/{LEAKED}: denied"))
    }
}

#[test]
fn a_tool_error_is_redacted_at_the_mcp_sink() {
    tole_core::secrets::add_role_secret("hooks.test", LEAKED);
    let mut reg = ToolRegistry::with_approver(AllowlistApprover::allow_only(vec![]));
    reg.register(Box::new(FailTool)).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let text = rt.block_on(async {
        let (client_out, server_in) = tokio::io::duplex(8192);
        let (server_out, client_in) = tokio::io::duplex(8192);
        let server = RegistryServer::new(reg);
        tokio::spawn(async move {
            if let Ok(running) = server.serve((server_in, server_out)).await {
                let _ = running.waiting().await;
            }
        });
        let client = serve_client((), (client_in, client_out)).await.unwrap();
        let args = json!({}).as_object().unwrap().clone();
        let res = client
            .peer()
            .call_tool(CallToolRequestParams::new("fail_tool").with_arguments(args))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = match &res.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        client.cancel().await.ok();
        text
    });
    assert!(text.contains("denied"), "{text}");
    assert!(!text.contains(LEAKED), "the secret left: {text}");
}
