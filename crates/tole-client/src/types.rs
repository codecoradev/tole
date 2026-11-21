//! Request and response types of the serve routes a graphical client uses.
//!
//! Field names are the wire names: serve is not uniform (`stopReason` and
//! `sessionId` are camelCase, `plan_mode` and `usage_report` are snake_case),
//! and each rename below mirrors what the server emits. Every type both
//! serializes and deserializes, so the contract test can prove a response is
//! decoded without loss (re-serialized JSON equals the server's JSON), and the
//! committed JSON Schema is derived from these types (`schema/`, #1068).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use tole_core::entry::Entry;

macro_rules! wire {
    ($($item:item)*) => {$(
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[cfg_attr(test, derive(schemars::JsonSchema))]
        $item
    )*};
}

wire! {
    /// `GET /health`: `status`, and since serve reports its protocol and
    /// binary version (#1043) also `protocol` and `version`. Both are
    /// optional so a server that predates them still decodes, and a decoded
    /// answer serializes back to the JSON the server sent.
    pub struct Health {
        pub status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub protocol: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub version: Option<String>,
    }

    /// One live session in `GET /sessions`.
    pub struct SessionSummary {
        pub id: String,
        pub busy: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub profile: Option<String>,
    }

    /// `GET /sessions`: the sessions live in this serve.
    pub struct SessionList {
        pub sessions: Vec<SessionSummary>,
    }

    /// One row of `GET /sessions/index`: a past or live session of the workspace.
    #[serde(rename_all = "camelCase")]
    pub struct PastSession {
        pub id: String,
        pub name: Option<String>,
        pub folder: String,
        /// Milliseconds since the Unix epoch.
        pub last_activity: u64,
        pub entries: u64,
        pub turns: u64,
        /// The session this one was forked from.
        pub parent: Option<String>,
        pub live: bool,
        pub busy: bool,
    }

    /// `GET /sessions/index`. Page with `cursor = next_cursor`.
    #[serde(rename_all = "camelCase")]
    pub struct SessionIndex {
        pub sessions: Vec<PastSession>,
        pub next_cursor: Option<String>,
        pub skipped: u64,
        pub scan_truncated: bool,
    }

    /// `POST /sessions`: create a session, or reopen one with `resume`.
    #[derive(Default)]
    pub struct NewSession {
        /// Working folder (must lie inside the server's workspace).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub profile: Option<String>,
        /// Only when creating: the registry without mutation tools.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub plan_mode: Option<bool>,
        /// Reopen this session id instead of creating one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resume: Option<String>,
    }

    /// The answer to `POST /sessions`. A create sets `workspace`; a resume
    /// sets `resumed` and `already_live` (and `busy` for a session that was
    /// already live).
    #[serde(rename_all = "camelCase")]
    pub struct SessionOpened {
        pub session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resumed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub already_live: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub busy: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub plan_mode: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub profile: Option<String>,
    }

    /// `POST /sessions/{id}/prompt`.
    pub struct PromptRequest {
        pub text: String,
        /// Ids of images uploaded to this session.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub images: Vec<String>,
    }

    /// The answer to a prompt, once the turn stopped. `stop_reason` is
    /// `approval_required` when a Write waits for a decision: `approval` is
    /// then the queue row (absent when it was decided or expired meanwhile).
    #[serde(rename_all = "camelCase")]
    pub struct PromptReply {
        pub stop_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub approval: Option<Approval>,
    }

    /// `POST /sessions/{id}/cancel`.
    #[serde(rename_all = "camelCase")]
    pub struct CancelReply {
        pub cancelled: bool,
        pub session_id: String,
    }

    /// `GET /sessions/{id}/status`. Model, effort and context figures are
    /// `null` until known; `stale` says they come from the last snapshot
    /// because a turn is running.
    #[serde(rename_all = "camelCase")]
    pub struct SessionStatus {
        pub id: String,
        /// `null` while a turn holds the session.
        pub entries: Option<u64>,
        pub busy: bool,
        /// The usage report as the server builds it (opaque to the client).
        #[serde(rename = "usage_report")]
        pub usage_report: Value,
        pub model: Option<String>,
        pub effort: Option<String>,
        pub context_window: Option<u64>,
        pub context_used: Option<u64>,
        pub name: Option<String>,
        pub snapshot_at: Option<u64>,
        pub stale: bool,
    }

    /// `GET /sessions/{id}/entries`. Page with `after = next_after` while
    /// `has_more`.
    #[serde(rename_all = "camelCase")]
    pub struct EntriesPage {
        pub id: String,
        #[cfg_attr(test, schemars(with = "Vec<crate::schema::EntrySchema>"))]
        pub entries: Vec<Entry>,
        pub next_after: u64,
        pub has_more: bool,
        pub last_seq: u64,
    }

    /// One row of the approval queue (`GET /approvals`, the
    /// `approval_requested` / `approval_resolved` event frames, and a prompt
    /// that stopped for approval). Rows of a delegated child have a different
    /// field set: what the client does not model is kept in `extra`.
    #[serde(rename_all = "camelCase")]
    pub struct Approval {
        pub id: String,
        pub tool: String,
        /// `pending`, `approved`, `denied` or `expired`.
        pub status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub created_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub expires_in_seconds: Option<u64>,
        /// What the request will do, derived by code next to the gate (#1041).
        /// Absent on a row from a server without it. Boxed so that the event
        /// type that carries an approval stays small.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub summary: Option<Box<ActionSummary>>,
        #[serde(flatten)]
        pub extra: Map<String, Value>,
    }

    /// The structured summary of an approval request (#1041). `kind` and
    /// `effect` are the server's codes (`create_file`, `adds`, ...) and stay
    /// strings: a client translates by code, and a code it does not know is
    /// kept, not rejected. Nothing in a summary is model text, and it never
    /// changes the risk tier.
    #[serde(rename_all = "camelCase")]
    pub struct ActionSummary {
        pub kind: String,
        /// A path relative to the working folder, a command line, or a tool
        /// name.
        pub target: String,
        pub effect: String,
        /// True only when the server confirmed the target is inside the
        /// working folder.
        #[serde(default)]
        pub inside_workspace: bool,
        /// True when `target` was cut.
        #[serde(default)]
        pub target_truncated: bool,
        /// Bounded raw detail: diff lines (`+` and `-`), the command, or the
        /// input.
        #[serde(default)]
        pub preview: Vec<String>,
        /// True when `preview` is not the whole of it.
        #[serde(default)]
        pub preview_truncated: bool,
    }

    /// `GET /approvals`.
    pub struct ApprovalList {
        pub approvals: Vec<Approval>,
    }

    /// The body of `POST /approvals/{id}/decision`. Build it with
    /// [`DecisionRequest::new`]: a delegated child's row (`dap-*` id) needs
    /// `confirm` (the id) for an allow, and its `fingerprint`.
    pub struct DecisionRequest {
        pub decision: Decision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub confirm: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub fingerprint: Option<String>,
    }

    /// `POST /approvals/{id}/decision`. A repeat of the same verdict is
    /// idempotent; the opposite verdict is a 409 ([`crate::Error::Status`]).
    pub struct DecisionReply {
        pub id: String,
        pub status: String,
        /// `true` when the allowed call is being replayed in the background.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resuming: Option<bool>,
    }

    /// `GET /models`: what the owner lets a client pick.
    pub struct Models {
        pub models: Vec<String>,
        pub default: Option<String>,
    }

    /// The body of `POST /sessions/{id}/interrupt`: steer the running turn
    /// without cancelling it.
    pub struct InterruptRequest {
        /// What to tell the agent; a blank text is refused (`400`).
        pub text: String,
        /// Also wake a long wait of the turn; serve reads it as `false` when
        /// absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub urgent: Option<bool>,
    }

    /// The answer to `POST /sessions/{id}/interrupt` (`202`): queued, and
    /// delivered at the turn's next safe point.
    #[serde(rename_all = "camelCase")]
    pub struct InterruptReply {
        pub queued: bool,
        pub urgent: bool,
        pub session_id: String,
    }

    /// The answer to `POST /sessions/{id}/detach` (`202`): the running turn
    /// stops being waited for and ends `detached`, resumable.
    #[serde(rename_all = "camelCase")]
    pub struct DetachReply {
        pub detached: bool,
        pub session_id: String,
    }

    /// The body of `POST /sessions/{id}/fork`.
    #[derive(Default)]
    pub struct ForkRequest {
        /// The `seq` of a settled entry (as `entries` shows it) to cut at;
        /// absent means the last settled entry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub at: Option<u64>,
    }

    /// The answer to `POST /sessions/{id}/fork`: a NEW session file. It is not
    /// live; open it with [`NewSession::resume`].
    #[serde(rename_all = "camelCase")]
    pub struct ForkReply {
        /// The new session.
        pub id: String,
        pub forked_from: String,
        /// The entry id the fork was cut at.
        pub at: String,
        /// That entry's `seq` in the source, `null` when it is not found there.
        pub seq: Option<u64>,
    }

    /// One session of `GET /sessions/{id}/tree`.
    pub struct TreeNode {
        pub id: String,
        /// The session this one was forked from.
        pub parent: Option<String>,
        /// The entry id it was cut at.
        pub at: Option<String>,
        /// The server's state code for the session (a string, so a code this
        /// client does not know is kept).
        pub state: String,
        pub turns: u64,
        /// 0 for the root of the family.
        pub depth: u64,
        /// True for the session the request named.
        pub current: bool,
    }

    /// `GET /sessions/{id}/tree`: the session's parent chain up to the root and
    /// every fork below it, depth-first.
    pub struct SessionTree {
        pub id: String,
        pub tree: Vec<TreeNode>,
    }

    /// The answer to `POST /sessions/{id}/compact`: `compaction` is
    /// `requested`; the session's NEXT prompt compacts first.
    pub struct CompactReply {
        pub id: String,
        pub compaction: String,
    }

    /// The body of `POST /sessions/{id}/name`.
    pub struct NameRequest {
        /// At most 120 characters, no control characters.
        pub name: String,
    }

    /// The answer to `POST /sessions/{id}/name`: the trimmed label.
    pub struct NameReply {
        pub id: String,
        pub name: String,
    }

    /// The body of `POST /sessions/{id}/model`.
    pub struct ModelRequest {
        /// One of [`Models::models`].
        pub model: String,
    }

    /// The answer to `POST /sessions/{id}/model`.
    pub struct ModelReply {
        pub id: String,
        pub model: String,
    }

    /// The body of `POST /sessions/{id}/effort`. The key is always sent: a
    /// missing one is a `400`, `null` clears the override.
    pub struct EffortRequest {
        /// `minimal`, `low`, `medium` or `high`; `None` clears it.
        pub effort: Option<String>,
    }

    /// The answer to `POST /sessions/{id}/effort`: the canonical level, or
    /// `null` once cleared.
    pub struct EffortReply {
        pub id: String,
        pub effort: Option<String>,
    }

    /// The answer to `POST /sessions/{id}/images`: the reference to put in
    /// [`PromptRequest::images`]. `id` is the content hash (`fnv1a64`).
    pub struct ImageUploaded {
        pub id: String,
        /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`, sniffed by
        /// the server from the bytes.
        pub mime: String,
        pub bytes: u64,
        pub fnv1a64: String,
    }

    /// The body of `POST /missions`. Only these four keys are accepted: the
    /// verify command, the workspace and the allow list are the server owner's
    /// configuration, and a budget can only be lowered below the owner's
    /// ceiling.
    pub struct MissionRequest {
        pub goal: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_steps: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_minutes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_tokens: Option<u64>,
    }

    /// The answer to `POST /missions` (`202`): the mission is its own session
    /// and runs on the server, one at a time.
    pub struct MissionStarted {
        pub id: String,
        /// `running`.
        pub status: String,
    }

    /// `GET /missions/{id}`. While it runs only `id`, `status` (`running`) and
    /// `running` are set. Once it ended `status` is how (`complete`,
    /// `exhausted_steps`, `exhausted_tokens`, `exhausted_time`,
    /// `verify_failed`, `verify_refused`, `stopped`, or `error` with `error`
    /// set); `summary` is the durable `fact/mission` register as the server
    /// holds it. A figure the server sent as `null` reads as `None`.
    #[serde(rename_all = "camelCase")]
    pub struct Mission {
        pub id: String,
        pub status: String,
        pub running: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub turns: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub steps: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub wall_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<String>,
    }

    /// The answer to `POST /missions/{id}/cancel` (`202`): the mission ends at
    /// its next checkpoint with a durable summary.
    pub struct MissionCancelled {
        pub id: String,
        pub cancelled: bool,
    }

    /// One row of `GET /discussions`. A ledger that cannot be read is a row
    /// with `status` `unreadable` and an `error` instead of the figures.
    pub struct DiscussionRow {
        pub id: String,
        /// `open`, `closed (<gate>)` or `unreadable`.
        pub status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub rounds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tokens_used: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub budget: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub question: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<String>,
    }

    /// `GET /discussions`.
    pub struct DiscussionList {
        pub discussions: Vec<DiscussionRow>,
    }

    /// One round of a discussion. `summary` is the scribe's digest (decisions,
    /// claims, `disagreements`, open questions, actions) or the list of
    /// speakers when it was passed on as written; `checklist` is the
    /// claim-checker's verdict per criterion. Both stay as the server's JSON.
    pub struct DiscussionRound {
        pub round: u32,
        pub speakers: Vec<String>,
        pub summary: Option<Value>,
        pub checklist: Option<Vec<Value>>,
        pub gate: Option<String>,
        pub tokens: u64,
        pub retries: u64,
        pub charged: Vec<Value>,
    }

    /// How a closed discussion ended: the gate's decision and the report text.
    pub struct DiscussionOutcome {
        pub gate: String,
        pub why: String,
        pub text: String,
    }

    /// The owner's decision about a helper's first call of an allowed tool
    /// (#1039): `verdict` is `allow`, `deny` or `expired`.
    pub struct DiscussionToolApproval {
        pub request: String,
        pub tool: String,
        pub verdict: String,
    }

    /// `GET /discussions/{id}`.
    pub struct Discussion {
        pub id: String,
        pub status: String,
        pub question: String,
        pub criteria: Vec<String>,
        pub participants: Vec<String>,
        pub budget: u64,
        pub max_rounds: u32,
        /// The connected tools the owner allowed the helpers (#1039); left
        /// out when none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub tools: Vec<String>,
        /// The owner's decisions about those tools, in ledger order; left out
        /// when none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub tool_approvals: Vec<DiscussionToolApproval>,
        pub tokens_used: u64,
        pub open_calls: u64,
        pub rounds: Vec<DiscussionRound>,
        pub outcome: Option<DiscussionOutcome>,
        /// `running` (a thread of this server drives it), `closed`, or
        /// `interrupted` (open and nobody drives it: resume it).
        pub state: String,
    }

    /// The answer to `POST /discussions`: the contract is recorded and the
    /// discussion runs.
    pub struct DiscussionStarted {
        pub id: String,
        pub status: String,
        pub plan: String,
        pub advice: Vec<String>,
    }

    /// One job of `GET /cron/jobs`. `created_by` is `cli` (its owner) or
    /// `agent`; `awaiting_review` is true for an agent's job the owner has not
    /// enabled, which cannot be resumed over serve.
    pub struct CronJob {
        pub id: String,
        pub name: String,
        pub schedule: String,
        pub timezone: String,
        pub mode: String,
        /// `active` or `paused`.
        pub state: String,
        /// Unix seconds of the next run, `null` while paused.
        pub next_run: Option<i64>,
        pub next: String,
        /// The last run's status: `ok`, `error`, `cancelled`, `budget`,
        /// `awaiting_approval` or `interrupted`.
        pub last: Option<String>,
        pub missed: u64,
        pub blocked: u64,
        pub created_by: String,
        pub awaiting_review: bool,
    }

    /// Whether a ticker is alive to run the jobs, and where it runs.
    pub struct CronScheduler {
        pub alive: bool,
        /// `serve` (`tole serve --cron`) or `tick` (`tole cron tick`).
        pub host: Option<String>,
        pub age_seconds: Option<i64>,
    }

    /// `GET /cron/jobs`.
    pub struct CronJobs {
        pub jobs: Vec<CronJob>,
        pub scheduler: CronScheduler,
    }

    /// The answer to pausing or resuming a job.
    pub struct CronJobState {
        pub id: String,
        pub state: String,
    }

    /// One run of `GET /cron/runs`; `output` is capped by the server.
    pub struct CronRun {
        pub fire_id: String,
        pub job_id: String,
        pub scheduled_for: i64,
        /// `null` while the run is going.
        pub status: Option<String>,
        pub session_id: Option<String>,
        pub usage: Option<Value>,
        pub deliveries: Vec<Value>,
        pub output: String,
        pub output_truncated: bool,
    }

    /// `GET /cron/runs`.
    pub struct CronRuns {
        pub runs: Vec<CronRun>,
    }

    /// One helper of `GET /profiles`. A profile that cannot be loaded is a row
    /// with only `name` and `error`. `persona` is the first line of the persona
    /// file (absent when it has none); `models` has the tiers the profile sets
    /// and its `default` tier name; `may_use` is the profile's own allow list
    /// and `session_allow` what a session opened with it gets on this server.
    pub struct Profile {
        pub name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub persona: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub persona_chars: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub models: Option<Map<String, Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub memory_room: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub may_use: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub session_allow: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<String>,
    }

    /// `GET /profiles`.
    pub struct Profiles {
        pub profiles: Vec<Profile>,
    }

    /// One skill of `GET /skills`. `state` is `approved`, `changed` (the
    /// content differs from what was approved: it is not used), `not_approved`,
    /// `refused` (the approval ledger disagrees) or `unreadable`.
    pub struct SkillRow {
        pub name: String,
        pub state: String,
        pub status: String,
        pub hash: Option<String>,
        pub approved_hash: Option<String>,
        pub approved_at: Option<String>,
        pub source: Option<Value>,
        pub source_text: String,
        pub history: Vec<Value>,
    }

    /// A skill the agent proposed, waiting for the owner's review.
    pub struct SkillDraft {
        pub id: String,
        pub name: String,
        pub description: String,
        pub created_at: String,
        pub hash: String,
        /// The session that proposed it read untrusted text.
        pub tainted: bool,
        pub warnings: Vec<String>,
        pub nearest: Option<String>,
    }

    /// A skill the curator moved to the archive.
    pub struct ArchivedSkill {
        pub skill: String,
        pub entry: u64,
        pub at: String,
        pub reason: String,
    }

    /// `GET /skills`.
    pub struct SkillsState {
        pub skills: Vec<SkillRow>,
        pub drafts: Vec<SkillDraft>,
        pub archived: Vec<ArchivedSkill>,
        pub notes: Vec<String>,
    }

    /// One connected tool server of `GET /tools`. `trust` is `trusted`,
    /// `changed`, `not_trusted` or `removed` (revoked, or approved but no
    /// longer configured); `state` is the sentence `tole plugin list` prints.
    pub struct ToolServer {
        pub trust: String,
        pub name: String,
        pub state: String,
        pub tier: String,
        pub source: Option<String>,
        pub version: Option<String>,
        pub approved_at: Option<String>,
        pub secrets: Vec<String>,
        pub sandbox: String,
        pub manifest_hash: Option<String>,
        pub lock: String,
    }

    /// `GET /tools`. `probed` is always false: the live manifest check starts
    /// the server and is done at a terminal with `tole plugin list`.
    pub struct ToolsState {
        pub servers: Vec<ToolServer>,
        pub notes: Vec<String>,
        pub probed: bool,
    }

    /// The answer to `POST /discussions/{id}/resume`.
    pub struct DiscussionResumed {
        pub id: String,
        pub status: String,
    }

    /// The answer to `POST /discussions/{id}/stop`. `state` is `closed` once
    /// the report is written, `running` while its thread is still closing it.
    pub struct DiscussionStopped {
        pub id: String,
        pub outcome: String,
        pub state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub note: Option<String>,
    }

    /// The body of `POST /discussions/estimate`: what the owner types for
    /// `tole discuss start`. Participants are profile names of the server
    /// owner's user config; `files` are paths inside the server's workspace.
    pub struct DiscussionRequest {
        pub question: String,
        pub criteria: Vec<String>,
        pub participants: Vec<String>,
        pub budget_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_rounds: Option<u32>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub id: Option<String>,
    }

    /// The answer to the estimate: the cost plan as text and as numbers.
    pub struct DiscussionEstimate {
        pub plan: String,
        pub advice: Vec<String>,
        pub participants: Vec<String>,
        pub max_rounds: u32,
        pub max_calls: u64,
        pub budget_tokens: u64,
        pub round_floor_tokens: u64,
        pub files: Vec<String>,
    }
}

/// The verdict of `POST /approvals/{id}/decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
}

/// The bytes of an uploaded image and the type the server recorded for it
/// (`GET /sessions/{id}/images/{hash}` answers the image itself, not JSON, so
/// this is not a wire type of the schema).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub mime: String,
    pub data: Vec<u8>,
}

