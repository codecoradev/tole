//! MCP client integration (issue #74): tole connects to external MCP
//! servers over **stdio** and registers their tools into the normal
//! ToolRegistry.
//!
//! Trust model (threat-model.md must be read alongside this):
//! - Every MCP tool registers as [`Risk::Write`]. Server-supplied
//!   metadata (including `ReadOnlyHint`) is NEVER trusted for the risk
//!   tier — the approval gate fires on every call.
//! - Tool results are untrusted content: they flow through the normal
//!   settle path and get the same wire fencing as native tools.
//! - Server env: the child is spawned with the SAME scrubbed env as
//!   every other subprocess (ENV-1), plus a minimal MCPPATH marker.
//! - A server that fails to start or to answer `tools/list` degrades
//!   to a startup warning (same pattern as the uteke probe) — never a
//!   phantom tool and never a hard error.
//!
//! Runtime: rmcp needs a tokio reactor. The harness is synchronous, so
//! the client session lives on a dedicated background tokio runtime
//! thread; native code talks to it over an mpsc channel with a
//! hard timeout (the same discipline as run_with_timeout).

use crate::subprocess::SUBPROCESS_TIMEOUT;
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Request into the background tokio reactor.
enum McpRequest {
    ListTools {
        config: McpServerConfig,
        resp: mpsc::Sender<Result<Vec<(String, String)>, String>>,
    },
    Call {
        server: String,
        tool: String,
        args: Value,
        resp: mpsc::Sender<Result<String, String>>,
    },
}

/// Handle to the single background runtime. Spawned lazily on first
/// MCP tool registration.
static RUNTIME: OnceLock<mpsc::Sender<McpRequest>> = OnceLock::new();

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Public config parsed from `--mcp-server <name>=<command...>` flags
/// (kept simple; a config file can layer on later).
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

impl McpServerConfig {
    /// Parse `name=prog arg1 arg2 ...`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (name, rest) = spec
            .split_once('=')
            .ok_or_else(|| format!("mcp server spec must be name=command args, got {spec:?}"))?;
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("invalid mcp server name {name:?}"));
        }
        let mut parts = rest.split_whitespace();
        let command = parts
            .next()
            .ok_or_else(|| format!("mcp server {name:?} has no command"))?
            .to_string();
        Ok(Self {
            name: name.to_string(),
            command,
            args: parts.map(str::to_string).collect(),
        })
    }
}

/// Spawn the background tokio reactor if not yet running. Returns the
/// request channel.
fn runtime() -> mpsc::Sender<McpRequest> {
    RUNTIME
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<McpRequest>();
            std::thread::Builder::new()
                .name("mcp-runtime".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(1)
                        .enable_all()
                        .build()
                        .expect("tokio runtime");
                    let mut conns: std::collections::HashMap<String, McpConnection> =
                        std::collections::HashMap::new();
                    let mut configs: std::collections::HashMap<String, McpServerConfig> =
                        std::collections::HashMap::new();
                    rt.block_on(async {
                        // Serialized request handling: MCP servers are
                        // single-stdio and tole is single-agent; no need
                        // for concurrency inside the reactor.
                        while let Ok(req) = rx.recv() {
                            // Every operation is bounded INSIDE the
                            // reactor (CodeCora: the sync side's
                            // recv_timeout abandons but cannot cancel a
                            // stuck future — one hung server must not
                            // starve the shared loop). On expiry the
                            // connection is dropped; the next call
                            // reconnects.
                            match req {
                                McpRequest::ListTools { config, resp } => {
                                    configs.insert(config.name.clone(), config.clone());
                                    let op = with_connection(&config, &mut conns, |c| {
                                        Box::pin(async move { c.list_tools().await })
                                    });
                                    let r = match tokio::time::timeout(SUBPROCESS_TIMEOUT, op).await {
                                        Ok(r) => r,
                                        Err(_) => {
                                            conns.remove(&config.name);
                                            Err(format!(
                                                "mcp {:?}: tools/list timed out; connection dropped",
                                                config.name
                                            ))
                                        }
                                    };
                                    let _ = resp.send(r);
                                }
                                McpRequest::Call {
                                    server,
                                    tool,
                                    args,
                                    resp,
                                } => {
                                    let Some(config) = configs.get(&server).cloned() else {
                                        let _ = resp.send(Err(format!(
                                            "mcp server {server:?} was never registered"
                                        )));
                                        continue;
                                    };
                                    let tool_owned = tool.clone();
                                    let op = with_connection(&config, &mut conns, move |c| {
                                        Box::pin(async move { c.call(&tool_owned, args).await })
                                    });
                                    let r = match tokio::time::timeout(CALL_TIMEOUT, op).await {
                                        Ok(r) => r,
                                        Err(_) => {
                                            conns.remove(&server);
                                            Err(format!(
                                                "mcp {server:?}: call {tool:?} timed out; connection dropped"
                                            ))
                                        }
                                    };
                                    // Evict on TRANSPORT-class failures only
                                    // (server crash, closed stdio). Tool-
                                    // reported and argument errors mean the
                                    // server is healthy: tearing it down on
                                    // every failed call would respawn on the
                                    // next call and drop server session state.
                                    if r.as_ref().is_err_and(|e| e.starts_with("mcp transport")) {
                                        conns.remove(&server);
                                    }
                                    let _ = resp.send(r);
                                }
                            }
                        }
                    });
                })
                .expect("spawn mcp runtime");
            tx
        })
        .clone()
}

