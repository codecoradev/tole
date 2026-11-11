//! Streamable-HTTP MCP client tests (#530). A tiny in-process HTTP server
//! speaks just enough JSON-RPC (initialize, tools/list, tools/call) so the
//! whole client path (config, handshake, token, eviction, redirects, error
//! scrubbing) runs against a real socket, with no network and no python.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tole_core::approval::AllowlistApprover;
use tole_core::mcp::{
    get_server_prompt, probe_server_extras, register_server_tools, McpServerConfig,
};
use tole_core::secrets::SecretSet;
use tole_core::tool::{Risk, Tool, ToolRegistry};

#[derive(Clone, Debug)]
struct Seen {
    headers: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct Behavior {
    /// Reply 401 (echoing the request headers in the body, like a sloppy
    /// server would) unless `Authorization` is exactly this.
    require_bearer: Option<String>,
    /// Answer every POST with a redirect to this url.
    redirect_to: Option<String>,
}

struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// While set, connections are dropped without a response.
    down: Arc<AtomicBool>,
}

impl Fake {
    fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn start(behavior: Behavior) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let down = Arc::new(AtomicBool::new(false));
    let (seen_t, down_t) = (seen.clone(), down.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (b, s, d) = (behavior.clone(), seen_t.clone(), down_t.clone());
            std::thread::spawn(move || handle(stream, &b, &s, &d));
        }
    });
    Fake { addr, seen, down }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, HashMap<String, String>, String)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let method = lines.next()?.split_whitespace().next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some((method, headers, String::from_utf8_lossy(&body).to_string()))
}

fn reply(stream: &mut TcpStream, status: &str, extra: &[(&str, &str)], body: &str) {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    let _ = stream.write_all(out.as_bytes());
}

fn rpc(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn handle(mut stream: TcpStream, behavior: &Behavior, seen: &Mutex<Vec<Seen>>, down: &AtomicBool) {
    let Some((method, headers, body)) = read_request(&mut stream) else {
        return;
    };
    if down.load(Ordering::SeqCst) {
        return; // drop the connection: a transport-level failure
    }
    seen.lock().unwrap().push(Seen {
        headers: headers.clone(),
    });
    if let Some(want) = &behavior.require_bearer {
        if headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {want}")) {
            let echo = format!("unauthorized, got headers {headers:?}");
            reply(&mut stream, "401 Unauthorized", &[], &echo);
            return;
        }
    }
    match method.as_str() {
        // No server-initiated stream: the spec lets a server refuse the GET.
        "GET" => reply(
            &mut stream,
            "405 Method Not Allowed",
            &[("Allow", "POST")],
            "",
        ),
        "DELETE" => reply(&mut stream, "200 OK", &[], ""),
        "POST" => {
            if let Some(to) = &behavior.redirect_to {
                reply(&mut stream, "302 Found", &[("Location", to)], "");
                return;
            }
            let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let id = req.get("id").cloned();
            let json_ct = [("Content-Type", "application/json")];
            match (req.get("method").and_then(Value::as_str), id) {
                (Some("initialize"), Some(id)) => {
                    let pv = req["params"]["protocolVersion"].clone();
                    let result = json!({
                        "protocolVersion": pv,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake", "version": "0"}
                    });
                    reply(
                        &mut stream,
                        "200 OK",
                        &[json_ct[0], ("Mcp-Session-Id", "sess-1")],
                        &rpc(&id, result),
                    );
                }
                (Some("tools/list"), Some(id)) => {
                    let result = json!({"tools": [{
                        "name": "echo",
                        "description": "echo the text back",
                        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
                    }]});
                    reply(&mut stream, "200 OK", &json_ct, &rpc(&id, result));
                }
                (Some("tools/call"), Some(id)) => {
                    let text = req["params"]["arguments"]["text"].as_str().unwrap_or("");
                    let result = json!({
                        "content": [{"type": "text", "text": format!("echo:{text}")}],
                        "isError": false
                    });
                    reply(&mut stream, "200 OK", &json_ct, &rpc(&id, result));
                }
                (_, None) => reply(&mut stream, "202 Accepted", &[], ""),
                (_, Some(id)) => {
                    let err = json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}});
                    reply(&mut stream, "200 OK", &json_ct, &err.to_string());
                }
            }
        }
        _ => reply(&mut stream, "405 Method Not Allowed", &[], ""),
    }
}

