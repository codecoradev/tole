//! The events a destination is sent (#911, design section 2): ids, the
//! envelope and the per-type `data`. Pure: no network, no file, no clock.
//!
//! Redaction comes first, then the caps. A secret is redacted by the caller's
//! function on the whole text, and only after that is the text cut, so a cut
//! can never split a secret into a piece the redactor would no longer
//! recognise. Fields that would not fit are cut, never the envelope.

use serde_json::{json, Value};
use tole_core::ledger::RunStatus;

use super::turn::{TurnEnd, MAX_TURN_BODY};
use super::Event;

/// The envelope after serialization, at most.
pub const MAX_BODY: usize = 16 * 1024;
/// A cron run's `summary`, at most (after redaction).
pub const MAX_CRON_SUMMARY: usize = 4 * 1024;
/// An approval's `summary`, at most (after redaction).
pub const MAX_APPROVAL_SUMMARY: usize = 512;
/// Any other text field (job name, ids, tool), in characters.
pub const MAX_FIELD_CHARS: usize = 256;
/// The readable part of a cron event id, at most.
const READABLE_LEN: usize = 64;

/// Why an event could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// An approval id outside `[A-Za-z0-9._:-]` or longer than 128.
    BadId,
    /// Still over [`MAX_BODY`] with the summary dropped (or, for a turn event,
    /// over [`MAX_TURN_BODY`]: it is refused, never cut).
    TooLarge,
    /// The value could not be serialized.
    Serialize,
}

/// A built event: its id and the exact body bytes to store and send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub id: String,
    pub body: Vec<u8>,
}

/// `<readable>:<hash16>` of `key`: the part cron and turn ids share. `<hash16>`
/// (the first 16 hex digits of SHA-256 over the whole key) makes it unique;
/// `<readable>` is the key with every byte outside `[A-Za-z0-9._-]` replaced by
/// `-`, cut to 64, for humans.
fn readable_and_digest(key: &str) -> String {
    let readable: String = key
        .bytes()
        .take(READABLE_LEN)
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
                b as char
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "{readable}:{}",
        tole_core::webhook::short_digest(key.as_bytes())
    )
}

/// `cron:<readable>:<hash16>`: a pure function of the fire id, so the same in
/// `tole serve` and `tole cron tick`, on every attempt and after a restart. At
/// most 86 characters and always a valid delivery id.
pub fn cron_event_id(fire_id: &str) -> String {
    format!("cron:{}", readable_and_digest(fire_id))
}

/// `turn:<readable>:<hash16>:<last_seq>` (#962): a pure function of durable
/// state (the session id and the log position when the turn settled), so the
/// same across retries and a restart; every settle of a session has a larger
/// `last_seq` (the log is append-only), so a resumed leg never collides with an
/// earlier one. At most 5 + 64 + 1 + 16 + 1 + 20 = 107 characters.
pub fn turn_event_id(session_id: &str, last_seq: u64) -> String {
    format!("turn:{}:{last_seq}", readable_and_digest(session_id))
}

/// `approval:<approval_id>:requested` or `:resolved`; `None` when the approval
/// id would not be a valid delivery id (the loader rejects it, never sends).
pub fn approval_event_id(approval_id: &str, resolved: bool) -> Option<String> {
    let id = format!(
        "approval:{approval_id}:{}",
        if resolved { "resolved" } else { "requested" }
    );
    tole_core::webhook::valid_delivery(&id).then_some(id)
}

/// True when a cron run notifies nothing. The core rule is "empty output, or a
/// first line `[SILENT]`", but a webhook applies it to a run that finished
/// `ok` only: an error, budget stop, cancel, interruption or approval wait with
/// no output is what an owner most wants to hear about.
pub fn cron_is_silent(status: RunStatus, output: &str) -> bool {
    // Same rule as `cron::deliver::silent` (this module sits below `cron`
    // in the layering, so it cannot call it).
    status == RunStatus::Ok
        && (output.trim().is_empty() || output.lines().next().map(str::trim) == Some("[SILENT]"))
}