/// Live connection state lives INSIDE the reactor thread (rmcp sessions
/// are !Send-safe to keep there), keyed by server name.
struct McpConnection {
    service: rmcp::service::RunningService<rmcp::service::RoleClient, rmcp::model::ClientInfo>,
}

impl McpConnection {
    /// List tool (name, description) pairs from the server.
    async fn list_tools(&mut self) -> Result<Vec<(String, String)>, String> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        Ok(tools
            .into_iter()
            .map(|t| {
                let description = t
                    .description
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                (t.name.to_string(), description)
            })
            .collect())
    }

    /// Call a tool by name; returns the concatenated text content.
    ///
    /// Error classes (the reactor evicts the connection ONLY on
    /// `transport:` — tool-reported and argument errors mean the server
    /// is healthy and must keep its session state):
    /// - `"mcp transport ..."` — connection/wire-level failure.
    /// - `"mcp tool reported error: ..."` — the SERVER says the call
    ///   failed (isError=true); the connection is fine.
    /// - `"mcp arguments ..."` — caller-side validation; no wire round
    ///   trip happened.
    async fn call(&mut self, tool: &str, args: Value) -> Result<String, String> {
        let Some(obj) = args.as_object() else {
            return Err("mcp arguments: arguments must be a JSON object".into());
        };
        let mut params = rmcp::model::CallToolRequestParams::new(tool.to_string());
        params.arguments = Some(obj.clone());
        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| format!("mcp transport: tool call failed: {e}"))?;
        // Untrusted server output: cap what reaches the transcript/log
        // (threat model resource-exhaustion row), with a marked suffix.
        const MAX_RESULT_CHARS: usize = 256 * 1024;
        let mut text = String::new();
        for block in &result.content {
            if let rmcp::model::ContentBlock::Text(t) = block {
                text.push_str(&t.text);
                text.push('\n');
            }
            if text.chars().count() > MAX_RESULT_CHARS {
                let cut: String = text.chars().take(MAX_RESULT_CHARS).collect();
                text = format!("{cut}\n…[truncated, server output exceeded {MAX_RESULT_CHARS} chars]");
                break;
            }
        }
        // MCP spec: servers report tool FAILURE via isError + textual
        // content. That must settle as an error here, not Ok — the
        // durable log's success/failure bookkeeping depends on it.
        if result.is_error == Some(true) {
            return Err(format!("mcp tool reported error: {}", text.trim()));
        }
        Ok(text)
    }
}

async fn start_server(
    cfg: &McpServerConfig,
    conns: &mut std::collections::HashMap<String, McpConnection>,
) -> Result<(), String> {
    if conns.contains_key(&cfg.name) {
        return Ok(());
    }
    let mut std_cmd = std::process::Command::new(&cfg.command);
    std_cmd.args(&cfg.args);
    // ENV-1: same secret scrubbing as every other spawn point.
    crate::subprocess::scrub_env_for_child(&mut std_cmd);
    let mut tokio_cmd = tokio::process::Command::from(std_cmd);
    // Evicted/hung connections must not leak orphaned server processes
    // (CodeCora): kill the child when the connection struct is dropped.
    tokio_cmd.kill_on_drop(true);
    let transport = rmcp::transport::child_process::TokioChildProcess::new(tokio_cmd)
        .map_err(|e| format!("mcp {:?}: failed to start server: {e}", cfg.name))?;
    let service = tokio::time::timeout(
        SUBPROCESS_TIMEOUT,
        rmcp::service::serve_client(rmcp::model::ClientInfo::default(), transport),
    )
    .await
    .map_err(|_| format!("mcp {:?}: handshake timed out", cfg.name))?
    .map_err(|e| format!("mcp {:?}: handshake failed: {e}", cfg.name))?;
    conns.insert(cfg.name.clone(), McpConnection { service });
    Ok(())
}