// NOTE: every test must use its OWN server name (httpa, httpb, ...). The MCP
// reactor is one per process and keeps connections keyed by name, so two tests
// sharing a name would talk to each other's fake server.

/// Write tools (every MCP tool) only register behind an approval gate.
fn registry() -> ToolRegistry {
    ToolRegistry::with_approver(AllowlistApprover::allow_only(vec!["mcp_*".into()]))
}

/// Call the remote `echo` tool: the registry tool takes `{"arguments": {..}}`
/// and answers `{"server", "result"}`.
fn echo(tool: &dyn Tool, text: &str) -> Result<String, String> {
    let out = tool.execute(json!({"arguments": {"text": text}}))?;
    Ok(out["result"]
        .as_str()
        .unwrap_or_default()
        .trim_end()
        .to_string())
}

/// The tokens every test relies on, set exactly once before any test touches
/// the MCP reactor: its thread reads the environment, and changing the
/// environment while another thread reads it is a data race. Servers `httpb`
/// and `httpe` deliberately have no token.
fn env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("TOLE_MCP_HTTPA_TOKEN", "s3cr3t-token-a");
        std::env::set_var("TOLE_MCP_HTTPC_TOKEN", "wrong-token-value");
        std::env::set_var("TOLE_MCP_HTTPD_TOKEN", "redirect-token");
        std::env::remove_var("TOLE_MCP_HTTPB_TOKEN");
        std::env::remove_var("TOLE_MCP_HTTPE_TOKEN");
    });
}

fn cfg(name: &str, fake: &Fake) -> McpServerConfig {
    env();
    McpServerConfig::parse(&format!("{name}={}", fake.url())).unwrap()
}

#[test]
fn http_server_registers_tools_and_sends_the_env_token() {
    let fake = start(Behavior {
        require_bearer: Some("s3cr3t-token-a".into()),
        ..Default::default()
    });
    let mut reg = registry();
    let names = register_server_tools(&mut reg, &cfg("httpa", &fake), &SecretSet::default());
    assert_eq!(names, vec!["mcp_httpa_echo".to_string()]);

    let tool = reg.get("mcp_httpa_echo").unwrap();
    // Trust model unchanged: a remote server never gets a lower risk tier.
    assert_eq!(tool.risk(), Risk::Write);
    assert_eq!(echo(tool, "hi").unwrap(), "echo:hi");

    let seen = fake.seen();
    assert!(seen.len() >= 3, "handshake + list + call, got {seen:?}");
    for s in &seen {
        assert_eq!(
            s.headers.get("authorization").map(String::as_str),
            Some("Bearer s3cr3t-token-a"),
            "every request carries the token: {s:?}"
        );
    }
}

#[test]
fn http_server_without_a_token_sends_no_authorization() {
    let fake = start(Behavior::default());
    let mut reg = registry();
    let names = register_server_tools(&mut reg, &cfg("httpb", &fake), &SecretSet::default());
    assert_eq!(names, vec!["mcp_httpb_echo".to_string()]);
    assert!(!fake.seen().is_empty());
    assert!(fake
        .seen()
        .iter()
        .all(|s| !s.headers.contains_key("authorization")));
}

#[test]
fn rejected_connection_error_hides_token_url_and_host() {
    let fake = start(Behavior {
        require_bearer: Some("the-right-token".into()),
        ..Default::default()
    });
    let c = cfg("httpc", &fake);
    // The probe registers the config in the reactor (and degrades silently);
    // the next op surfaces the connection error.
    probe_server_extras(&c);
    let err = get_server_prompt("httpc", "p", Default::default()).unwrap_err();
    assert!(!err.is_empty());
    let port = fake.addr.port().to_string();
    for secret in [
        "wrong-token-value",
        &fake.url(),
        &format!("127.0.0.1:{port}"),
    ] {
        assert!(!err.contains(secret), "error leaks {secret:?}: {err}");
    }
}

