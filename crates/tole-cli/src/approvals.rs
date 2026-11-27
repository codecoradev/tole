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
use tole_core::action_summary::ActionSummary;
use tole_core::approval::{AllowlistApprover, Approver, ToolRequest, Verdict};
use tole_core::tool::Risk;
use tole_core::turn::call_fingerprint;

/// Pending decisions expire to denied after this long (lazily pruned).
pub const EXPIRY_SECS: u64 = 900;

/// How long after an Allow its session stays protected from eviction while
/// the resume thread claims the turn.
const RESUME_GRACE_MS: u64 = 30_000;

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
    /// Position in the queue's monotonic sequence (#571): lets a caller
    /// tell an entry queued during its own turn from an older one.
    pub seq: u64,
    /// The code-derived summary of the call (#1041), already redacted and
    /// bounded. `None` for an entry queued without one.
    pub summary: Option<ActionSummary>,
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

/// The part of an approval an announcement needs: no storage path and no call
/// fingerprint. `Debug` leaves out the description too, because it is raw text
/// until a builder redacts it: a stray `{:?}` in a log must not print it.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApprovalInfo {
    pub id: String,
    pub session_id: String,
    pub tool: String,
    /// The tool call's own text, NOT redacted: whoever builds an outgoing
    /// event must redact it first.
    pub description: String,
    pub created_at_ms: u64,
    /// The entry's place in the queue's sequence (monotonic).
    pub seq: u64,
}

impl std::fmt::Debug for ApprovalInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalInfo")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("tool", &self.tool)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl From<&PendingApproval> for ApprovalInfo {
    fn from(e: &PendingApproval) -> ApprovalInfo {
        ApprovalInfo {
            id: e.id.clone(),
            session_id: e.session_id.clone(),
            tool: e.tool.clone(),
            description: e.description.clone(),
            created_at_ms: e.created_at_ms,
            seq: e.seq,
        }
    }
}

/// What an [`ApprovalNotifier`] is told (#911): a request was queued, or an
/// entry left `Pending`.
///
/// **No ordering is promised.** `Resolved` can reach the notifier before
/// `Requested` of the same id: an entry is visible to the decision route a
/// moment before `queue_entry` calls back (the callback runs after the lock is
/// released, by design). A receiver keys its state by approval id, and
/// `resolved` wins whatever the arrival order; `seq` orders entries.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ApprovalEvent {
    /// `queue_entry` added this entry (status `Pending`).
    Requested(ApprovalInfo),
    /// The entry left `Pending`: `status` is `Approved`, `Denied` or `Expired`.
    /// Told exactly once per entry, because an entry leaves `Pending` once.
    Resolved {
        info: ApprovalInfo,
        status: ApprovalStatus,
    },
}

/// Called after an event, with NO queue lock held, on the thread that caused
/// it (a turn queuing a request, the decision route, the expiry timer). It
/// must only enqueue, and must never block: not on a full bounded channel
/// (`try_send` or an unbounded push), not on a lock another thread holds while
/// it does I/O. An approval never waits for it and never fails because of it,
/// a panic in it is caught, but a stall delays the fail-closed `Deny` and the
/// operator's reply, and `catch_unwind` cannot help with that.
pub type ApprovalNotifier = Arc<dyn Fn(ApprovalEvent) + Send + Sync>;

#[derive(Default)]
pub struct ApprovalQueue {
    entries: Mutex<Vec<PendingApproval>>,
    /// Set by `tole serve` when a webhook destination lists approval events;
    /// absent, the paths below are unchanged.
    notifier: Mutex<Option<ApprovalNotifier>>,
    /// Count of entries ever queued; each entry carries its number.
    seq: std::sync::atomic::AtomicU64,
    /// Approved one-shots keyed by (SESSION, fingerprint) — a shared
    /// tool+input across sessions must never let one session's ALLOW
    /// authorize another's replay (cora MAJOR). Value: tool + granted
    /// time for the stale sweep.
    one_shot: Mutex<HashMap<(String, u64), (String, u64)>>,
}

