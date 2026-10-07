//! Remote approval escalation (issue #200): when a serve-face session
//! hits a Write approval, the pending decision becomes a queue entry; a
//! remote client (CLI, later uteke-mobile) lists/approves/denies.
//!
//! Semantics ride the EXISTING machinery — nothing new is invented:
//! - the queue approver denies the call (fail-closed, #84 semantics) →
//!   the turn settles as `ApprovalRequired`, resumable;
//! - an ALLOW decision stores a one-shot fingerprint (tool + canonical
//!   input, `call_fingerprint`) and triggers an approvals-only resume
//!   (`resume_turn`) — the replayed guarded intent re-consults the
//!   approver, the fingerprint matches once, the tool executes;
//! - a DENY decision just records the verdict — the session stays
//!   settled exactly as an interactive denial;
//! - entries expire to denied ([`EXPIRY_SECS`], pruned lazily on every
//!   list/decide) so missions never hang silently;
//! - every decision lands a durable `fact/approval_*` register on the
//!   session (the audit trail) — written by the serve route, not here.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tole_core::approval::{AllowlistApprover, Approver, ToolRequest, Verdict};
use tole_core::tool::Risk;
use tole_core::turn::call_fingerprint;

/// Pending decisions expire to denied after this long (lazily pruned).
pub const EXPIRY_SECS: u64 = 900;

/// One queued approval request.
#[derive(Clone, Debug)]
pub struct PendingApproval {
    pub id: String,
    pub session_id: String,
    pub tool: String,
    pub description: String,
    pub fingerprint: u64,
    pub created_at_ms: u64,
    pub status: ApprovalStatus,
    /// The session's durable file, recorded at queue time — the audit
    /// fallback for an evicted session must reopen the EXACT file
    /// (cora MAJOR #3: cwd-dependent layouts defeat daemon-cwd guesses).
    pub storage_path: std::path::PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
        }
    }
}

#[derive(Default)]
pub struct ApprovalQueue {
    entries: Mutex<Vec<PendingApproval>>,
    /// Approved one-shots keyed by (SESSION, fingerprint) — a shared
    /// tool+input across sessions must never let one session's ALLOW
    /// authorize another's replay (cora MAJOR). Value: tool + granted
    /// time for the stale sweep.
    one_shot: Mutex<HashMap<(String, u64), (String, u64)>>,
}

#[derive(Debug)]
pub enum DecisionOutcome {
    Decided {
        status: ApprovalStatus,
        session_id: String,
        tool: String,
    },
    Unknown,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ApprovalQueue {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Prune expired entries (pending → expired) and stale one-shots.
    fn prune_expired(&self) -> Vec<String> {
        let now = now_ms();
        let mut expired = Vec::new();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        for e in entries.iter_mut() {
            if e.status == ApprovalStatus::Pending
                && now.saturating_sub(e.created_at_ms) > EXPIRY_SECS * 1000
            {
                e.status = ApprovalStatus::Expired;
                expired.push(e.id.clone());
            }
        }
        // Bounding (cora MAJOR): terminal entries past the visibility
        // budget leave the Vec entirely — a long-running daemon must not
        // grow unbounded, and every GET /approvals must stay small.
        entries.retain(|e| {
            !(e.status != ApprovalStatus::Pending
                && now.saturating_sub(e.created_at_ms) > EXPIRY_SECS * 1000)
        });
        drop(entries);
        // Stale one-shots (a resumed turn that never replayed) die with
        // the same budget — no forever-granted authorization.
        self.one_shot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, (_, granted)| now.saturating_sub(*granted) <= EXPIRY_SECS * 1000);
        expired
    }

    /// Register a pending decision for a denied Write call. Returns the
    /// entry id.
    pub fn queue_entry(
        &self,
        session_id: &str,
        tool: &str,
        description: &str,
        fingerprint: u64,
        storage_path: std::path::PathBuf,
    ) -> String {
        self.prune_expired();
        let id = format!("apr-{:x}-{:x}", now_ms(), fingerprint as u32);
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(PendingApproval {
                id: id.clone(),
                session_id: session_id.to_string(),
                tool: tool.to_string(),
                description: description.to_string(),
                fingerprint,
                created_at_ms: now_ms(),
                status: ApprovalStatus::Pending,
                storage_path,
            });
        id
    }

    /// The recorded durable file of a session's latest queued entry
    /// (audit fallback for evicted sessions).
    pub fn storage_path(&self, session_id: &str) -> Option<std::path::PathBuf> {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .rev()
            .find(|e| e.session_id == session_id)
            .map(|e| e.storage_path.clone())
    }

    /// Validate an id WITHOUT mutating: (session_id, tool) when the
    /// entry is Pending. The decision route audits BEFORE deciding, so
    /// a failed audit leaves the queue untouched.
    pub fn peek_pending(&self, id: &str) -> Option<(String, String)> {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|e| e.id == id && e.status == ApprovalStatus::Pending)
            .map(|e| (e.session_id.clone(), e.tool.clone()))
    }

    /// Apply an operator decision.
    pub fn decide(&self, id: &str, allow: bool) -> DecisionOutcome {
        self.prune_expired();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
            return DecisionOutcome::Unknown;
        };
        if e.status != ApprovalStatus::Pending {
            return DecisionOutcome::Decided {
                status: e.status,
                session_id: e.session_id.clone(),
                tool: e.tool.clone(),
            };
        }
        if allow {
            e.status = ApprovalStatus::Approved;
            self.one_shot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(
                    (e.session_id.clone(), e.fingerprint),
                    (e.tool.clone(), now_ms()),
                );
        } else {
            e.status = ApprovalStatus::Denied;
        }
        DecisionOutcome::Decided {
            status: e.status,
            session_id: e.session_id.clone(),
            tool: e.tool.clone(),
        }
    }

    /// List pending-or-recently-decided entries, optionally filtered by
    /// session. Expired entries are marked (lazily) before listing.
    pub fn list(&self, session: Option<&str>) -> Vec<PendingApproval> {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|e| session.map(|s| e.session_id == s).unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Consume a one-shot approval for a replayed call (exactly once,
    /// and only for the SAME session that queued it).
    fn take_one_shot(&self, session_id: &str, fingerprint: u64) -> bool {
        self.one_shot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(session_id.to_string(), fingerprint))
            .is_some()
    }
}

