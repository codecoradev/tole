# Outbound signed webhook delivery — design note

Status: **proposal, nothing implemented.** Issue #911, part of the gateway epic
#910 (step G0b). Extends [automation-design.md](automation-design.md) §9
(delivery without channels) and mirrors the inbound gate of §6 (#582).

**Owner decisions of 2026-11-17 (fixed, not reopened here):** a named
destination lives in the **user config only**; a cron job refers to it by name
(`deliver = ["webhook:gateway"]`); events are a finished cron run and
`approval_requested` / `approval_resolved`; the payload goes through
`SecretSet::redact`, is size-capped and carries a stable event id; HMAC-SHA256
over timestamp + body like `POST /hooks/<route>`; at-least-once with bounded
retries, then a ledger line and a doctor warning; delivery never blocks a turn
or the cron tick; only configured destinations are reachable, loopback is an
explicit opt-in, redirects are not followed, no private range unless the
destination opts in.

**Why:** a chat gateway (separate process and repository) must learn about
cron results and approval requests without holding SSE streams (serve caps
them at `MAX_STREAMS = 8`, `serve_support/events.rs`) and without polling.
The webhook **only notifies**. It never carries a credential, never grants
anything, and the answer to an approval still arrives through
`POST /approvals/{id}/decision`.

## 1. Config: a named destination (user layer only)

```toml
[webhook_out.gateway]
url             = "http://127.0.0.1:8787/tole/events"   # http only with allow_loopback
secret_env      = "TOLE_GATEWAY_SECRET"                 # or secret_file = "/abs/path"
events          = ["cron_run_finished", "approval_requested", "approval_resolved"]
allow_loopback  = true     # default false
allow_private   = false    # default false (RFC 1918, ULA, link-local, metadata ...)
allow_agent_jobs = false   # default false, see below
timeout_secs    = 10       # default 10, 1..=30
```

- **Name:** `[a-z0-9_-]{1,32}`. Section is `webhook_out`, not `hooks`, so the
  inbound table (`HookConfig`, `config/mod.rs`) keeps its meaning.
- **Secret source:** the same two sources and checks as `[hooks.*]` (`hmac_key_env`
  / `hmac_key_file`: exactly one, at least 16 bytes, a regular file of at most
  4 KiB, no link, no FIFO). Never inline, never shown (not in errors, `config
  check`, doctor, logs, ledger). The loader reuses `hooks::read_key_file` (move it
  to a small shared helper rather than copy it) and registers the key with
  `secrets::add_role_secret("webhook_out.<name>", key)`, exactly like
  `hooks::hold_keys`, so redaction and the child env scrub cover it.
- **`events`** defaults to `["cron_run_finished"]`. Approval events are opt-in:
  a destination that does not list them never hears about approvals.
- **User layer only.** Add `webhook_out` to the keys `layered()` takes from the
  user config alone, and print a warning (like `hooks_ignored_warning`) when a
  project config carries it; `config check` marks it IGNORED. A cloned
  repository can neither open nor redirect an outbound path.
- **Load errors stop `tole serve` before it listens** (like a bad hook route),
  and fail `tole cron create` / `tole doctor` with the destination name and the
  fix: unknown field (`deny_unknown_fields`), no or two secret sources, short
  key, bad URL, a non-`https` URL without `allow_loopback` for a loopback host
  (see §5), an unknown event name.
- **A cron job names it** as a `deliver` entry `webhook:<name>`:
  `Target::Webhook(String)` in `cron/deliver.rs` (`parse`, `spec`, `validate`).
  `validate` checks shape only (the name charset); `check()` (pre-dispatch,
  P10) resolves the name in the user config and blocks the occurrence with
  `delivery_invalid` when it does not exist or does not list
  `cron_run_finished`.
- **Trusted-job rule.** `command:` is trust-gated because it runs code. A
  webhook runs no code and can only reach a destination the owner declared, so
  any CLI-created job may use it. An **agent-created** job (`created_by != cli`)
  may name a destination only when that destination sets `allow_agent_jobs =
  true` (default false): otherwise it is `blocked` with `delivery_untrusted`,
  the same code and place as `command:`. Reason: an agent can put anything it
  read into a job's output, so letting it pick a third-party sink by default
  would be an exfiltration channel the owner never opted into.
- Agent-written text (a job's prompt, a tool result, a skill) has no field that
  names a URL, a header or a secret. The only agent-reachable input is the
  destination **name**, and only under the rule above.

Code: `crates/tole-cli/src/webhook_out/{mod,config,payload,outbox,worker}.rs`
(new), `config/mod.rs` (struct + user-only layering), `cron/deliver.rs`
(`Target`), `doctor.rs`.

## 2. Events and payload (schema v1)

Three event types (the `type`, also sent as `X-Tole-Event`, which already
passes `webhook::valid_event`):

| type | emitted when | to |
|---|---|---|
| `cron_run_finished` | a run settles (any status, silent output excluded, see below) | destinations a job names in `deliver` |
| `approval_requested` | `ApprovalQueue::queue_entry` adds a pending entry | destinations listing it in `events` |
| `approval_resolved` | an entry leaves `Pending`: allow, deny, or expiry | destinations listing it in `events` |

Envelope (one JSON object, UTF-8, at most **16 KiB** after serialization):

```json
{
  "v": 1,
  "id": "cron:fire-abc123",
  "type": "cron_run_finished",
  "destination": "gateway",
  "at": 1795000000,
  "data": { "...": "per type, below" }
}
```

- **`id` is stable and deterministic**, the idempotency key: `cron:<fire_id>`,
  `approval:<approval_id>:requested`, `approval:<approval_id>:resolved`.
  Deterministic so a re-enqueue after a crash produces the same id; the
  charset is within `webhook::valid_delivery` (`[A-Za-z0-9._:-]`, at most 128).
  It is sent as `X-Tole-Delivery` too.
- **`v`** is the schema version. Additive fields do not bump it; a removed or
  re-typed field does. A receiver ignores unknown fields and rejects an unknown
  `v`.
- **`destination`** is inside the signed body, so a capture cannot be replayed
  to another destination that happens to share a secret.
- **`cron_run_finished.data`:** `job` (id), `job_name`, `fire_id`, `status`
  (`RunStatus`, snake_case), `scheduled_for`, `finished_at`, `summary`
  (redacted output, at most **4 KiB**, cut at a character boundary after
  redaction), `truncated` (bool). A silent run (`deliver::silent`) sends
  nothing, as every target does today (`skipped` is recorded).
- **`approval_requested.data`:** `approval_id`, `session_id`, `tool`, `risk`
  (`"write"`: only non-ReadOnly, non-Destructive calls reach the queue;
  Destructive tools are not registered for a non-interactive approver),
  `summary` (the entry's `description`, redacted, at most **512 bytes**),
  `created_at`, `expires_at` (`EXPIRY_SECS = 900`).
- **`approval_resolved.data`:** `approval_id`, `session_id`, `tool`, `status`
  (`approved` | `denied` | `expired`), `resolved_at`. No `decided_by`: serve has
  one credential and no identity (threat model, serve auth row).
- Fields that would not fit are cut, never the envelope: if the body is still
  over 16 KiB after the caps above, `summary` is dropped and `truncated` is
  set.

## 3. Signature (mirror of the inbound gate)

The sender reuses `tole_core::webhook::sign` (`ledger/webhook.rs`, native v2)
unchanged, so inbound and outbound cannot drift:

```
POST <url>
Content-Type: application/json
X-Tole-Timestamp: <unix seconds, taken at THIS attempt>
X-Tole-Delivery:  <envelope id>
X-Tole-Event:     <envelope type>
X-Tole-Signature: v2=<hex HMAC-SHA256(secret, "v2\n<ts>\n<id>\n<event>\n<body>")>
User-Agent:       tole/<version>
```

- The body bytes are fixed at enqueue time and identical on every attempt; only
  the timestamp and signature are recomputed per attempt. A retry minutes later
  therefore still verifies inside the receiver's window, while the id stays the
  same.
- **The receiver must:** (1) read the raw body (cap 64 KiB) before parsing;
  (2) require all four headers; (3) recompute the HMAC over exactly
  `v2\n<ts>\n<id>\n<event>\n<raw body>` and compare in constant time;
  (4) reject a timestamp more than **300 s** from its own clock
  (`WINDOW_SECS`); (5) check `X-Tole-Event == body.type`, `X-Tole-Delivery ==
  body.id` and `body.destination` is its own name; (6) dedupe on `id` and keep
  ids for at least **24 h** (longer than the sender's retry horizon, §4); (7)
  treat an unknown `v` as an error it logs, not a crash; (8) answer 2xx only
  after it has durably accepted the event; (9) use a secret not shared with any
  other destination or inbound route.
- The docs ship a **test vector** (key, timestamp, id, event, body, expected
  `v2=` value) so the gateway repository, which does not link `tole-core`, can
  pin its verifier to the same bytes. A unit test in `tole-core` checks the
  vector against `webhook::sign`.
- No GitHub-compat scheme outbound; no v1.

## 4. Delivery semantics

**At-least-once, no ordering, never blocks.**

- **Where it runs.** One long-lived **sender** (a thread per `tole serve`
  process, started next to `serve_ticker`, `serve.rs:187`; ureq is blocking and
  serve is thread-based, there is no async runtime to join). Producers never do
  network I/O: they redact, serialize, cap, and `enqueue` (a bounded channel
  send or one appended line) and return. The cron tick's `deliver()` loop
  (`cron/tick.rs`, runs on the tick thread without the ledger lock) therefore
  gains no latency, and `QueueApprover::decide` (called on the turn's thread)
  cannot be slowed by a dead receiver.
- **Queue bound.** 256 pending events per destination in memory. When full,
  the new event is dropped, counted, and recorded as failed with reason
  `queue_full`; nothing blocks. A single in-flight request per destination, so
  one slow receiver cannot spawn threads or sockets.
- **Retry.** Up to **6 attempts**; waits 2 s, 10 s, 30 s, 2 min, 10 min (each
  +/- 20 % jitter). A `Retry-After` on 429/503 is honoured, capped at 10 min.
  Per-event deadline: `cron_run_finished` 6 h, approval events until the
  approval's own expiry (900 s), after which the event is `expired`, not sent
  (a stale `approval_requested` is worse than none). Retry on: connect or TLS
  error, timeout, 5xx, 408, 429. **No retry** (terminal failure) on any other 4xx
  (the receiver understood and refused), on a 3xx (redirects are never
  followed), on a response over the size cap, and on an unresolvable or
  policy-refused address.
- **Ordering: none guaranteed.** The sender is FIFO per destination, but a
  failed event is rescheduled while later events go on, so `approval_resolved`
  can arrive before `approval_requested`. Receivers key state by
  `approval_id` / `fire_id`; `resolved` wins over `requested` whatever the
  arrival order. Rationale: head-of-line blocking would let one poisoned event
  stall every approval.
- **Idempotency key:** the envelope `id` (also `X-Tole-Delivery`). A receiver
  that processes an id twice is the receiver's bug; the sender may send it more
  than once (after a lost 2xx, or a crash between send and record).
- **Durable outbox for cron results, volatile queue for approvals
  (recommended).**
  - `cron_run_finished` is appended to `<cron dir>/outbox.jsonl` (append-only
    JSONL, mode 0600, the redacted body stored once so every attempt is
    byte-identical) with lines `enqueue`, `attempt`, `done`, `dead`. Pending =
    enqueued, neither done nor dead, not past its deadline; a fold at startup
    resumes them. Needed because `tole cron tick` can be a separate short-lived
    process (system crontab, design §4) that exits right after the tick: it
    only appends; the sender in `tole serve` drains it, and `tole cron tick`
    drains it itself for at most 20 s at the end of a tick if no serve holds
    the drain lock (a lock file next to the ledger, `store::with_lock` style).
    Two drainers are harmless anyway because of the idempotency key.
  - Approval events are **not** persisted. `ApprovalQueue` is in memory: after
    a `tole serve` restart every pending approval is already gone (denied), so
    replaying `approval_requested` would announce something that cannot be
    decided. The gateway reconciles with `GET /approvals` on start.
  - Why not best-effort for everything: a cron result is the only record the
    owner gets of an unattended run, and a restart mid-retry (deploy, crash) is
    exactly when it matters. Why not durable for everything: it would persist
    dead approvals, and one mechanism per event class costs less than a
    resurrection filter.
  - The file grows by one `enqueue` plus at most 6 `attempt` and one terminal
    line per event and is not rotated (same accepted residual as `hook_seen`,
    1 line per event; open question 6).
- **Ledger.** On a terminal state a `cron_run_finished` event writes the normal
  `delivery` record (`fire_id`, `target = "webhook:gateway"`, `attempt` =
  attempts made, `status` = `sent` | `failed`, `error` = a short code such as
  `http_500`, `timeout`, `refused_address`, `queue_full`, `expired`; never the
  URL, a header, or any response byte), through `l.deliver()` as `tick.rs` does
  for the other targets. A failed delivery never changes the run status. Approval
  events have no `fire_id`; their terminal state is the outbox/queue counters
  and a stderr line from serve (`tole serve: webhook gateway: approval event
  dropped (http_500)`), with the same redacted code.
- **Doctor.** `tole doctor` lists each destination (name, host:port only, events,
  loopback/private flags, never the URL path or query or the secret source),
  warns on: a failed or dead event in the last 24 h (count and last code), a
  non-empty pending outbox older than 10 min, a destination whose URL is plain
  `http` to a non-loopback host (cannot load, shown as an error), and a
  `webhook:<name>` job whose destination no longer exists.
- **Shutdown.** The sender is not joined on exit. Pending cron events are
  already on disk; an attempt in flight is abandoned (the receiver may have
  seen it, so it is re-sent, which at-least-once allows). Volatile approval
  events die with the process, like the approvals themselves.

## 5. Network policy and SSRF

- **Reachable set = the configured destinations.** The request URL is the
  configured string and nothing else; no event field, header value or response
  is ever used to build another URL.
- **HTTP client: ureq 3, already in the tree.** `tole-core/src/web.rs` already
  has the hardened building blocks for `web_fetch`: `SsrfResolver` (vet-and-pin
  in the resolver: the addresses vetted are the addresses connected, so there
  is no check-then-resolve rebinding window), `ip_is_private` (v4 and every
  v4-in-v6 form), `max_redirects(0)`, `proxy(None)`, bounded body reads, and
  rustls with the `ring` provider (no aws-lc). Reuse them; add **no new
  dependency**. `vetted_get` is GET-only, so add a sibling `vetted_post` in
  `web.rs` (same agent construction) and change the resolver policy from the
  current all-or-nothing `opt_out: bool` to `AddrPolicy { loopback: bool,
  private: bool }`. `web_fetch` keeps its existing behaviour (`TOLE_WEB_ALLOW_PRIVATE`
  maps to both true). The env proxy stays off: through a proxy the resolver only
  vets the proxy host (the cora finding already recorded in `vetted_config`).
- **Address policy per destination.** Loopback addresses need `allow_loopback`;
  every other non-public range (RFC 1918, CGNAT, link-local incl. the cloud
  metadata address, ULA, multicast, the 6to4/Teredo/NAT64 embeddings) needs
  `allow_private`. The policy is applied inside the connection's resolver, on
  every attempt, so a hostname that resolves to a public address at load time
  and to `169.254.169.254` at attempt time is refused (DNS rebinding), and a
  host with several addresses is refused if **any** is out of policy.
- **Redirects:** never followed (`max_redirects(0)`, `http_status_as_error
  (false)`); a 3xx is a terminal failure `redirect_refused`. The `Location`
  value is not read.
- **TLS:** `https` is required for any non-loopback host, validated against
  the webpki roots ureq/rustls ships; no option to skip verification, no custom
  CA in v1. Plain `http` is accepted only when the host resolves to loopback
  and `allow_loopback = true` (a local gateway). `allow_private` does not
  relax this: a private-network receiver still needs TLS (open question 5).
- **Timeouts and size:** connect 5 s, whole request `timeout_secs` (default 10,
  at most 30), request body at most 16 KiB, response body read at most 4 KiB and
  then discarded; only the status code is used. Response bodies and headers are
  never logged or recorded (a receiver controls them: log injection, secret
  echo).
- **No cookies, no auth other than the signature,** no connection reuse across
  destinations. One in-flight request per destination.
- **User-Agent only identifies tole** (`tole/<version>`): no host, user or
  session data.

## 6. Redaction and what is never sent

- **Order (same as hooks templates):** build the typed payload, redact **every
  string value** with `SecretSet::redact_value` using the process set
  (`secrets::process_secrets()`, which includes the destination secret by role),
  **then** apply the caps, so a cap never leaves the head of a secret. Redact
  again over the serialized envelope as a belt-and-braces pass; both passes are
  idempotent. Shape masking (#597) applies as in every sink.
- **Never sent:** tool arguments or inputs, file contents, command lines
  beyond the 512-byte redacted `description`, environment, session transcript,
  provider key or base URL, model output other than the capped cron summary,
  the destination's own URL, paths of the user's home or sessions dir.
  `approval_requested` is deliberately a **pointer**: id, session, tool, risk,
  short summary. The gateway shows the owner full redacted detail by calling
  `GET /approvals` itself, over the authenticated serve API, which is the
  channel that already carries that detail.
- **Why a summary at all, and why 512 bytes:** the owner needs to recognise the
  request in a notification; `description` is already redacted text produced by
  the approver and already returned by `GET /approvals`. It is display text
  that came from tool input, so it is **untrusted**: the receiver must render
  it as data (escape for its chat markup) and never parse it.
- **Agent-written text can never choose or change a destination:** destinations
  exist only in the user config; the payload, the job text and tool output have
  no path into URL, headers, secret or event routing (events are routed by type
  and by the job's `deliver` list, which only names a destination).

## 7. Security properties and threats

| Threat | What it can and cannot cause | Mitigation |
|---|---|---|
| Attacker controls a web page or tool output (prompt injection) | Can shape a cron run's output, hence the 4 KiB `summary`, and the text of an approval `summary`. Cannot pick a URL, header, secret, event type or destination, cannot make a webhook fire for a job that does not name it, cannot reach an internal host. | Destinations user-config only; payload fields are typed, strings redacted and capped; receiver treats text as data (documented duty). |
| Attacker controls a project config / cloned repo | Cannot define, change or redirect a destination (`webhook_out` ignored with a warning), cannot add `deliver` entries to an existing job. A repo-supplied job file does not exist in the cron ledger. | User-layer-only key, warning, `config check` IGNORED. |
| Agent creates a cron job naming a destination | Can only name an existing destination, and only when `allow_agent_jobs = true`; the job is also parked until the owner enables it (#640). | Trusted-job rule, default deny; payload redaction; per-destination `events`. |
| SSRF via the destination (DNS rebinding, redirect to metadata, IPv6 tricks) | Out of reach by default: private and loopback ranges refused per attempt in the resolver; redirects not followed; env proxy off. | `AddrPolicy` in `SsrfResolver` (vet-and-pin), `max_redirects(0)`, tests for rebinding and v4-in-v6 forms. |
| Compromised or malicious receiver | Receives only what the owner chose to send: redacted summaries and approval pointers. Cannot approve anything (no credential, no token in the payload), cannot make tole connect elsewhere, cannot inject into tole through the response (4 KiB, discarded, status only). Can drop events and lie about success. | Notify-only design; payload minimisation; response never parsed or logged; `doctor` shows failures. A secret per destination limits blast radius if the receiver leaks it. |
| Replay of a captured delivery | Within 300 s the same `v2` message verifies; after, the timestamp fails. A relabelled event or another destination fails the signature (event, id and destination are signed). | v2 scheme, receiver duty (window + dedupe on `id` for 24 h). A replay alone cannot grant an approval: decisions only arrive on serve with its own credential. |
| Secret leakage through retries or the outbox | The destination secret is a role secret, so it is redacted wherever it appears; the outbox stores the already-redacted body only (0600); retries re-sign, they never re-serialize. A value added to the secret set **after** enqueue is not re-redacted in a stored body. | `add_role_secret`; redact before storing; no URL, header or response in the ledger, logs or doctor; leak check (§9). |
| Log injection (receiver-controlled text into tole logs) | None: response bytes never reach a log; ledger `error` is a fixed code from a closed set; stderr lines use the destination name (charset-restricted) and the same code. | Closed error vocabulary. |
| Flooding the receiver / being flooded by our own events | Bounded: one request in flight per destination, queue of 256, at most 6 attempts per event, approval events limited by the approval queue itself. | Queue bound, `queue_full` record, doctor warning. |
| A notification mistaken for authority | A webhook can say "approval X is pending"; nothing in it executes or grants. | §8. |
| Destination secret reused as inbound hook key | Cross-protocol confusion. | Loader refuses a destination whose key equals any `[hooks.*]` key held in the process; docs say one secret per purpose. |

## 8. The single approval gate

- The webhook is a **notification**. It never carries the decision endpoint's
  credential and no approval link or token; `POST /approvals/{id}/decision`
  with the serve credential stays the only way to allow or deny.
- Emission points: `ApprovalQueue::queue_entry` (requested) and every
  `Pending -> Approved | Denied | Expired` transition. `prune_expired` already
  returns the ids it expires; `decide` must report whether it **transitioned**
  the entry (today a repeat decision returns the stored status), so a repeat
  emits nothing. The queue gets an optional `Arc<dyn Fn(ApprovalEvent) + Send +
  Sync>` notifier set by serve; the closure only enqueues (never blocks, never
  fails the approval path). When no destination lists approval events the
  closure is absent and the path is unchanged.
- An emission failure never changes an approval, never retries a decision,
  and never delays the fail-closed `Verdict::Deny` of `QueueApprover::decide`.
- Unanswered means denied (`EXPIRY_SECS`) whether or not the receiver was told.
- Not in v1: delegated-child approvals (`dap-*`, durable log, §13 of the
  automation note; open question 2).
- Design rules kept: `Destructive` is unaffected (never queued through serve,
  never allowlistable); no new approval path, no bypass; `tole-core` stays
  platform-agnostic (the sender, config, outbox and notifier live in
  `tole-cli`; only `vetted_post` and `AddrPolicy` are in `tole-core/web.rs`,
  which already owns the SSRF code).

## 9. Test plan, PR split, review

**Unit:** config loader (every refusal in §1, user-only layering, project
warning, secret never in `Debug`/errors); `Target::parse`/`check` including the
agent-job rule and unknown destination; payload builders (caps, truncation at
char boundary after redaction, 16 KiB envelope, deterministic ids, `summary`
dropped when over cap); signing against the shared **test vector** and against
`webhook::verify`; `AddrPolicy` table (loopback only with flag, every private
form incl. v4-in-v6, rebinding stub resolver, multi-address host with one bad
address); redirect refused; response over 4 KiB; retry schedule, `Retry-After`
cap, 4xx-no-retry, deadline expiry (injected clock); outbox fold (torn tail,
resume, done/dead, expired); queue bound.

**Real-binary e2e** (`crates/tole-cli/tests`, built `./target/release/tole`, a
local receiver thread that verifies the v2 signature with an independent
implementation, checks the window and dedupes): cron job with `deliver =
["webhook:gateway"]` produces exactly one verified delivery with the expected
payload; approval parked in `tole serve` yields `approval_requested`, a
decision yields `approval_resolved`, expiry yields `approval_resolved
expired`. **Failure injection:** receiver returns 500 then 200 (retry, same id,
new timestamp, same body); returns 400 (no retry, ledger `failed`); never
answers (timeout, the tick and an approval turn are not delayed beyond a small
bound, asserted); closed port; `serve` killed mid-retry and restarted (the cron
event arrives once or twice, same id; the approval event does not); queue full;
a destination whose host resolves to a private address without the flag is
refused; 302 is not followed (the redirect target records no hit).
**Leak check (#781):** run with a canary secret in the environment, in a tool
result, in the job prompt and in the approval description; assert the canary and
the destination secret appear in neither the received bodies, the outbox, the
ledger, doctor output, stderr, nor `config check`.

**Estimated PRs (each under ~600 changed code lines, tests included in the
count; docs not counted):**

1. `refactor(web)` / `feat(core)`: `AddrPolicy` + `vetted_post` in
   `tole-core/web.rs`, `web_fetch` unchanged in behaviour; SSRF and redirect
   tests (~450). **Independent review** (outbound path).
2. `feat(cli)`: `[webhook_out]` config, user-only layering, secret loading
   and role secret, `Target::Webhook`, `check()` rules, `config check`, doctor
   config lines; no sending yet (~500).
3. `feat(cron)`: payload, signing, outbox, sender thread, retry, cron
   integration in `tick.rs`, ledger `delivery`, doctor warnings, test vector and
   most unit tests (~580). **Independent review** (redaction, outbox, data path).
4. `feat(serve)`: approval notifier (`approvals.rs`, `serve.rs`), approval
   events, e2e suite with failure injection and the leak check, threat-model
   rows, README status line and `docs/epics.md` in the same PR (~450).
   **Independent review** (touches the approval path).

Each PR body carries `cora callers` / `cora impact` output for the touched
public items (`ApprovalQueue`, `Target`, `deliver::deliver`, `web.rs` fns),
verified by grep as AGENTS.md §4 requires. `tole-core` additions are
`pub(crate)` where possible: `tole-core` is published, so a public item added
is a semver commitment.

## 10. Open questions for the owner

1. **Durable outbox for cron results only, volatile for approvals** (§4).
   Recommendation: yes. Alternative: best-effort for all (smaller, loses
   results on restart) or durable for all (resurrects dead approvals).
2. **Delegation approvals (`dap-*`) in v1?** Recommendation: no; they use a
   durable log and a separate route, add them once the gateway needs them.
3. **Approval `summary` text (512 B, redacted) or ids only?** Recommendation:
   include the summary, so a notification is recognisable even if the gateway
   cannot reach serve at that moment; it is already exposed by `GET /approvals`.
4. **Agent-created jobs may use a destination?** Recommendation: default no
   (`allow_agent_jobs = false`), per destination, so a personal gateway can opt
   in deliberately.
5. **`allow_private` without TLS** (a receiver on a LAN)? Recommendation: no;
   require `https` for everything but loopback. Revisit with a custom CA option
   only if a real setup needs it.
6. **Outbox rotation.** Recommendation: none now, as for `hook_seen`; record
   it as an accepted residual in the threat model (one enqueue and at most 7
   lines per event).
7. **Secret rotation without a gap.** Recommendation: single secret per
   destination in v1, rotate by editing and restarting; add an optional
   `secret_env_next` (sign with the first, receiver accepts either) only on
   demand.
8. **`tole cron tick` drains the outbox for up to 20 s** when no `tole serve`
   does. Recommendation: yes, otherwise a crontab-only setup never delivers.
9. **Environment proxy support** for the sender. Recommendation: stay off, as
   `web_fetch`; a proxied sender would vet the proxy, not the destination.
10. **Event set growth** (cron failures only, approval-pending reminders,
    session-turn finished). Recommendation: out of scope; the type list is
    versioned and additive.