impl DecisionRequest {
    /// The body for `approval_id`. Serve's own rows need only the verdict; a
    /// delegated child's (`dap-*`) allow also carries `confirm` set to the id,
    /// and the row's `fingerprint` when it has one.
    pub fn new(approval_id: &str, decision: Decision, fingerprint: Option<&str>) -> Self {
        let delegated = approval_id.starts_with("dap-");
        Self {
            decision,
            confirm: (delegated && decision == Decision::Allow).then(|| approval_id.to_string()),
            fingerprint: fingerprint.filter(|_| delegated).map(str::to_string),
        }
    }
}

impl NewSession {
    /// Reopen an existing session.
    pub fn resume(id: impl Into<String>) -> Self {
        Self {
            resume: Some(id.into()),
            ..Self::default()
        }
    }
}

impl InterruptRequest {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            urgent: None,
        }
    }
}

impl ForkRequest {
    /// Cut at the entry with this `seq`.
    pub fn at(seq: u64) -> Self {
        Self { at: Some(seq) }
    }
}

impl NameRequest {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl ModelRequest {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }
}

impl EffortRequest {
    /// Set a level, or clear the override with `None`.
    pub fn new(effort: Option<&str>) -> Self {
        Self {
            effort: effort.map(str::to_string),
        }
    }
}

