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
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Error-class prefix: transport failures evict the connection, while
/// tool-reported/argument errors keep it (see the module docs). Producer
/// and matcher share this constant so they cannot drift (CodeCora scan
/// 2026-09-18: brittle string-prefix matching).
const TRANSPORT_ERR: &str = "mcp transport";

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
    /// Probe-first discovery of prompts/resources (#213).
    Probe {
        config: McpServerConfig,
        resp: mpsc::Sender<Result<McpExtras, String>>,
    },
    GetPrompt {
        server: String,
        name: String,
        args: std::collections::BTreeMap<String, String>,
        resp: mpsc::Sender<Result<String, String>>,
    },
    ReadResource {
        server: String,
        uri: String,
        resp: mpsc::Sender<Result<String, String>>,
    },
}

/// Handle to the single background runtime. Spawned lazily on first
/// MCP tool registration.
static RUNTIME: OnceLock<mpsc::Sender<McpRequest>> = OnceLock::new();

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Cap on untrusted server text reaching the transcript (threat model
/// resource-exhaustion row); shared by tool results, prompts, resources.
const MAX_RESULT_CHARS: usize = 256 * 1024;

/// A hostile server must not flood the chat listing with prompts.
const MAX_PROMPTS_PER_SERVER: usize = 64;

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
                                    if r.as_ref().is_err_and(|e| e.starts_with(TRANSPORT_ERR)) {
                                        conns.remove(&server);
                                    }
                                    let _ = resp.send(r);
                                }
                                McpRequest::Probe { config, resp } => {
                                    configs.insert(config.name.clone(), config.clone());
                                    let r = run_op(&config, &mut conns, SUBPROCESS_TIMEOUT, |c| {
                                        Box::pin(async move { c.probe().await })
                                    })
                                    .await;
                                    let _ = resp.send(r);
                                }
                                McpRequest::GetPrompt {
                                    server,
                                    name,
                                    args,
                                    resp,
                                } => {
                                    let r = match configs.get(&server).cloned() {
                                        Some(config) => {
                                            run_op(&config, &mut conns, CALL_TIMEOUT, move |c| {
                                                Box::pin(async move { c.get_prompt(&name, args).await })
                                            })
                                            .await
                                        }
                                        None => Err(format!("mcp server {server:?} was never registered")),
                                    };
                                    let _ = resp.send(r);
                                }
                                McpRequest::ReadResource { server, uri, resp } => {
                                    let r = match configs.get(&server).cloned() {
                                        Some(config) => {
                                            run_op(&config, &mut conns, CALL_TIMEOUT, move |c| {
                                                Box::pin(async move { c.read_resource(&uri).await })
                                            })
                                            .await
                                        }
                                        None => Err(format!("mcp server {server:?} was never registered")),
                                    };
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
    service: rmcp::service::RunningService<rmcp::service::RoleClient, rmcp::model::ClientConfig>,
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

    /// Probe-first (#213): `prompts/list` is only sent when the server
    /// advertised the capability at the handshake, so a server without
    /// prompts/resources is never asked and contributes nothing.
    async fn probe(&mut self) -> Result<McpExtras, String> {
        let (has_prompts, has_resources) = self
            .service
            .peer_info()
            .map(|i| {
                (
                    i.capabilities.prompts.is_some(),
                    i.capabilities.resources.is_some(),
                )
            })
            .unwrap_or((false, false));
        let mut prompts = Vec::new();
        if has_prompts {
            let listed = self
                .service
                .list_all_prompts()
                .await
                .map_err(|e| format!("{TRANSPORT_ERR}: prompts/list failed: {e}"))?;
            for p in listed.into_iter().take(MAX_PROMPTS_PER_SERVER) {
                prompts.push(McpPromptInfo {
                    name: p.name,
                    description: p.description.unwrap_or_default(),
                    arguments: p
                        .arguments
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| a.name)
                        .collect(),
                });
            }
        }
        Ok(McpExtras {
            prompts,
            resources: has_resources,
        })
    }

    /// Render a prompt server-side; returns its text messages joined.
    async fn get_prompt(
        &mut self,
        name: &str,
        args: std::collections::BTreeMap<String, String>,
    ) -> Result<String, String> {
        let mut params = rmcp::model::GetPromptRequestParams::new(name.to_string());
        if !args.is_empty() {
            params.arguments = Some(args.into_iter().map(|(k, v)| (k, json!(v))).collect());
        }
        let result = self
            .service
            .get_prompt(params)
            .await
            .map_err(|e| format!("{TRANSPORT_ERR}: prompts/get failed: {e}"))?;
        let blocks: Vec<&str> = result
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        Ok(cap_text_blocks(&blocks, MAX_RESULT_CHARS)
            .trim_end()
            .to_string())
    }

    /// Read a resource by URI; text parts only (blobs are omitted).
    async fn read_resource(&mut self, uri: &str) -> Result<String, String> {
        let result = self
            .service
            .read_resource(rmcp::model::ReadResourceRequestParams::new(uri.to_string()))
            .await
            .map_err(|e| format!("{TRANSPORT_ERR}: resources/read failed: {e}"))?;
        let blocks: Vec<&str> = result
            .contents
            .iter()
            .filter_map(|c| match c {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        Ok(cap_text_blocks(&blocks, MAX_RESULT_CHARS)
            .trim_end()
            .to_string())
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
            .map_err(|e| format!("{TRANSPORT_ERR}: tool call failed: {e}"))?;
        // Untrusted server output: cap what reaches the transcript/log
        // (threat model resource-exhaustion row), with a marked suffix.
        // The cap applies INCREMENTALLY while appending (CodeCora scan
        // 2026-09-18): a single oversized text block must not be copied
        // into the buffer in full before any limit is checked.
        let blocks: Vec<&str> = result
            .content
            .iter()
            .filter_map(|b| match b {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        let text = cap_text_blocks(&blocks, MAX_RESULT_CHARS);
        // MCP spec: servers report tool FAILURE via isError + textual
        // content. That must settle as an error here, not Ok — the
        // durable log's success/failure bookkeeping depends on it.
        if result.is_error == Some(true) {
            return Err(format!("mcp tool reported error: {}", text.trim()));
        }
        Ok(text)
    }
}

/// Cap untrusted text blocks for the transcript: append up to `max`
/// chars total, with a marked suffix when anything was cut. Pure so the
/// truncation contract is testable without a live server (CodeCora scan
/// 2026-09-18 — the cap must apply while appending, not after a full
/// block copy).
fn cap_text_blocks(blocks: &[&str], max: usize) -> String {
    let mut text = String::new();
    let mut len = 0usize;
    let mut truncated = false;
    for block in blocks {
        if truncated {
            break;
        }
        for ch in block.chars() {
            if len >= max {
                truncated = true;
                break;
            }
            text.push(ch);
            len += 1;
        }
        if !truncated {
            // Issue #251 (rescan #130): the separator counts toward the
            // cap too — N empty blocks each added an uncounted byte, a
            // trivial transcript-cap bypass for a hostile server.
            if len >= max {
                truncated = true;
            } else {
                text.push('\n');
                len += 1;
            }
        }
    }
    if truncated {
        text.push_str(&format!(
            "\n…[truncated, server output exceeded {max} chars]"
        ));
    }
    text
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
        rmcp::service::serve_client(rmcp::model::ClientConfig::default(), transport),
    )
    .await
    .map_err(|_| format!("mcp {:?}: handshake timed out", cfg.name))?
    .map_err(|e| format!("mcp {:?}: handshake failed: {e}", cfg.name))?;
    conns.insert(cfg.name.clone(), McpConnection { service });
    Ok(())
}

/// One bounded reactor op: connect on demand, time-box, and drop the
/// connection on timeout or transport failure (same eviction contract as
/// `Call`).
async fn run_op<R>(
    cfg: &McpServerConfig,
    conns: &mut std::collections::HashMap<String, McpConnection>,
    budget: Duration,
    f: impl FnOnce(
        &mut McpConnection,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<R, String>> + '_>>,
) -> Result<R, String> {
    let r = match tokio::time::timeout(budget, with_connection(cfg, conns, f)).await {
        Ok(r) => r,
        Err(_) => Err(format!("{TRANSPORT_ERR}: mcp {:?}: timed out", cfg.name)),
    };
    if r.as_ref().is_err_and(|e| e.starts_with(TRANSPORT_ERR)) {
        conns.remove(&cfg.name);
    }
    r
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

    fn summary(&self) -> String {
        // Provider-facing text is deliberately unchanged from the
        // pre-`summary()` wire: the server-supplied description in the
        // approval-line format (wire stability for existing sessions).
        self.describe(&Value::Null)
    }

    fn describe(&self, input: &Value) -> String {
        // The approver approves THIS string: surface the model-controlled
        // arguments. Truncation is always MARKED — an approver must know
        // they are seeing a partial view (a model can pad innocuous
        // content before a harmful tail). Server-supplied description is
        // capped with the same marker discipline.
        fn truncate_marked(s: &str, max: usize) -> String {
            // Streaming: no full Vec<char> materialization of an
            // arbitrarily large argument payload (CodeCora scan
            // 2026-09-18).
            let mut out = String::new();
            let mut taken = 0usize;
            let mut total = 0usize;
            for ch in s.chars() {
                total += 1;
                if taken < max {
                    out.push(ch);
                    taken += 1;
                }
            }
            if total > max {
                out.push_str(&format!("…[truncated, +{} chars]", total - max));
            }
            out
        }
        let description = truncate_marked(&sanitize_one_line(&self.description), 200);
        let args = truncate_marked(
            &input
                .get("arguments")
                .map(|a| a.to_string())
                .unwrap_or_default(),
            200,
        );
        format!(
            "mcp[{}] {} args={}",
            sanitize_one_line(&self.server),
            description,
            args
        )
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

// ---------------------------------------------------------------------------
// Prompts and resources (#213)
// ---------------------------------------------------------------------------

/// A prompt template a server offers. Server-controlled text: same trust
/// tier as a tool description.
#[derive(Debug, Clone)]
pub struct McpPromptInfo {
    pub name: String,
    pub description: String,
    pub arguments: Vec<String>,
}

/// What a server offers beyond tools. Empty/false when the server did not
/// advertise the capability (probe-first: no phantom entries).
#[derive(Debug, Clone, Default)]
pub struct McpExtras {
    pub prompts: Vec<McpPromptInfo>,
    pub resources: bool,
}

/// Send a request to the reactor and wait for its verdict.
fn ask<R>(
    make: impl FnOnce(mpsc::Sender<Result<R, String>>) -> McpRequest,
    budget: Duration,
) -> Result<R, String> {
    let (tx, rx) = mpsc::channel();
    runtime()
        .send(make(tx))
        .map_err(|_| "mcp runtime is down".to_string())?;
    rx.recv_timeout(budget)
        .map_err(|_| "mcp runtime dropped the request".to_string())?
}

/// Probe `cfg` for prompts/resources. Failures are silent-empty: a server
/// that cannot answer was already warned about at tool registration.
pub fn probe_server_extras(cfg: &McpServerConfig) -> McpExtras {
    ask(
        |resp| McpRequest::Probe {
            config: cfg.clone(),
            resp,
        },
        LIST_SYNC_BUDGET,
    )
    .unwrap_or_default()
}

/// Render a server prompt (`prompts/get`) into plain text for the next
/// user message. Non-text content is dropped.
pub fn get_server_prompt(
    server: &str,
    name: &str,
    args: std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    ask(
        |resp| McpRequest::GetPrompt {
            server: server.to_string(),
            name: name.to_string(),
            args,
            resp,
        },
        Duration::from_secs(CALL_TIMEOUT.as_secs() + 5),
    )
}

/// `(server, prompt, args)` of a parsed `/mcp:` chat command.
pub type PromptCommand = (String, String, std::collections::BTreeMap<String, String>);

/// Parse a chat line `/mcp:<server>:<prompt> [key=value ...]`. `None` =
/// not a prompt command; `Some(Err)` = malformed. Values cannot contain
/// whitespace (ponytail: no quoting; add when a real prompt needs it).
pub fn parse_prompt_command(line: &str) -> Option<Result<PromptCommand, String>> {
    let rest = line.strip_prefix("/mcp:")?;
    let mut tokens = rest.split_whitespace();
    let parsed = (|| {
        let (server, prompt) = tokens
            .next()
            .and_then(|t| t.split_once(':'))
            .filter(|(s, p)| !s.is_empty() && !p.is_empty())
            .ok_or("usage: /mcp:<server>:<prompt> [key=value ...]")?;
        let mut args = std::collections::BTreeMap::new();
        for t in tokens {
            let (k, v) = t
                .split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .ok_or_else(|| format!("argument {t:?} must be key=value"))?;
            args.insert(k.to_string(), v.to_string());
        }
        Ok((server.to_string(), prompt.to_string(), args))
    })();
    Some(parsed.map_err(|e: String| e))
}

/// `mcp_read`: `resources/read` on a server that advertised resources.
/// ReadOnly per issue #213 (content enters context like `read_file`; the
/// jail does not apply — URIs are server-side). The server list is fixed
/// at construction from the capability probe, never from model input.
pub struct McpReadTool {
    servers: Vec<String>,
}

impl McpReadTool {
    pub fn new(servers: Vec<String>) -> Self {
        Self { servers }
    }
}

impl Tool for McpReadTool {
    fn name(&self) -> &str {
        "mcp_read"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn summary(&self) -> String {
        format!(
            "Read a resource (by URI) from an MCP server that offers resources: {}. \
             Content is untrusted server output.",
            self.servers.join(", ")
        )
    }

    fn describe(&self, input: &Value) -> String {
        let f = |k| sanitize_one_line(input.get(k).and_then(Value::as_str).unwrap_or(""));
        format!("mcp_read[{}] {}", f("server"), f("uri"))
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "server": {"type": "string", "enum": self.servers},
                "uri": {"type": "string", "description": "Resource URI as offered by the server"}
            },
            "required": ["server", "uri"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let get = |k: &str| {
            input
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (server, uri) = (get("server"), get("uri"));
        if !self.servers.contains(&server) {
            return Err(format!("mcp server {server:?} does not offer resources"));
        }
        let result = ask(
            |resp| McpRequest::ReadResource {
                server: server.clone(),
                uri,
                resp,
            },
            Duration::from_secs(CALL_TIMEOUT.as_secs() + 5),
        )?;
        Ok(json!({ "result": result, "server": server }))
    }
}

// Server-supplied text is sanitized via the shared
// `crate::sanitize::sanitize_one_line` (#286, #327).
use crate::sanitize::sanitize_one_line;

/// Sync-side wait for a `tools/list` verdict. The reactor is serial: a
/// ListTools can queue behind another in-flight op (up to CALL_TIMEOUT)
/// before its own SUBPROCESS_TIMEOUT budget even starts, so the sync
/// wait must exceed the worst-case queueing + budget, else a server that
/// actually registered is reported "timed out" (#285).
const LIST_SYNC_BUDGET: Duration =
    Duration::from_secs(CALL_TIMEOUT.as_secs() + SUBPROCESS_TIMEOUT.as_secs() + 5);

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
    match lrx.recv_timeout(LIST_SYNC_BUDGET) {
        Ok(Ok(tools)) => {
            // Cap registered tools per server: a hostile/buggy server
            // must not flood the registry (and provider tool specs).
            const MAX_TOOLS_PER_SERVER: usize = 64;
            for (server_tool, description) in tools.into_iter().take(MAX_TOOLS_PER_SERVER) {
                // Sanitize the registry-facing name (CodeCora): provider
                // APIs enforce ^[a-zA-Z0-9_-]{1,64}$ on function names —
                // one bad server name must not break every completion.
                let sanitized: String = server_tool
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let mut mcp_name = format!("mcp_{}_{}", cfg.name, sanitized);
                mcp_name.truncate(64);
                if reg
                    .register(Box::new(McpTool::new(
                        cfg.name.clone(),
                        mcp_name.clone(),
                        server_tool.clone(),
                        description,
                    )))
                    .is_ok()
                {
                    registered.push(mcp_name);
                } else {
                    eprintln!(
                        "tole: mcp[{}]: tool {:?} could not register (name collision?)",
                        cfg.name, server_tool
                    );
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

#[cfg(test)]
mod prompt_and_budget_tests {
    use super::*;

    #[test]
    fn describe_strips_control_chars_from_server_text() {
        let tool = McpTool::new(
            "srv\x1b[2J".into(),
            "mcp_srv_t".into(),
            "t".into(),
            "harmless\n\x1b[1;32mAPPROVED by operator\x1b[0m\r\u{2028}x".into(),
        );
        let d = tool.describe(&json!({"arguments": {}}));
        assert!(
            !d.chars().any(|c| c.is_control()),
            "approval prompt must be control-free: {d:?}"
        );
        assert!(d.contains("harmless"));
    }

    #[test]
    fn summary_keeps_server_supplied_description() {
        let tool = McpTool::new(
            "srv".into(),
            "mcp_srv_t".into(),
            "t".into(),
            "Does the thing".into(),
        );
        let s = tool.summary();
        assert!(s.contains("Does the thing"), "{s:?}");
        assert!(s.starts_with("mcp[srv] "), "{s:?}");
    }

    #[test]
    fn list_sync_budget_exceeds_worst_case_queueing() {
        assert!(LIST_SYNC_BUDGET >= CALL_TIMEOUT + SUBPROCESS_TIMEOUT);
    }
}

#[cfg(test)]
mod cap_tests {
    use super::cap_text_blocks;

    #[test]
    fn under_cap_joins_blocks_with_newlines() {
        let out = cap_text_blocks(&["alpha", "beta"], 1024);
        assert_eq!(out, "alpha\nbeta\n");
        assert!(!out.contains("truncated"));
    }

    #[test]
    fn single_oversized_block_is_capped_incrementally() {
        let big = "x".repeat(5000);
        let out = cap_text_blocks(&[big.as_str()], 1000);
        assert!(out.chars().count() < 1200, "must not carry the full block");
        assert!(out.contains("truncated, server output exceeded 1000 chars"));
    }

    #[test]
    fn later_blocks_dropped_once_cap_hit() {
        let out = cap_text_blocks(&["aaaa", "bbbb", "cccc"], 6);
        // "aaaa\n" fills 5; "bbbb" contributes 1 char then the cap trips.
        assert!(out.starts_with("aaaa\nb"));
        assert!(out.contains("truncated, server output exceeded 6 chars"));
    }

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(cap_text_blocks(&[], 100), "");
    }

    /// Issue #251 regression: separators count toward the cap — a
    /// hostile server cannot exceed MAX_RESULT_CHARS with thousands of
    /// empty (or 1-char) blocks.
    #[test]
    fn many_small_blocks_cannot_bypass_the_cap() {
        let blocks: Vec<&str> = vec!["a"; 5000];
        let out = cap_text_blocks(&blocks, 1000);
        let content_len =
            out.chars().count() - "…[truncated, server output exceeded 1000 chars]".len() - 1;
        assert!(
            content_len <= 1000 + 2,
            "total content must stay at the cap (+ separator/truncation slack): {content_len}"
        );
        assert!(out.contains("truncated"));
        // The all-empty-block variant: pure newline flood. At most ONE
        // separator byte slips past (the byte that reaches the cap was
        // pushed before the check) — bounded, versus unbounded before.
        let empty: Vec<&str> = vec![""; 5000];
        let out2 = cap_text_blocks(&empty, 1000);
        assert!(
            out2.contains("truncated"),
            "empty-block flood must trip the cap"
        );
        assert!(
            out2.chars().count() <= 1000 + 50,
            "got {}",
            out2.chars().count()
        );
    }
}
