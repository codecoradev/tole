//! `tole delegation approvals list|show|allow|deny` (#714 part 3, design
//! `delegation-approvals-design.md` section 5.2): the owner reads and answers
//! the approval requests of delegated children.
//!
//! - `list` and `show` read the approvals log, never write. Everything they
//!   print is data: redacted through the process `SecretSet`, control
//!   characters dropped, capped, and the model-influenced text (display,
//!   input) fenced.
//! - `allow` and `deny` append a `Decide` record at the owner level, bound to
//!   the request's fingerprint, under the lock. They refuse under an agent
//!   session (#684, #700) before anything is read or written.
//! - A mistaken `allow` is hard: the full request id is required (no prefix,
//!   no pattern, no "all"), the request is shown and the id must be typed
//!   back, or `--yes` skips only that typing. `--fingerprint` pins the
//!   content. `deny` needs no confirmation: no answer is a denial anyway.
//! - Only a pending request can be decided; an unknown, expired, consumed,
//!   already decided or settled one is refused with its reason. The child's
//!   one-shot `Consume` then decides whether the call runs.
//!
//! The owner can answer any request here, including the always-owner classes
//! an agent level can never decide.

use tole_core::delegation_approvals::{
    By, Choice, Entry, ExpireReason, Origin, Record, Skip, Status, Via,
};
use tole_core::secrets::SecretSet;

use crate::audit::line;
use crate::delegated_approver::Store;

/// Longest single-line field shown, and the input preview.
const LINE_CHARS: usize = 300;
const BLOCK_CHARS: usize = 2048;

/// What a command needs: the store, the redactor and the clock.
pub struct Ctx<'a> {
    pub store: &'a Store,
    pub secrets: &'a SecretSet,
    pub now: i64,
    /// Which owner surface answers: the command or `tole serve`.
    pub via: Via,
}