async fn with_connection<R>(
    cfg: &McpServerConfig,
    conns: &mut std::collections::HashMap<String, McpConnection>,
    f: impl FnOnce(
        &mut McpConnection,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<R, String>> + '_>>,
) -> Result<R, String> {
    start_server(cfg, conns).await?;
    let conn = conns
        .get_mut(&cfg.name)
        .ok_or_else(|| format!("mcp {:?} vanished", cfg.name))?;
    f(conn).await
}

// ---------------------------------------------------------------------------
// Registry-facing tool
// ---------------------------------------------------------------------------

/// An MCP-server-backed tool in the registry. Executes over the reactor.
pub struct McpTool {
    server: String,
    /// Registry-facing name (prefixed: `mcp_<server>_<tool>`).
    name: String,
    /// The SERVER-side tool name (what the wire call must use).
    server_tool: String,
    description: String,
}

impl McpTool {
    pub fn new(server: String, name: String, server_tool: String, description: String) -> Self {
        Self {
            server,
            name,
            server_tool,
            description,
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn risk(&self) -> Risk {
        // Trust model: NEVER trust server metadata with the risk tier.
        // Every external tool is at least Write → approval gate.
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        // The approver approves THIS string: surface the model-controlled
        // arguments. Truncation is always MARKED — an approver must know
        // they are seeing a partial view (a model can pad innocuous
        // content before a harmful tail). Server-supplied description is
        // capped with the same marker discipline.
        fn truncate_marked(s: &str, max: usize) -> String {
            let chars: Vec<char> = s.chars().collect();
            if chars.len() <= max {
                s.to_string()
            } else {
                format!(
                    "{}…[truncated, +{} chars]",
                    chars[..max].iter().collect::<String>(),
                    chars.len() - max
                )
            }
        }
        let description = truncate_marked(&self.description, 200);
        let args = truncate_marked(
            &input
                .get("arguments")
                .map(|a| a.to_string())
                .unwrap_or_default(),
            200,
        );
        format!("mcp[{}] {} args={}", self.server, description, args)
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "arguments": {
                    "type": "object",
                    "description": format!("Arguments for mcp[{}]/{} per its JSON schema", self.server, self.name)
                }
            },
            "required": ["arguments"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let args = input.get("arguments").cloned().unwrap_or(json!({}));
        let (tx, rx) = mpsc::channel();
        runtime()
            .send(McpRequest::Call {
                server: self.server.clone(),
                tool: self.server_tool.clone(),
                args,
                resp: tx,
            })
            .map_err(|_| "mcp runtime is down".to_string())?;
        // SYNC_BUDGET must exceed the reactor's CALL_TIMEOUT (which also
        // covers a possible reconnect): the reactor's verdict must always
        // arrive before the sync side gives up — otherwise an abandoned
        // in-flight call could complete server-side after the turn
        // settled it as an error, and a documented-retry would
        // double-apply a non-idempotent effect (ReplaySafety hazard).
        const SYNC_BUDGET: Duration = Duration::from_secs(65); // CALL_TIMEOUT + 5s
        let result = rx
            .recv_timeout(SYNC_BUDGET)
            .map_err(|_| "mcp runtime dropped the request".to_string())??;
        Ok(json!({ "result": result, "server": self.server }))
    }
}

/// Register tools from `cfg` into `reg`. Startup-degrading: any failure
/// is a warning, never an error (same contract as the uteke probe).
/// Returns the names registered (empty on failure).
pub fn register_server_tools(
    reg: &mut crate::tool::ToolRegistry,
    cfg: &McpServerConfig,
) -> Vec<String> {
    let mut registered = Vec::new();
    let start = Instant::now();
    // The reactor's ListTools handler connects on demand (startup is
    // expected fast; bounded by the subprocess ceiling).
    let runtime_tx = runtime();

    let (ltx, lrx) = mpsc::channel();
    runtime_tx
        .send(McpRequest::ListTools {
            config: cfg.clone(),
            resp: ltx,
        })
        .map_err(|_| ())
        .ok();
    match lrx.recv_timeout(SUBPROCESS_TIMEOUT) {
        Ok(Ok(tools)) => {
            // Cap registered tools per server: a hostile/buggy server
            // must not flood the registry (and provider tool specs).
            const MAX_TOOLS_PER_SERVER: usize = 64;
            for (server_tool, description) in tools.into_iter().take(MAX_TOOLS_PER_SERVER) {
                let mcp_name = format!("mcp_{}_{}", cfg.name, server_tool);
                if reg
                    .register(Box::new(McpTool::new(
                        cfg.name.clone(),
                        mcp_name.clone(),
                        server_tool,
                        description,
                    )))
                    .is_ok()
                {
                    registered.push(mcp_name);
                }
            }
            eprintln!(
                "tole: mcp[{}]: {} tool(s) registered in {:?}",
                cfg.name,
                registered.len(),
                start.elapsed()
            );
        }
        Ok(Err(e)) => eprintln!(
            "tole: mcp[{}]: listing failed: {e} — server skipped",
            cfg.name
        ),
        Err(_) => eprintln!(
            "tole: mcp[{}]: timed out listing tools — server skipped",
            cfg.name
        ),
    }
    registered
}
