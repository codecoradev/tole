//! `tole serve` and the delegation approval queue (#714 part 4, design
//! `delegation-approvals-design.md` section 5.2).
//!
//! `GET /approvals` lists the pending requests of delegated children next to
//! serve's own (`source: "delegation"`), and `POST /approvals/{id}/decision`
//! routes `dap-*` ids here. This is the DURABLE queue of delegated children
//! (`<data dir>/delegation/<hash>/approvals.jsonl`); serve's in-memory
//! `ApprovalQueue` (deny-then-resume for serve-hosted sessions) is a separate
//! thing and is not touched. No session is resumed: the child is already
//! waiting.
//!
//! The rules are the CLI's (`approvals_owner`), because it is the same code:
//! the bearer token is approval authority (#200) and the route refuses when
//! serve itself runs under an agent session; only a pending request is
//! decided; the decision is bound to the request's fingerprint and recorded
//! `via: serve`; an `allow` must carry the request id again (`confirm`), and
//! no id means "all" (a pattern, a prefix or a missing id is refused); what is
//! shown is redacted, capped and fenced. The owner can answer always-owner
//! requests here. The scan is bounded (at most [`MAX_LOGS`] goals' logs).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tole_core::delegation_approvals::{Choice, Status, Via};
use tole_core::secrets::SecretSet;

use crate::approvals_owner::{decide, requests, show, valid_id, Confirm, Ctx};
use crate::audit::line;
use crate::delegated_approver::{approvals_path, Store};

/// Most delegation logs scanned per request.
pub const MAX_LOGS: usize = 64;
const LINE_CHARS: usize = 300;

/// The delegation logs under `data` that have an approvals log, sorted.
pub fn logs(data: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(data.join("delegation")) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .take(4 * MAX_LOGS)
        .map(|e| e.path().join(crate::delegate::LOG_FILE))
        .filter(|l| approvals_path(l).is_file())
        .collect();
    v.sort();
    v.truncate(MAX_LOGS);
    v
}

fn ctx<'a>(store: &'a Store, secrets: &'a SecretSet, now: i64) -> Ctx<'a> {
    Ctx {
        store,
        secrets,
        now,
        via: Via::Serve,
    }
}

/// The pending requests of every goal, for `GET /approvals`.
pub fn list(data: &Path, secrets: &SecretSet, now: i64) -> Vec<Value> {
    let mut out = Vec::new();
    for log in logs(data) {
        let store = Store::new(log);
        let c = ctx(&store, secrets, now);
        for (e, st) in requests(&c).unwrap_or_default() {
            if !matches!(st, Status::Pending { .. }) {
                continue;
            }
            let r = &e.req;
            let chain: Vec<Value> = r
                .chain
                .iter()
                .enumerate()
                .map(|(i, l)| json!({"level": i + 1, "node": l.node, "depth": l.depth, "skipped": l.skipped}))
                .collect();
            out.push(json!({
                "id": r.req,
                "source": "delegation",
                "status": "pending",
                "goal": line(secrets, &r.goal, LINE_CHARS),
                "node": line(secrets, &r.node, LINE_CHARS),
                "tool": line(secrets, &r.tool, LINE_CHARS),
                "target": r.target.as_deref().map(|t| line(secrets, t, LINE_CHARS)),
                "display": line(secrets, &r.display, LINE_CHARS),
                "ageSeconds": (now - r.at).max(0),
                "expiresInSeconds": (r.expires_at - now).max(0),
                "fingerprint": r.fingerprint,
                "chain": chain,
                // The whole request, fenced as data, as `tole delegation
                // approvals show` prints it.
                "text": show(&c, &r.req).unwrap_or_default(),
            }));
        }
    }
    out
}

fn store_of(data: &Path, id: &str) -> Option<Store> {
    logs(data).into_iter().map(Store::new).find(|s| {
        s.read(|_, a| a.state().entry(id).is_some())
            .unwrap_or(false)
    })
}

fn err(code: u16, msg: impl Into<String>) -> (u16, Value) {
    (code, json!({"error": msg.into()}))
}

/// `POST /approvals/{id}/decision` for a `dap-*` id.
pub fn decide_route(
    data: Option<&Path>,
    secrets: &SecretSet,
    now: i64,
    env: &dyn Fn(&str) -> Option<String>,
    id: &str,
    body: &str,
) -> (u16, Value) {
    // Before the body, the id or the log is looked at.
    if let Err(e) = crate::owner_only::check(env, "POST /approvals decision") {
        return err(403, e);
    }
    if !valid_id(id) {
        return err(400, "not a request id: give the full id of one request");
    }
    let Ok(Value::Object(req)) = serde_json::from_str::<Value>(body) else {
        return err(400, "the body must be a JSON object");
    };
    if let Some(k) = req
        .keys()
        .find(|k| !["decision", "confirm", "fingerprint"].contains(&k.as_str()))
    {
        return err(400, format!("unknown field `{k}`"));
    }
    let text = |k: &str| req.get(k).and_then(Value::as_str);
    let verdict = match text("decision") {
        Some("allow") => Choice::Allow,
        Some("deny") => Choice::Deny,
        _ => return err(400, "decision must be 'allow' or 'deny'"),
    };
    if verdict == Choice::Allow && text("confirm") != Some(id) {
        return err(400, "allow needs `confirm` set to the request id");
    }
    let Some(data) = data else {
        return err(503, "cannot resolve the tole data dir");
    };
    let Some(store) = store_of(data, id) else {
        return err(404, "unknown approval id");
    };
    let c = ctx(&store, secrets, now);
    match decide(&c, env, id, verdict, text("fingerprint"), Confirm::Yes) {
        Ok(_) => {
            let status = if verdict == Choice::Allow {
                "allowed"
            } else {
                "denied"
            };
            (
                200,
                json!({"id": id, "source": "delegation", "status": status}),
            )
        }
        Err(e) if e.contains("cannot be decided") => err(409, e),
        Err(e) if e.contains("agent session") => err(403, e),
        Err(e) if e.contains("--fingerprint") || e.contains("not a request id") => err(400, e),
        Err(e) => err(500, e),
    }
}

#[cfg(test)]
#[path = "serve_delegation_tests.rs"]
mod tests;