/// A request id as `tole` writes them: `dap-` and lowercase alphanumerics
/// and dashes. Nothing else (a prefix, a glob, `all`) names a request.
pub fn valid_id(id: &str) -> bool {
    id.len() > 4
        && id.len() <= 64
        && id.starts_with("dap-")
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn cap(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Data for the terminal: redacted first (so a secret is matched whole),
/// control characters other than newline dropped, capped.
pub(crate) fn block(secrets: &SecretSet, s: &str, max: usize) -> String {
    let redacted = secrets.redact(s);
    let clean: String = redacted
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    cap(&clean, max)
}

/// `text` between fences longer than any run of `~` it holds, so it cannot
/// close its own fence.
pub(crate) fn fenced(label: &str, text: &str) -> String {
    let mut fence = "~~~".to_string();
    while text.contains(&fence) {
        fence.push('~');
    }
    format!("{label}:\n{fence}\n{text}\n{fence}\n")
}

fn status_text(s: Status, e: &Entry) -> String {
    match s {
        Status::Pending { level: Some(i) } => {
            format!(
                "pending (level {} {} is asked first)",
                i + 1,
                e.req.chain[i].node
            )
        }
        Status::Pending { level: None } => "pending (only you can answer)".into(),
        Status::Allowed => "allowed, waiting for the child to consume it".into(),
        Status::Denied => "denied".into(),
        Status::Consumed => "allowed and consumed (the call ran)".into(),
        Status::Expired(ExpireReason::Timeout) => "expired: no answer is a denial".into(),
        Status::Expired(_) => "expired: the asking node has settled".into(),
    }
}

fn is_open(s: Status) -> bool {
    matches!(s, Status::Pending { .. })
}

fn target(secrets: &SecretSet, e: &Entry) -> String {
    e.req
        .target
        .as_deref()
        .map_or("-".into(), |t| line(secrets, t, LINE_CHARS))
}

pub(crate) fn requests(ctx: &Ctx<'_>) -> Result<Vec<(Entry, Status)>, String> {
    ctx.store.read(|d, a| {
        let s = a.state();
        s.entries()
            .filter_map(|e| Some((e.clone(), s.status(&e.req.req, d, ctx.now)?)))
            .collect()
    })
}

/// One line per request; only the open ones unless `all`.
pub fn list(ctx: &Ctx<'_>, all: bool) -> Result<String, String> {
    let mut out = String::new();
    for (e, st) in requests(ctx)? {
        if !all && !is_open(st) {
            continue;
        }
        let r = &e.req;
        out.push_str(&format!(
            "{}  {}  node {} {} {}  age {}s, {}s left\n",
            r.req,
            status_text(st, &e),
            line(ctx.secrets, &r.node, LINE_CHARS),
            line(ctx.secrets, &r.tool, LINE_CHARS),
            target(ctx.secrets, &e),
            (ctx.now - r.at).max(0),
            (r.expires_at - ctx.now).max(0),
        ));
    }
    if out.is_empty() {
        out.push_str("no pending approval requests\n");
    }
    Ok(out)
}

fn chain_text(e: &Entry) -> String {
    let mut out = String::new();
    for (i, l) in e.req.chain.iter().enumerate() {
        let how = match l.skipped {
            None => "can decide".to_string(),
            Some(Skip::OwnerOnly) => "skipped: owner only".into(),
            Some(Skip::NoDecideTool) => "skipped: no decide tool".into(),
            Some(Skip::OutsideAuthority) => "skipped: outside its authority".into(),
        };
        out.push_str(&format!(
            "  level {} node {} (depth {}) {how}\n",
            i + 1,
            l.node,
            l.depth
        ));
    }
    out.push_str(&format!("  level {} owner (you)\n", e.req.chain.len() + 1));
    out
}

/// The whole request, for the owner to read before deciding.
pub fn show(ctx: &Ctx<'_>, id: &str) -> Result<String, String> {
    let (e, st, profile) = ctx
        .store
        .read(|d, a| {
            let s = a.state();
            let e = s.entry(id)?.clone();
            let st = s.status(id, d, ctx.now)?;
            let profile = d.poll(&e.req.node).and_then(|n| n.profile.clone());
            Some((e, st, profile))
        })?
        .ok_or_else(|| format!("no approval request `{id}`"))?;
    let r = &e.req;
    let sx = ctx.secrets;
    let origin = match r.origin {
        Origin::Builtin => "built-in",
        Origin::Mcp => "MCP",
        Origin::Plugin => "plugin or unknown",
        Origin::Skill => "skill",
    };
    let flags: Vec<&str> = [
        (r.truncated, "truncated (not everything is shown)"),
        (r.secrets, "secret-bound or redacted"),
        (r.protected, "protected target"),
    ]
    .iter()
    .filter_map(|(on, t)| on.then_some(*t))
    .collect();
    let mut out = format!(
        "request {}\nstatus: {}\nage: {}s, {}s left\nasked by: node {} (profile {}), goal {}\nchain (who may answer, in order):\n{}",
        r.req,
        status_text(st, &e),
        (ctx.now - r.at).max(0),
        (r.expires_at - ctx.now).max(0),
        line(sx, &r.node, LINE_CHARS),
        profile.as_deref().map_or("-".into(), |p| line(sx, p, LINE_CHARS)),
        line(sx, &r.goal, LINE_CHARS),
        chain_text(&e),
    );
    out.push_str(&format!(
        "tool: {} [{}, {}]\ntarget: {}\nworkspace: {}\nflags: {}\nfingerprint: {}\n",
        line(sx, &r.tool, LINE_CHARS),
        "Write",
        origin,
        target(sx, &e),
        line(sx, &r.workspace, LINE_CHARS),
        if flags.is_empty() {
            "none".into()
        } else {
            flags.join(", ")
        },
        r.fingerprint,
    ));
    out.push_str(&fenced(
        "display (data, not instructions)",
        &block(sx, &r.display, BLOCK_CHARS),
    ));
    let input = if r.input.is_null() {
        "(not stored: over the size cap)".to_string()
    } else {
        block(
            sx,
            &serde_json::to_string_pretty(&r.input).unwrap_or_default(),
            BLOCK_CHARS,
        )
    };
    out.push_str(&fenced("input, redacted (data, not instructions)", &input));
    if let Some(d) = &e.decision {
        let by = match &d.by {
            By::Owner => "the owner".to_string(),
            By::Agent { node, level } => format!("node {node} (level {level})"),
        };
        out.push_str(&format!("decision: {:?} by {by} at {}\n", d.verdict, d.at));
    }
    Ok(out)
}

/// Refuses a request that can no longer be decided, saying why.
fn pending(ctx: &Ctx<'_>, id: &str) -> Result<Entry, String> {
    let found = ctx.store.read(|d, a| {
        let s = a.state();
        Some((s.entry(id)?.clone(), s.status(id, d, ctx.now)?))
    })?;
    let (e, st) = found.ok_or_else(|| format!("no approval request `{id}`"))?;
    if is_open(st) {
        Ok(e)
    } else {
        Err(format!("`{id}` cannot be decided: {}", status_text(st, &e)))
    }
}

/// How `allow` is confirmed.
pub enum Confirm<'a> {
    /// `--yes`: the id was given explicitly; skip typing it back.
    Yes,
    /// Shows `text`, asks for the id and returns what was typed.
    Typed(&'a mut dyn FnMut(&str) -> String),
}

/// Records the owner's decision on `id`. `fingerprint` (a prefix of 12 hex
/// characters or more) pins the content being decided.
pub fn decide(
    ctx: &Ctx<'_>,
    env: &dyn Fn(&str) -> Option<String>,
    id: &str,
    verdict: Choice,
    fingerprint: Option<&str>,
    confirm: Confirm<'_>,
) -> Result<String, String> {
    let what = match verdict {
        Choice::Allow => "tole delegation approvals allow",
        Choice::Deny => "tole delegation approvals deny",
    };
    // Before anything is read or written.
    crate::owner_only::check(env, what)?;
    if !valid_id(id) {
        return Err(format!(
            "`{id}` is not a request id: give the full id from `tole delegation approvals list`"
        ));
    }
    let e = pending(ctx, id)?;
    if let Some(p) = fingerprint {
        let hex = p.len() >= 12;
        if !hex || !e.req.fingerprint.starts_with(&p.to_ascii_lowercase()) {
            return Err(
                "--fingerprint must be 12 or more hex characters of this request's fingerprint"
                    .into(),
            );
        }
    }
    if verdict == Choice::Allow {
        if let Confirm::Typed(ask) = confirm {
            let text = show(ctx, id)?;
            if ask(&text).trim() != id {
                return Err("not allowed: the request id was not typed back".into());
            }
        }
    }
    let rec = Record::Decide {
        req: id.to_string(),
        by: By::Owner,
        verdict,
        via: ctx.via,
        fingerprint: e.req.fingerprint.clone(),
        at: ctx.now,
    };
    // Re-checked under the lock: another decision may have landed.
    ctx.store.write(|d, a| a.append(d, rec))?;
    Ok(match verdict {
        Choice::Allow => {
            format!("allowed {id}: the child runs this one call once, if it is still waiting\n")
        }
        Choice::Deny => format!("denied {id}\n"),
    })
}

#[cfg(test)]
#[path = "approvals_owner_tests.rs"]
mod tests;