/// The serve-face approver (issue #200): pre-authorized patterns behave
/// exactly like `AllowlistApprover` (`--allow`, `--trust`); anything
/// else is queued and denied — the turn settles resumably, a remote
/// operator decides, and the ALLOW one-shot lets the replayed guarded
/// effect execute. Destructive stays structurally unreachable (the
/// registry skips Destructive tools for non-interactive approvers).
pub struct QueueApprover {
    allow_only: AllowlistApprover,
    queue: Arc<ApprovalQueue>,
    session_id: String,
    storage_path: std::path::PathBuf,
}

impl QueueApprover {
    pub fn new(
        allow_patterns: Vec<String>,
        queue: Arc<ApprovalQueue>,
        session_id: impl Into<String>,
        storage_path: std::path::PathBuf,
    ) -> Self {
        Self {
            allow_only: AllowlistApprover::allow_only(allow_patterns),
            queue,
            session_id: session_id.into(),
            storage_path,
        }
    }
}

impl Approver for QueueApprover {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        if self.allow_only.decide(req) == Verdict::Allow {
            return Verdict::Allow;
        }
        if req.risk == Risk::ReadOnly {
            return Verdict::Allow;
        }
        let fp = call_fingerprint(req.tool, req.input);
        if self.queue.take_one_shot(&self.session_id, fp) {
            return Verdict::Allow;
        }
        self.queue.queue_entry(
            &self.session_id,
            req.tool,
            &req.description,
            fp,
            self.storage_path.clone(),
        );
        // Fail closed (#84): the turn settles ApprovalRequired — the
        // remote decision resumes it.
        Verdict::Deny
    }

    fn interactive(&self) -> bool {
        false
    }
}

/// JSON view for the REST list.
pub fn approval_json(e: &PendingApproval) -> Value {
    json!({
        "id": e.id,
        "sessionId": e.session_id,
        "tool": e.tool,
        "description": e.description,
        "status": e.status.as_str(),
        "createdAtMs": e.created_at_ms,
        "expiresInSeconds": EXPIRY_SECS,
    })
}

/// Thin std-only HTTP helper for the CLI consumer (tole serve speaks
/// one-request-per-connection `Connection: close` — a raw TcpStream is
/// the dependency-free client).
fn http_call(
    url: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> anyhow::Result<(u16, Value)> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    // Issue #246 (rescan #35): the raw-TcpStream transport is plaintext
    // by design (loopback approval daemon, Connection: close). An
    // https:// URL can NEVER be served correctly over it — stripping
    // the scheme silently sent the Bearer token in cleartext. Loud
    // refusal; no downgrade.
    if url.starts_with("https://") {
        return Err(anyhow::anyhow!(
            "approval endpoint {url} uses https:// but the approval HTTP client is a plaintext \
             loopback-only transport — serve the approval daemon over http:// (the bearer token \
             would otherwise cross the wire unencrypted)"
        ));
    }
    // url → host:port (+ optional base path is not supported; the serve
    // face is root-mounted).
    let authority = url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let mut stream =
        TcpStream::connect(&authority).map_err(|e| anyhow::anyhow!("connect {authority}: {e}"))?;
    let req = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n\
             Authorization: Bearer {token}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n\
             Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        ),
    };
    stream
        .write_all(req.as_bytes())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let mut payload = String::new();
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            == 0
        {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            reader
                .read_to_string(&mut payload)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            break;
        }
    }
    let v = serde_json::from_str(payload.trim()).unwrap_or(Value::Null);
    Ok((code, v))
}

/// CLI entry: `tole approvals list|allow|deny`.
pub fn cli(action: &str, id: Option<&str>, url: &str, token: &str) -> anyhow::Result<()> {
    match action {
        "list" => {
            let (code, v) = http_call(url, token, "GET", "/approvals", None)?;
            if code != 200 {
                anyhow::bail!("serve returned {code}: {v}");
            }
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        allow_or_deny @ ("allow" | "deny") => {
            let Some(id) = id else {
                anyhow::bail!("`tole approvals {allow_or_deny}` needs an <id>");
            };
            let body = json!({"decision": allow_or_deny}).to_string();
            let (code, v) = http_call(
                url,
                token,
                "POST",
                &format!("/approvals/{id}/decision"),
                Some(&body),
            )?;
            if !(200..300).contains(&code) {
                anyhow::bail!("serve returned {code}: {v}");
            }
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        other => anyhow::bail!("unknown action {other:?} — use list | allow | deny"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #246 regression: an https:// approval URL is loudly
    /// refused — never silently downgraded to plaintext (the Bearer
    /// token would cross the wire unencrypted).
    #[test]
    fn https_approval_url_is_refused() {
        let err = http_call("https://127.0.0.1:9/approve", "tok", "GET", "/p", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("https://"), "{msg}");
        assert!(msg.contains("plaintext"), "{msg}");
    }
}