#[derive(Debug)]
pub enum DecisionOutcome {
    #[non_exhaustive]
    Decided {
        status: ApprovalStatus,
        session_id: String,
        tool: String,
        /// This call moved the entry out of `Pending`. False for a repeat
        /// (the stored status is returned) and for an entry that had already
        /// expired, so only the first decision is ever announced.
        transitioned: bool,
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

    /// Installs (or removes) the notifier (#911). Replaces any earlier one.
    pub fn set_notifier(&self, notifier: Option<ApprovalNotifier>) {
        *self.notifier.lock().unwrap_or_else(|p| p.into_inner()) = notifier;
    }

    /// Tells the notifier, if there is one; `event` is only built then. The
    /// caller holds no queue lock: the Arc is copied out of its mutex first,
    /// and a panic in the notifier is caught, so an approval is never changed
    /// or failed by it (a stall is the notifier's contract, see
    /// [`ApprovalNotifier`]).
    fn notify(&self, event: impl FnOnce() -> ApprovalEvent) {
        let n = self
            .notifier
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(n) = n {
            let event = event();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| n(event)));
        }
    }

    /// Moves entries past their expiry out of `Pending` now (and tells the
    /// notifier), instead of waiting for the next lazy call. An idle serve
    /// calls it on a timer, so an unanswered approval still announces its
    /// expiry. Returns how many it expired.
    pub fn expire_due(&self) -> usize {
        self.prune_expired().len()
    }

    /// Prune expired entries (pending → expired) and stale one-shots. Returns a
    /// full copy of each entry it expired, taken BEFORE the terminal entries
    /// are dropped below (the same call can both expire an entry and drop it),
    /// and tells the notifier of each after every lock is released.
    fn prune_expired(&self) -> Vec<PendingApproval> {
        let now = now_ms();
        let mut expired = Vec::new();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        for e in entries.iter_mut() {
            if e.status == ApprovalStatus::Pending
                && now.saturating_sub(e.created_at_ms) > EXPIRY_SECS * 1000
            {
                e.status = ApprovalStatus::Expired;
                expired.push(e.clone());
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
        for entry in &expired {
            self.notify(|| ApprovalEvent::Resolved {
                info: entry.into(),
                status: ApprovalStatus::Expired,
            });
        }
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
        self.queue_entry_with_summary(
            session_id,
            tool,
            description,
            fingerprint,
            storage_path,
            None,
        )
    }

    /// [`Self::queue_entry`] with the call's summary (#1041).
    pub fn queue_entry_with_summary(
        &self,
        session_id: &str,
        tool: &str,
        description: &str,
        fingerprint: u64,
        storage_path: std::path::PathBuf,
        summary: Option<ActionSummary>,
    ) -> String {
        self.prune_expired();
        // Issue #255 (rescan #36): the id previously used only
        // (millisecond timestamp, low-32 fingerprint) — two entries
        // queued in the same millisecond with matching fingerprints
        // collided. Cora CI round: a bare process-local counter still
        // collides ACROSS processes (each CLI is one process per
        // session, both emitting seq 0) and after restart. Mix in a
        // per-process RANDOM NONCE (generated once per process) so ids
        // are unique across processes and restarts.
        static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let nonce = *NONCE.get_or_init(|| {
            let mut b = [0u8; 8];
            if std::fs::File::open("/dev/urandom")
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
                .is_err()
            {
                // Fallback: time + pid hash (still cross-process unique
                // in practice).
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                b = (t ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
                    .to_le_bytes();
            }
            u64::from_le_bytes(b)
        });
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("apr-{:x}-{:x}{:x}", now_ms(), nonce, seq);
        // The sequence number is taken under the entries lock, so sequence
        // order is insertion order and `mark()` (which locks too) never
        // sees a number whose entry is not in the Vec yet.
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let entry_seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let entry = PendingApproval {
            id: id.clone(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            description: description.to_string(),
            fingerprint,
            created_at_ms: now_ms(),
            status: ApprovalStatus::Pending,
            storage_path,
            seq: entry_seq,
            summary,
        };
        entries.push(entry.clone());
        drop(entries);
        self.notify(|| ApprovalEvent::Requested((&entry).into()));
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

    /// True when `session_id` must stay live in memory: it has a Pending
    /// entry, or an Allow granted its one-shot less than [`RESUME_GRACE_MS`]
    /// ago that the resume has not consumed yet. The second case closes the
    /// gap between the decision and the resume thread claiming the turn (the
    /// session is idle and no longer Pending there).
    pub fn has_pending(&self, session_id: &str) -> bool {
        self.prune_expired();
        let pending = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .any(|e| e.session_id == session_id && e.status == ApprovalStatus::Pending);
        pending || {
            let now = now_ms();
            self.one_shot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|((sid, _), (_, granted))| {
                    sid == session_id && now.saturating_sub(*granted) < RESUME_GRACE_MS
                })
        }
    }

    /// Withdraw the one-shot an Allow of approval `id` granted, for a decision
    /// that turned out to resume nothing: a session reopened later must not
    /// silently consume it.
    pub fn revoke_one_shot(&self, id: &str) {
        let key = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|e| e.id == id)
            .map(|e| (e.session_id.clone(), e.fingerprint));
        if let Some(key) = key {
            self.one_shot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&key);
        }
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

    /// The queue position now (#571): snapshot it before a turn, then ask
    /// [`Self::queued_since`] what that turn queued.
    pub fn mark(&self) -> u64 {
        let _entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        self.seq.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The newest entry of `session_id` for `tool` queued strictly after
    /// `mark`, in any status (#571). The session id is part of the key, so
    /// another session's approval is never returned; the mark excludes
    /// older entries of the same session and tool, so a stop that queued
    /// nothing (a pre-tool hook denial) never picks up a stale row.
    pub fn queued_since(&self, session_id: &str, tool: &str, mark: u64) -> Option<PendingApproval> {
        self.prune_expired();
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .rev()
            .find(|e| e.session_id == session_id && e.tool == tool && e.seq > mark)
            .cloned()
    }

    /// Apply an operator decision.
    pub fn decide(&self, id: &str, allow: bool) -> DecisionOutcome {
        self.prune_expired();
        let (outcome, resolved) = {
            let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
                return DecisionOutcome::Unknown;
            };
            if e.status != ApprovalStatus::Pending {
                return DecisionOutcome::Decided {
                    status: e.status,
                    session_id: e.session_id.clone(),
                    tool: e.tool.clone(),
                    transitioned: false,
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
            (
                DecisionOutcome::Decided {
                    status: e.status,
                    session_id: e.session_id.clone(),
                    tool: e.tool.clone(),
                    transitioned: true,
                },
                e.clone(),
            )
        };
        // The entries lock is released: only the first decision is announced.
        self.notify(|| ApprovalEvent::Resolved {
            info: (&resolved).into(),
            status: resolved.status,
        });
        outcome
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
    pub(crate) fn take_one_shot(&self, session_id: &str, fingerprint: u64) -> bool {
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
        self.queue.queue_entry_with_summary(
            &self.session_id,
            req.tool,
            &req.description,
            fp,
            self.storage_path.clone(),
            Some(req.action_summary()),
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
/// `description` redacted with what serve redacts every egress with: the
/// process set (environment names and the role secrets of webhook keys and the
/// serve token) plus the `${NAME}` secrets of `web_fetch` headers, which are
/// only in the set once a session was built.
pub fn redact_description(description: &str) -> String {
    egress_secrets().redact(description).into_owned()
}

pub fn egress_secrets() -> tole_core::secrets::SecretSet {
    let mut set = tole_core::secrets::process_secrets();
    for (name, value) in tole_core::web::header_secrets(&|k| std::env::var(k).ok()) {
        set = set.with(&name, &value);
    }
    set
}

pub fn approval_json(e: &PendingApproval) -> Value {
    // The description is a tool call's own text (a path, a command, input), so
    // it can carry a secret the model read. `GET /approvals` and the event
    // stream are the one place it left the process unredacted (#911, design
    // section 6, question 11): redact it with the process's secret set.
    let set = egress_secrets();
    let description = set.redact(&e.description).into_owned();
    let mut row = json!({
        "id": e.id,
        "sessionId": e.session_id,
        "tool": e.tool,
        "description": description,
        "status": e.status.as_str(),
        "createdAtMs": e.created_at_ms,
        "expiresInSeconds": EXPIRY_SECS,
    });
    // #1041: the shared, code-derived summary, redacted once more with the
    // egress set (header secrets are only known here). Absent for an entry
    // queued without one, so an old consumer's row is unchanged.
    if let Some(s) = &e.summary {
        row["summary"] = json!(s.finalized(&set));
    }
    row
}

/// The `POST /sessions/{id}/prompt` body for a turn that stopped on an
/// approval (#571), or `None` when the stop queued nothing. `mark` is
/// [`ApprovalQueue::mark`] taken before the turn: only an entry of this
/// session and `tool` queued DURING the turn counts, so a pre-tool hook
/// denial (the turn reports the same outcome but nothing waits for a
/// decision) is `None`, as is an older undecided entry. The body is
/// `stopReason: "approval_required"` plus the queue row (the
/// `GET /approvals` shape) while that entry is still pending; when it
/// expired or was decided in the meantime `approval` is omitted.
pub fn approval_required_body(
    queue: &ApprovalQueue,
    session_id: &str,
    tool: &str,
    mark: u64,
) -> Option<Value> {
    let e = queue.queued_since(session_id, tool, mark)?;
    let mut out = json!({"stopReason": "approval_required"});
    if e.status == ApprovalStatus::Pending {
        out["approval"] = approval_json(&e);
    }
    Some(out)
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

    fn queue_one(q: &ApprovalQueue, session: &str, tool: &str) -> String {
        q.queue_entry(session, tool, "d", 7, std::path::PathBuf::from("/x"))
    }

    /// #571: an entry queued during the turn is named, with its row.
    #[test]
    fn approval_required_body_carries_the_row_queued_during_the_turn() {
        let q = ApprovalQueue::default();
        let mark = q.mark();
        let id = queue_one(&q, "s1", "write_file");
        let b = approval_required_body(&q, "s1", "write_file", mark).unwrap();
        assert_eq!(b["stopReason"], "approval_required");
        assert_eq!(b["approval"]["id"], id.as_str());
        assert_eq!(b["approval"]["tool"], "write_file");
        assert_eq!(b["approval"]["status"], "pending");
    }

    /// #571: queued during the turn but decided before the lookup ->
    /// still `approval_required`, no `approval` key.
    #[test]
    fn approval_required_body_without_a_pending_row_omits_approval() {
        let q = ApprovalQueue::default();
        let mark = q.mark();
        let id = queue_one(&q, "s1", "write_file");
        q.decide(&id, false);
        let b = approval_required_body(&q, "s1", "write_file", mark).unwrap();
        assert_eq!(b["stopReason"], "approval_required");
        assert!(b.get("approval").is_none(), "{b}");
    }

    /// #571: a stop that queued nothing (pre-tool hook denial) is `None`,
    /// even with an older undecided entry of the same session and tool.
    #[test]
    fn a_stop_that_queued_nothing_is_not_approval_required() {
        let q = ApprovalQueue::default();
        let stale = queue_one(&q, "s1", "write_file");
        let mark = q.mark();
        assert!(approval_required_body(&q, "s1", "write_file", mark).is_none());
        // The stale entry is still pending and untouched.
        assert_eq!(
            q.peek_pending(&stale).map(|p| p.1),
            Some("write_file".into())
        );
    }

    /// #571: another session's (or another tool's) entry is never returned.
    #[test]
    fn approval_required_body_never_leaks_other_sessions() {
        let q = ApprovalQueue::default();
        let mark = q.mark();
        queue_one(&q, "other", "write_file");
        queue_one(&q, "s1", "edit_file");
        assert!(approval_required_body(&q, "s1", "write_file", mark).is_none());
        let own = queue_one(&q, "s1", "write_file");
        queue_one(&q, "other", "write_file");
        let b = approval_required_body(&q, "s1", "write_file", mark).unwrap();
        assert_eq!(b["approval"]["id"], own.as_str());
        assert_eq!(b["approval"]["sessionId"], "s1");
    }

    /// #571: under concurrent pushes sequence order is insertion order.
    #[test]
    fn sequence_order_is_insertion_order_under_concurrent_pushes() {
        let q = std::sync::Arc::new(ApprovalQueue::default());
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let q = std::sync::Arc::clone(&q);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        queue_one(&q, &format!("s{t}"), "write_file");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let seqs: Vec<u64> = q.list(None).iter().map(|e| e.seq).collect();
        assert_eq!(seqs.len(), 400);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
        assert_eq!(q.mark(), 400);
    }

    // ---- the notifier (#911) ----

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A notifier that records every event.
    fn recording(q: &ApprovalQueue) -> Arc<Mutex<Vec<ApprovalEvent>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        q.set_notifier(Some(Arc::new(move |e| sink.lock().unwrap().push(e))));
        seen
    }

    /// Ages every entry by `secs` (as if it had been queued that long ago).
    fn age(q: &ApprovalQueue, secs: u64) {
        for e in q.entries.lock().unwrap().iter_mut() {
            e.created_at_ms = e.created_at_ms.saturating_sub(secs * 1000);
        }
    }

    #[test]
    fn a_queued_request_is_announced_once_with_its_entry() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let id = q.queue_entry("s1", "write_file", "write notes.md", 7, "/x".into());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let ApprovalEvent::Requested(e) = &seen[0] else {
            panic!("{:?}", seen[0])
        };
        assert_eq!(
            (e.id.as_str(), e.session_id.as_str(), e.tool.as_str()),
            (id.as_str(), "s1", "write_file")
        );
        assert_eq!(e.description, "write notes.md");
        assert_eq!(e.seq, 1);
    }

    #[test]
    fn only_the_first_decision_is_announced() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let id = queue_one(&q, "s1", "write_file");
        let first = q.decide(&id, true);
        let again = q.decide(&id, true);
        let flipped = q.decide(&id, false);
        let transitioned = |o: &DecisionOutcome| {
            matches!(
                o,
                DecisionOutcome::Decided {
                    transitioned: true,
                    ..
                }
            )
        };
        assert!(transitioned(&first));
        assert!(!transitioned(&again) && !transitioned(&flipped));
        // The stored status is what a repeat sees, whatever it asked for.
        assert!(matches!(
            flipped,
            DecisionOutcome::Decided {
                status: ApprovalStatus::Approved,
                ..
            }
        ));
        let seen = seen.lock().unwrap();
        let resolved: Vec<_> = seen
            .iter()
            .filter(|e| matches!(e, ApprovalEvent::Resolved { .. }))
            .collect();
        assert_eq!(resolved.len(), 1, "{seen:?}");
        assert!(matches!(
            resolved[0],
            ApprovalEvent::Resolved {
                status: ApprovalStatus::Approved,
                ..
            }
        ));
    }

    #[test]
    fn a_denial_is_announced_as_denied_and_an_unknown_id_is_silent() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let id = queue_one(&q, "s1", "write_file");
        assert!(matches!(
            q.decide("apr-nope", true),
            DecisionOutcome::Unknown
        ));
        q.decide(&id, false);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "requested + denied only: {seen:?}");
        assert!(matches!(
            &seen[1],
            ApprovalEvent::Resolved {
                status: ApprovalStatus::Denied,
                ..
            }
        ));
    }

    #[test]
    fn an_idle_expiry_is_announced_with_the_full_entry_exactly_once() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let id = q.queue_entry("s1", "write_file", "write notes.md", 7, "/x".into());
        assert_eq!(q.expire_due(), 0, "not due yet");
        age(&q, EXPIRY_SECS + 1);
        // The timer is the only caller: nothing else ran since the entry aged.
        assert_eq!(q.expire_due(), 1);
        assert_eq!(q.expire_due(), 0, "it leaves Pending once");
        let seen = seen.lock().unwrap();
        let expired: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                ApprovalEvent::Resolved { info, status } => Some((info, *status)),
                _ => None,
            })
            .collect();
        assert_eq!(expired.len(), 1, "{seen:?}");
        let (entry, status) = expired[0];
        // The same call expired AND dropped it, yet the copy is complete: the
        // announcement needs its session and tool.
        assert_eq!(status, ApprovalStatus::Expired);
        assert_eq!(
            (
                entry.id.as_str(),
                entry.session_id.as_str(),
                entry.tool.as_str()
            ),
            (id.as_str(), "s1", "write_file")
        );
        assert!(q.list(None).is_empty(), "and it is gone from the queue");
    }

    #[test]
    fn a_lazy_call_announces_the_expiry_it_finds_and_a_late_decision_does_not_repeat_it() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let id = queue_one(&q, "s1", "write_file");
        age(&q, EXPIRY_SECS + 1);
        // Any lazy caller (here: a listing) finds it expired.
        assert!(q.list(None).is_empty());
        assert!(matches!(q.decide(&id, true), DecisionOutcome::Unknown));
        let seen = seen.lock().unwrap();
        let expiries = seen
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ApprovalEvent::Resolved {
                        status: ApprovalStatus::Expired,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(expiries, 1, "{seen:?}");
    }

    #[test]
    fn a_decision_after_the_expiry_is_not_announced_again() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let id = queue_one(&q, "s1", "write_file");
        // Past its 900 s of life but inside the 900 s the terminal entry stays
        // visible: expired once, still decidable-to-nothing.
        age(&q, EXPIRY_SECS + 1);
        q.expire_due();
        let late = q.decide(&id, true);
        // Dropped in the same call (age > 2x not needed: it was aged past both).
        assert!(!matches!(
            late,
            DecisionOutcome::Decided {
                transitioned: true,
                ..
            }
        ));
        let n = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, ApprovalEvent::Resolved { .. }))
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn the_notifier_runs_with_no_queue_lock_held() {
        // Calling back into the queue from the notifier would deadlock if a
        // queue lock were held. Run it on a thread with a deadline, so a
        // regression fails instead of hanging the suite.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let q = Arc::new(ApprovalQueue::default());
            let calls = Arc::new(AtomicUsize::new(0));
            let (qq, c) = (Arc::clone(&q), Arc::clone(&calls));
            q.set_notifier(Some(Arc::new(move |_| {
                let _ = qq.list(None);
                let _ = qq.mark();
                let _ = qq.peek_pending("x");
                c.fetch_add(1, Ordering::SeqCst);
            })));
            let id = queue_one(&q, "s1", "write_file");
            q.decide(&id, true);
            queue_one(&q, "s1", "edit_file");
            age(&q, EXPIRY_SECS + 1);
            q.expire_due();
            let _ = done_tx.send(calls.load(Ordering::SeqCst));
        });
        let calls = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a queue lock was held while the notifier ran");
        assert_eq!(calls, 4, "requested, approved, requested, expired");
    }

    #[test]
    fn a_slow_notifier_does_not_block_other_threads_using_the_queue() {
        let q = Arc::new(ApprovalQueue::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        q.set_notifier(Some(Arc::new(move |_| {
            let _ = entered_tx.send(());
            // A notifier stuck on something slow.
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        })));
        let qa = Arc::clone(&q);
        let slow = std::thread::spawn(move || queue_one(&qa, "s1", "write_file"));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The entry is visible and the queue usable from another thread while
        // the first one is inside the notifier.
        let started = std::time::Instant::now();
        assert_eq!(q.list(None).len(), 1);
        let _ = q.mark();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the queue was blocked"
        );
        let _ = release_tx.send(());
        slow.join().unwrap();
    }

    #[test]
    fn two_threads_deciding_the_same_entry_announce_it_once() {
        for _ in 0..50 {
            let q = Arc::new(ApprovalQueue::default());
            let seen = recording(&q);
            let id = queue_one(&q, "s1", "write_file");
            let wins = Arc::new(AtomicUsize::new(0));
            let handles: Vec<_> = [true, false, true, false]
                .into_iter()
                .map(|allow| {
                    let (q, id, wins) = (Arc::clone(&q), id.clone(), Arc::clone(&wins));
                    std::thread::spawn(move || {
                        if let DecisionOutcome::Decided {
                            transitioned: true, ..
                        } = q.decide(&id, allow)
                        {
                            wins.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(wins.load(Ordering::SeqCst), 1, "exactly one transition");
            let n = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, ApprovalEvent::Resolved { .. }))
                .count();
            assert_eq!(n, 1, "exactly one announcement");
        }
    }

    #[test]
    fn a_decision_racing_the_expiry_timer_is_announced_once() {
        for _ in 0..50 {
            let q = Arc::new(ApprovalQueue::default());
            let seen = recording(&q);
            let id = queue_one(&q, "s1", "write_file");
            age(&q, EXPIRY_SECS + 1);
            let (qa, ida) = (Arc::clone(&q), id.clone());
            let decider = std::thread::spawn(move || qa.decide(&ida, true));
            let qb = Arc::clone(&q);
            let timer = std::thread::spawn(move || qb.expire_due());
            decider.join().unwrap();
            timer.join().unwrap();
            let n = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, ApprovalEvent::Resolved { .. }))
                .count();
            assert_eq!(n, 1, "an entry leaves Pending once");
        }
    }

    #[test]
    fn several_expiries_in_one_pass_are_one_event_each_and_a_pass_that_queues_announces_both() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let a = queue_one(&q, "s1", "write_file");
        let b = queue_one(&q, "s2", "edit_file");
        age(&q, EXPIRY_SECS + 1);
        // A new request prunes the two expired ones first: their expiries are
        // announced, then its own request.
        let c = queue_one(&q, "s3", "run_command");
        let seen = seen.lock().unwrap();
        let tags: Vec<String> = seen
            .iter()
            .map(|e| match e {
                ApprovalEvent::Requested(i) => format!("req:{}", i.id),
                ApprovalEvent::Resolved { info, status } => {
                    format!("{}:{}", status.as_str(), info.id)
                }
            })
            .collect();
        let expired: std::collections::BTreeSet<_> = tags
            .iter()
            .filter(|t| t.starts_with("expired:"))
            .cloned()
            .collect();
        assert_eq!(
            expired,
            [format!("expired:{a}"), format!("expired:{b}")]
                .into_iter()
                .collect()
        );
        assert_eq!(tags.last().unwrap(), &format!("req:{c}"), "{tags:?}");
    }

    #[test]
    fn an_announcement_carries_no_path_and_debug_hides_the_text() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        q.queue_entry(
            "s1",
            "run_command",
            "SECRET-LOOKING-TEXT",
            9,
            "/home/me/.tole/s.jsonl".into(),
        );
        let seen = seen.lock().unwrap();
        let shown = format!("{:?}", seen[0]);
        assert!(!shown.contains("/home/me"), "{shown}");
        assert!(!shown.contains("SECRET-LOOKING-TEXT"), "{shown}");
    }

    #[test]
    fn a_panicking_notifier_changes_nothing_about_the_approval() {
        let q = ApprovalQueue::default();
        q.set_notifier(Some(Arc::new(|_| panic!("a bad notifier"))));
        let id = queue_one(&q, "s1", "write_file");
        assert_eq!(q.peek_pending(&id).map(|p| p.1), Some("write_file".into()));
        let d = q.decide(&id, true);
        assert!(matches!(
            d,
            DecisionOutcome::Decided {
                status: ApprovalStatus::Approved,
                transitioned: true,
                ..
            }
        ));
        // The one-shot was granted: the replay would be allowed.
        assert!(q.take_one_shot("s1", 7));
    }

    #[test]
    fn an_approved_session_stays_held_until_the_resume_consumes_it() {
        let q = ApprovalQueue::default();
        let id = queue_one(&q, "s1", "write_file");
        assert!(q.has_pending("s1"));
        q.decide(&id, true);
        // Approved but not yet replayed: still held (the resume window).
        assert!(q.has_pending("s1"));
        assert!(!q.has_pending("other"));
        assert!(q.take_one_shot("s1", 7));
        assert!(!q.has_pending("s1"));
    }

    #[test]
    fn revoking_withdraws_the_one_shot() {
        let q = ApprovalQueue::default();
        let id = queue_one(&q, "s1", "write_file");
        q.decide(&id, true);
        q.revoke_one_shot(&id);
        assert!(!q.take_one_shot("s1", 7));
        assert!(!q.has_pending("s1"));
    }

    #[test]
    fn with_no_notifier_the_decisions_are_what_they_always_were() {
        let q = ApprovalQueue::default();
        let id = queue_one(&q, "s1", "write_file");
        let d = q.decide(&id, false);
        assert!(matches!(
            d,
            DecisionOutcome::Decided {
                status: ApprovalStatus::Denied,
                transitioned: true,
                ..
            }
        ));
        age(&q, EXPIRY_SECS + 1);
        assert_eq!(q.expire_due(), 0, "a decided entry is not expired");
    }

    #[test]
    fn a_removed_or_replaced_notifier_stops_telling_the_old_one() {
        let q = ApprovalQueue::default();
        let first = recording(&q);
        queue_one(&q, "s1", "write_file");
        let second = recording(&q); // replaces the first
        queue_one(&q, "s1", "edit_file");
        q.set_notifier(None);
        queue_one(&q, "s1", "run_command");
        assert_eq!(first.lock().unwrap().len(), 1);
        assert_eq!(second.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_decided_entry_is_never_announced_as_expired() {
        let q = ApprovalQueue::default();
        let seen = recording(&q);
        let id = queue_one(&q, "s1", "write_file");
        q.decide(&id, true);
        age(&q, EXPIRY_SECS + 1);
        q.expire_due();
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|e| matches!(
            e,
            ApprovalEvent::Resolved {
                status: ApprovalStatus::Expired,
                ..
            }
        )));
    }

    #[test]
    fn a_secret_in_a_description_is_redacted_where_approvals_are_listed() {
        let canary = "approval-canary-secret-7f6e5d4c3b2a";
        tole_core::secrets::add_role_secret("approval_test_canary", canary);
        let q = ApprovalQueue::default();
        let id = q.queue_entry(
            "s1",
            "run_command",
            &format!("curl -H 'Authorization: {canary}' https://example.com"),
            9,
            "/x".into(),
        );
        let row = q.list(None).into_iter().find(|e| e.id == id).unwrap();
        let json = approval_json(&row).to_string();
        assert!(!json.contains(canary), "the secret leaked: {json}");
        assert!(
            json.contains("curl"),
            "the rest of the text is kept: {json}"
        );
        // The stored entry itself is untouched: a decision still replays it.
        assert!(row.description.contains(canary));
    }
}