/// `RunStatus` as its wire name (snake_case).
fn status_name(s: RunStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// At most `max` bytes of `s`, cut at a character boundary.
fn cut_bytes(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// At most `max` characters of `s`.
fn cut_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn envelope(id: &str, event: Event, destination: &str, at: i64, data: Value) -> Value {
    json!({
        "v": 1,
        "id": id,
        "type": event.as_str(),
        "destination": destination,
        "at": at,
        "data": data,
    })
}

/// Serializes `value` within [`MAX_BODY`]. When it is over, drops `data.summary`
/// and sets `data.truncated`, once; still over is an error.
fn fit(mut value: Value) -> Result<Vec<u8>, BuildError> {
    let bytes = serde_json::to_vec(&value).map_err(|_| BuildError::Serialize)?;
    if bytes.len() <= MAX_BODY {
        return Ok(bytes);
    }
    if let Some(data) = value.get_mut("data").and_then(Value::as_object_mut) {
        data.insert("summary".into(), json!(""));
        data.insert("truncated".into(), json!(true));
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| BuildError::Serialize)?;
    if bytes.len() <= MAX_BODY {
        Ok(bytes)
    } else {
        Err(BuildError::TooLarge)
    }
}

/// A settled cron run, as the event needs it. `output` is the run's text,
/// before redaction.
#[derive(Debug, Clone, Copy)]
pub struct CronRun<'a> {
    pub job_id: &'a str,
    pub job_name: Option<&'a str>,
    pub fire_id: &'a str,
    pub status: RunStatus,
    pub scheduled_for: i64,
    pub finished_at: i64,
    pub output: &'a str,
    pub session_id: Option<&'a str>,
}

/// The `cron_run_finished` event for `destination`. `redact` is applied to
/// every text field first; the summary is then cut to [`MAX_CRON_SUMMARY`] at a
/// character boundary. `at` is when the event was made, and stays the same on
/// every attempt (it is inside the stored body).
pub fn cron_run_finished(
    destination: &str,
    at: i64,
    run: &CronRun<'_>,
    redact: &dyn Fn(&str) -> String,
) -> Result<Built, BuildError> {
    let id = cron_event_id(run.fire_id);
    let redacted = redact(run.output);
    let (summary, truncated) = cut_bytes(&redacted, MAX_CRON_SUMMARY);
    let field = |s: &str| cut_chars(&redact(s), MAX_FIELD_CHARS);
    let data = json!({
        "job": field(run.job_id),
        "job_name": run.job_name.map(field),
        "fire_id": field(run.fire_id),
        "status": status_name(run.status),
        "scheduled_for": run.scheduled_for,
        "finished_at": run.finished_at,
        "summary": summary,
        "truncated": truncated,
        "session_id": run.session_id.map(field),
    });
    let body = fit(envelope(&id, Event::CronRunFinished, destination, at, data))?;
    Ok(Built { id, body })
}

/// A queued approval, as the event needs it. `description` is before redaction.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalRequest<'a> {
    pub approval_id: &'a str,
    pub session_id: &'a str,
    pub tool: &'a str,
    pub description: &'a str,
    pub created_at: i64,
    pub expires_at: i64,
}

/// The `approval_requested` event. `risk` is `"write"`: only non-read-only,
/// non-destructive calls reach the queue.
pub fn approval_requested(
    destination: &str,
    at: i64,
    a: &ApprovalRequest<'_>,
    redact: &dyn Fn(&str) -> String,
) -> Result<Built, BuildError> {
    let id = approval_event_id(a.approval_id, false).ok_or(BuildError::BadId)?;
    let redacted = redact(a.description);
    let (summary, truncated) = cut_bytes(&redacted, MAX_APPROVAL_SUMMARY);
    let field = |s: &str| cut_chars(&redact(s), MAX_FIELD_CHARS);
    let data = json!({
        "source": "serve",
        "approval_id": a.approval_id,
        "session_id": field(a.session_id),
        "tool": field(a.tool),
        "risk": "write",
        "summary": summary,
        "truncated": truncated,
        "created_at": a.created_at,
        "expires_at": a.expires_at,
    });
    let body = fit(envelope(
        &id,
        Event::ApprovalRequested,
        destination,
        at,
        data,
    ))?;
    Ok(Built { id, body })
}