#[test]
fn cross_origin_redirect_is_not_followed() {
    let target = start(Behavior::default());
    let origin = start(Behavior {
        redirect_to: Some(target.url()),
        ..Default::default()
    });
    let c = cfg("httpd", &origin);
    probe_server_extras(&c);
    assert!(
        get_server_prompt("httpd", "p", Default::default()).is_err(),
        "a redirected handshake must fail"
    );
    assert!(
        target.seen().is_empty(),
        "the redirect target must never be contacted (it would receive the token): {:?}",
        target.seen()
    );
}

#[test]
fn dropped_connection_evicts_and_the_next_call_reconnects() {
    let fake = start(Behavior::default());
    let mut reg = registry();
    register_server_tools(&mut reg, &cfg("httpe", &fake), &SecretSet::default());
    let tool = reg.get("mcp_httpe_echo").unwrap();
    assert_eq!(echo(tool, "one").unwrap(), "echo:one");

    fake.down.store(true, Ordering::SeqCst);
    let err = echo(tool, "two").unwrap_err();
    assert!(err.contains("mcp"), "transport failure is reported: {err}");

    fake.down.store(false, Ordering::SeqCst);
    assert_eq!(
        echo(tool, "three").unwrap(),
        "echo:three",
        "the evicted connection is re-established on the next call"
    );
}

#[test]
fn connection_error_hides_the_normalized_url_the_client_echoes() {
    env();
    // A closed port: the client's own error text names the url it tried,
    // lowercased (`localhost`) and without anything the operator typed in
    // upper case, so a plain string replace of the spec would miss it.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let c = McpServerConfig::parse(&format!(
        "httpf=http://LOCALHOST:{port}/Cap/Path?Key=Sekret"
    ))
    .unwrap();
    probe_server_extras(&c);
    let err = get_server_prompt("httpf", "p", Default::default()).unwrap_err();
    assert!(!err.is_empty());
    let lower = err.to_ascii_lowercase();
    for secret in ["sekret", "cap/path", "localhost"] {
        assert!(!lower.contains(secret), "error leaks {secret:?}: {err}");
    }
}

#[test]
fn an_unparseable_url_built_without_parse_is_an_error_and_the_reactor_survives() {
    env();
    // `McpTransport` is public, so a config can skip `parse`. The url is
    // validated again at connect time and must yield an error, never a panic
    // on the shared reactor thread (which every MCP server of the process uses).
    for (i, bad) in [
        "https://example.com:99999/mcp",
        "https://1.2.3.4.5/mcp",
        "https://ex%41mple.com/mcp",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("httpbad{i}");
        let c = McpServerConfig {
            name: name.clone(),
            transport: tole_core::mcp::McpTransport::Http {
                url: (*bad).to_string(),
            },
        };
        probe_server_extras(&c);
        let err = get_server_prompt(&name, "p", Default::default()).unwrap_err();
        assert!(
            !err.contains("handshake failed"),
            "{bad} must be refused before any connection: {err}"
        );
    }
    // The reactor still serves a healthy server afterwards.
    let fake = start(Behavior::default());
    let mut reg = registry();
    let names = register_server_tools(&mut reg, &cfg("httpg", &fake), &SecretSet::default());
    assert_eq!(names, vec!["mcp_httpg_echo".to_string()]);
}

/// #595: a known secret in the model's arguments never reaches the server;
/// the echo proves what the server received.
#[test]
fn known_secrets_are_redacted_from_tool_arguments() {
    let fake = start(Behavior::default());
    let mut reg = registry();
    let secrets = SecretSet::collect(
        [(
            "TOLE_TEST_ARG_TOKEN".to_string(),
            "mcp-arg-secret-12345".to_string(),
        )],
        std::iter::empty(),
    );
    register_server_tools(&mut reg, &cfg("httph", &fake), &secrets);
    let tool = reg.get("mcp_httph_echo").unwrap();
    assert_eq!(
        echo(tool, "key=mcp-arg-secret-12345").unwrap(),
        "echo:key=«TOLE_TEST_ARG_TOKEN»"
    );
}