impl MissionRequest {
    /// A goal with the owner's default budgets.
    pub fn goal(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            max_steps: None,
            max_minutes: None,
            max_tokens: None,
        }
    }
}

impl PromptRequest {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_approval_row_decodes_its_summary_and_keeps_an_unknown_code() {
        let row = json!({
            "id": "apr-1", "tool": "write_file", "status": "pending",
            "summary": {
                "kind": "from_a_newer_server", "target": "n.md", "effect": "adds",
                "insideWorkspace": true, "targetTruncated": false,
                "preview": ["+ hi"], "previewTruncated": false
            }
        });
        let a: Approval = serde_json::from_value(row.clone()).unwrap();
        let s = a.summary.as_ref().expect("a summary");
        assert_eq!(
            (s.kind.as_str(), s.target.as_str()),
            ("from_a_newer_server", "n.md")
        );
        assert!(s.inside_workspace && !s.target_truncated && !s.preview_truncated);
        assert_eq!(s.preview, vec!["+ hi".to_string()]);
        // The summary is a typed field now, not part of `extra`, and the row
        // serializes back to the JSON the server sent.
        assert!(a.extra.is_empty(), "{:?}", a.extra);
        assert_eq!(serde_json::to_value(&a).unwrap(), row);
    }

    #[test]
    fn an_approval_row_without_a_summary_has_none_and_no_key() {
        let row = json!({"id": "apr-1", "tool": "write_file", "status": "pending"});
        let a: Approval = serde_json::from_value(row.clone()).unwrap();
        assert!(a.summary.is_none());
        assert_eq!(serde_json::to_value(&a).unwrap(), row);
    }

    #[test]
    fn a_decision_body_carries_confirm_and_fingerprint_only_for_delegated_rows() {
        let body = |id, d, f| serde_json::to_value(DecisionRequest::new(id, d, f)).unwrap();
        // Serve's own queue: the verdict only (an unknown key would be noise).
        assert_eq!(
            body("apr-1", Decision::Allow, Some("fp")),
            json!({"decision": "allow"})
        );
        // A delegated child: the exact keys serve_delegation::decide_route takes.
        assert_eq!(
            body("dap-9", Decision::Allow, Some("fp")),
            json!({"decision": "allow", "confirm": "dap-9", "fingerprint": "fp"})
        );
        assert_eq!(
            body("dap-9", Decision::Allow, None),
            json!({"decision": "allow", "confirm": "dap-9"})
        );
        assert_eq!(
            body("dap-9", Decision::Deny, None),
            json!({"decision": "deny"})
        );
    }
}