/// How an approval left `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Approved,
    Denied,
    Expired,
}

impl Resolution {
    fn as_str(self) -> &'static str {
        match self {
            Resolution::Approved => "approved",
            Resolution::Denied => "denied",
            Resolution::Expired => "expired",
        }
    }
}

/// An approval that left `Pending`. `session_id` and `tool` are before
/// redaction.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalOutcome<'a> {
    pub approval_id: &'a str,
    pub session_id: &'a str,
    pub tool: &'a str,
    pub resolution: Resolution,
    pub resolved_at: i64,
}

/// The `approval_resolved` event. No `decided_by`: serve has one credential
/// and no identity.
pub fn approval_resolved(
    destination: &str,
    at: i64,
    a: &ApprovalOutcome<'_>,
    redact: &dyn Fn(&str) -> String,
) -> Result<Built, BuildError> {
    let id = approval_event_id(a.approval_id, true).ok_or(BuildError::BadId)?;
    let field = |s: &str| cut_chars(&redact(s), MAX_FIELD_CHARS);
    let data = json!({
        "approval_id": a.approval_id,
        "session_id": field(a.session_id),
        "tool": field(a.tool),
        "status": a.resolution.as_str(),
        "resolved_at": a.resolved_at,
    });
    let body = fit(envelope(
        &id,
        Event::ApprovalResolved,
        destination,
        at,
        data,
    ))?;
    Ok(Built { id, body })
}

/// The `turn_finished` event (#962). `data` holds ids and closed words only:
/// `session_id`, `status`, `last_seq` (D2), and `approval_id` or `code` when
/// they apply. There is no free-text field, no `profile` (D7), no `entries`
/// count and no `truncated`: the body is a few hundred bytes by construction.
///
/// `approval_id` is the Pending queue entry of an `approval_pending` end. An id
/// outside its charset, or one the redactor would change (a secret-shaped id),
/// is refused as [`BuildError::BadId`]: it cannot be sent, so nothing is sent.
pub fn turn_finished(
    destination: &str,
    at: i64,
    session_id: &str,
    end: &TurnEnd,
    approval_id: Option<&str>,
    redact: &dyn Fn(&str) -> String,
) -> Result<Built, BuildError> {
    if !tole_core::storage::is_valid_session_id(session_id) {
        return Err(BuildError::BadId);
    }
    let id = turn_event_id(session_id, end.last_seq);
    if !tole_core::webhook::valid_delivery(&id) {
        return Err(BuildError::BadId);
    }
    let mut data = serde_json::Map::new();
    data.insert("session_id".into(), json!(session_id));
    data.insert("status".into(), json!(end.status.as_str()));
    data.insert("last_seq".into(), json!(end.last_seq));
    if let Some(a) = approval_id {
        if !tole_core::webhook::valid_delivery(a) {
            return Err(BuildError::BadId);
        }
        data.insert("approval_id".into(), json!(a));
    }
    if let Some(code) = end.code {
        data.insert("code".into(), json!(code.as_str()));
    }
    let value = envelope(
        &id,
        Event::TurnFinished,
        destination,
        at,
        Value::Object(data),
    );
    let body = serde_json::to_vec(&value).map_err(|_| BuildError::Serialize)?;
    if body.len() > MAX_TURN_BODY {
        return Err(BuildError::TooLarge);
    }
    // Redaction over the serialized envelope: every id is inside the delivery
    // charset, so a change means a secret-shaped id (or a destination name the
    // redactor knows). Nothing is sent rather than a redacted stand-in.
    let text = String::from_utf8(body).map_err(|_| BuildError::Serialize)?;
    if redact(&text) != text {
        return Err(BuildError::BadId);
    }
    Ok(Built {
        id,
        body: text.into_bytes(),
    })
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
