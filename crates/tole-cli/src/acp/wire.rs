//! The JSON-RPC wire: the connection with its response routing, the reply helpers and
//! the content blocks of a `session/prompt`.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tole_cli::session::problem::egress_error;

/// Shared connection state: outgoing JSON-RPC lines are pushed to the
/// writer thread; responses to agent-initiated requests (permission) are
/// routed back to whoever is waiting on them.
#[derive(Clone)]
pub(super) struct Conn {
    tx: mpsc::Sender<String>,
    next_id: Arc<Mutex<u64>>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
}

impl Conn {
    pub(super) fn new(tx: mpsc::Sender<String>) -> Self {
        Self {
            tx,
            next_id: Arc::new(Mutex::new(1)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn send_line(&self, line: &str) {
        if self.tx.send(format!("{line}\n")).is_err() {
            // Client is gone; the pending wait below will time out and
            // the session loop winds down on EOF anyway.
        }
    }

    pub(super) fn send_notification(&self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        self.send_line(&line);
    }

    /// Send ONE agent-initiated request and hand back its reply
    /// receiver, so the caller can wait in slices and re-check the
    /// turn's cancellation token between slices (issue #178) — calling
    /// [`Conn::request`] repeatedly would re-send duplicate wire
    /// requests. Pair with [`Conn::abandon`] when giving up.
    pub(super) fn open_request(&self, method: &str, params: Value) -> (u64, mpsc::Receiver<Value>) {
        let id = {
            let mut n = self.next_id.lock().expect("id lock");
            let id = *n;
            *n += 1;
            id
        };
        let (tx, rx) = mpsc::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.send_line(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        (id, rx)
    }

    /// Drop a pending request entry (fail-closed path). A late client
    /// reply then finds no waiter and is ignored by `route`.
    pub(super) fn abandon(&self, id: u64) {
        self.pending.lock().expect("pending lock").remove(&id);
    }

    /// Reader-side routing: a response line completes a pending agent
    /// request. Client ERROR replies route too, and so does a reply with the
    /// matching id but NEITHER field (malformed client; cora scan 2026-10-01,
    /// #155): an errored permission must fail closed immediately, not hang
    /// for the full timeout (CodeCora scan 2026-09-28).
    pub(super) fn route_reply(&self, id: u64, msg: &Value) {
        if let Some(result) = msg.get("result").cloned() {
            self.route(id, result);
        } else if let Some(err) = msg.get("error").cloned() {
            self.route(
                id,
                json!({"outcome": {"outcome": "cancelled"}, "error": err}),
            );
        } else {
            // Malformed reply: fail closed now (cancelled outcome), the
            // requester surfaces the error instead of blocking.
            self.route(
                id,
                json!({"outcome": {"outcome": "cancelled"}, "error": "malformed reply"}),
            );
        }
    }

    fn route(&self, id: u64, result: Value) {
        if let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) {
            let _ = tx.send(result);
        }
    }
}

pub(super) fn reply(conn: &Conn, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    conn.send_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
}

pub(super) fn reply_error(conn: &Conn, id: Option<Value>, message: &str) {
    let Some(id) = id else { return };
    conn.send_line(
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": egress_error(message)}})
            .to_string(),
    );
}

/// Split a `session/prompt` `prompt` into the turn's text and images
/// (#559). ACP sends an array of content blocks; a plain string is
/// accepted for client convenience.
///
/// - `text` blocks are joined into the user message;
/// - `image` blocks go through `resolve`, the same spill-and-reference path
///   as a CLI `data:` URL: the declared MIME type is not trusted (the bytes
///   are sniffed), the `uri` is never read, and one bad image refuses the
///   whole prompt rather than being dropped;
/// - every other block is ignored, as `promptCapabilities` say.
///
/// An empty prompt (no text, no image) is refused as "missing prompt" (cora
/// scan 2026-10-01, #155 #4); an image-only prompt is a real prompt.
pub(super) fn prompt_content(
    prompt: Option<&Value>,
    resolve: &dyn Fn(&str) -> Result<tole_core::image::ImageRef, String>,
) -> Result<(String, Vec<tole_core::image::ImageRef>), String> {
    const MISSING: &str = "session/prompt: missing prompt";
    let (text, images) = match prompt {
        Some(Value::String(s)) => (s.clone(), Vec::new()),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("image") => {
                        let data = b
                            .get("data")
                            .and_then(Value::as_str)
                            .filter(|d| !d.trim().is_empty())
                            .ok_or("session/prompt: image block without base64 `data`")?;
                        let r = resolve(data).map_err(|e| format!("session/prompt: {e}"))?;
                        images.push(r);
                    }
                    Some("text") | None => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                        }
                    }
                    Some(_) => {}
                }
            }
            (text, images)
        }
        _ => return Err(MISSING.into()),
    };
    if text.trim().is_empty() && images.is_empty() {
        return Err(MISSING.into());
    }
    Ok((text, images))
}
