# Outbound signed webhook delivery — design note

Status: **in progress.** Built: `vetted_post` and the address policy (#919),
the `[webhook_out]` config, its user-only layering and the `webhook:<name>`
cron target, which fails closed with `not_implemented` until the sender lands
(section 9, PR 2). Not built: the event ids and payloads, the outbox, the
sender, the approval events and `tole webhook test`. Issue #911, part of the
gateway epic #910 (step G0b). Extends [automation-design.md](automation-design.md) §9
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
hmac_key_env      = "TOLE_GATEWAY_SECRET"                 # or hmac_key_file = "/abs/path"
events          = ["cron_run_finished", "approval_requested", "approval_resolved"]
allow_loopback  = true     # default false; literal loopback IP or `localhost` hosts only
allow_private   = false    # default false (RFC 1918, ULA, link-local, metadata ...)
allow_agent_jobs = false   # default false, see below
timeout_secs    = 10       # default 10, 1..=30
```

- **Name:** `[a-z0-9_-]{1,32}`. Section is `webhook_out`, not `hooks`, so the
  inbound table (`HookConfig`, `config/mod.rs`) keeps its meaning.
- **Key names:** `hmac_key_env` and `hmac_key_file`, the names `[hooks.*]` uses.
  The first draft said `secret_env` / `secret_file`, but the config parser
  refuses every key that contains `secret`, `password`, `token` or `api_key`
  (a secret must never live in a config file), so those names cannot load.
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
  `validate` checks shape only (the name charset). `check()` (pre-dispatch,
  P10) is pure and runs **under the host lock** (`claim_one`, `tick.rs` ~188, and
  `resume_parked`, ~321), so it must not read config or a key file there:
  it takes a **preloaded destination map** carried in `TickOptions` as an
  `Option<Arc<Destinations>>` (name -> events, flags, `allow_agent_jobs`; no
  secret). `None` keeps `TickOptions::standalone` (`tick.rs`, ~56) a `const fn`;
  with `None`, a `webhook:` target is blocked `delivery_invalid` (fail closed).
  A long-running `tole serve` keeps the map it loaded until restart, so a
  config edit applies on the next start (doctor says so). The host loads it once
  at start (`tole serve`, `tole cron tick`, `tole cron create`) through the shared
  loader `webhook_out::load`, so a `tole cron tick`-only process loads
  destinations and keys too. It blocks the occurrence with `delivery_invalid`
  when the name does not exist or the destination does not list
  `cron_run_finished`.
- **Agent-facing schema.** The cron tool's `deliver` description
  (`cron/tool.rs`, ~99: "`session` (default), `log` or `file:PATH`") is updated
  to mention `webhook:<name>` (an existing destination; agent jobs need the
  destination's `allow_agent_jobs`), so agents learn the target and its rule.
  `tole cron runs` shows the target as `webhook:<name>`.
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
| `webhook_test` | `tole webhook test <name>` (soon, section 10) | that destination only |

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

- **`id` is stable and deterministic**, the idempotency key, and always inside
  `webhook::valid_delivery` (`[A-Za-z0-9._:-]`, at most 128, `ledger/webhook.rs`).
  Real fire ids are `<job>@<at>/<pid>.<start>` (`ledger/mod.rs`, ~941) and
  `<parked>/resume.<pid>.<start>` (~1168): they hold `@` and `/`, which that
  charset rejects, and a long job id can exceed 128. So:
  - cron: `cron:<readable>:<hash>`, where `<hash>` is the first 16 hex digits of
    SHA-256 over the **full fire id** and `<readable>` is the fire id with every
    byte outside `[A-Za-z0-9._-]` replaced by `-`, cut to 64 characters. At most
    5 + 64 + 1 + 16 = 86 characters. The hash makes it unique; the readable part
    is for humans.
  - Not `safe_name` (`tick.rs`, ~74): it maps every other character to `_`
    (lossy: `a@1/2.3` and `a_1_2.3` give the same name) and has no length cap, so
    two fire ids can collide and an id can be too long.
  - A pure function of the fire id, so it is the same in `tole serve` and in
    `tole cron tick`, on every attempt and after a restart. A resumed run has
    its own fire id and therefore its own event.
  - approval: `approval:<approval_id>:requested` and `:resolved` (approval ids,
    `apr-<hex>-<hex>` and `dap-*`, are inside the charset; the loader rejects an
    id that is not).
  - test event: `test:<16 random hex>`.
  It is sent as `X-Tole-Delivery` too.
- **`v`** is the schema version. Additive fields do not bump it; a removed or
  re-typed field does. A receiver ignores unknown fields and rejects an unknown
  `v`.
- **`destination`** is inside the signed body, so a capture cannot be replayed
  to another destination that happens to share a secret.
- **`cron_run_finished.data`:** `job` (id), `job_name`, `fire_id`, `status`
  (`RunStatus`, snake_case), `scheduled_for`, `finished_at`, `summary`
  (redacted output, at most **4 KiB**, cut at a character boundary after
  redaction), `truncated` (bool), and `session_id` (`Done.session_id`, so the
  gateway can link a run to its session transcript). **Silent rule:**
  `deliver::silent` (`deliver.rs`, ~144) treats empty output as silent for any
  status. For a webhook that applies only to status `ok`; an `error`, `budget`,
  `cancelled`, `interrupted` or `awaiting_approval` run with empty output still
  notifies (empty `summary`), because a failure with no output is what an owner
  most wants to hear about. A first line `[SILENT]` on an `ok` run stays silent
  (`skipped` is recorded). Runs blocked **before** dispatch
  (`delivery_untrusted`, `not_enabled`) never reach a settle; a
  `cron_run_blocked` event for them is reserved (section 10).
- **`approval_requested.data`:** `source` (`"serve"`; `"delegation"` reserved),
  `approval_id`, `session_id`, `tool`, `risk`
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
  body.id` and `body.destination` is its own name; (6) dedupe on `(destination, id)` and keep
  keys for at least **24 h** (longer than the sender's retry horizon, §4); (7)
  treat an unknown `v` as an error it logs, not a crash; (8) answer 2xx only
  after it has durably accepted the event; (9) use a secret not shared with any
  other destination or inbound route.
- The docs ship a **test vector** (key, timestamp, id, event, body, expected
  `v2=` value) so the gateway repository, which does not link `tole-core`, can
  pin its verifier to the same bytes. A unit test in `tole-core` checks the
  vector against `webhook::sign`.
- **Test vector** (computed once with an independent HMAC-SHA256, not with
  this code; pinned by `tole-core/tests/webhook_core.rs` and by
  `webhook_out::sign`). Key (ASCII): `tole-test-key-0123456789abcdef`.
  `X-Tole-Timestamp`: `1795000000`. `X-Tole-Delivery`:
  `cron:daily-1:0123456789abcdef`. `X-Tole-Event`: `cron_run_finished`. Body
  (one line, no trailing newline):

  ```
  {"v":1,"id":"cron:daily-1:0123456789abcdef","type":"cron_run_finished","destination":"gateway","at":1795000000,"data":{"job":"daily","status":"ok","summary":"all good","truncated":false}}
  ```

  Expected `X-Tole-Signature`:
  `v2=9d8c931c19daf1192cfb7509c7f10dd86faf8ff0c938ae22d9ca7d99d8764f62`.
- **Per attempt vs per event:** `X-Tole-Timestamp` (and therefore the
  signature) is regenerated on every attempt; the event id, the body bytes and
  `body.at` are not. A gateway applies the window to the timestamp and the
  dedupe to the id.
- No GitHub-compat scheme outbound; no v1.

## 4. Delivery semantics

**At-least-once, no ordering, never blocks.** Duplicates are possible and
documented: a lost 2xx, a crash between send and record, or two drainers
(below) can each send an event twice. The receiver's dedupe key is
(`destination`, `id`): the id derives from the fire id alone, so a job with
two `webhook:` targets sends the same id to both destinations.

- **Producers never do network I/O.** They redact, serialize, cap and enqueue,
  then return. Two producers of cron events: the tick's settle loop
  (`cron/tick.rs`, ~515, on the tick thread without the ledger lock) and
  `resume_parked` (`tick.rs`, ~321, an approval-resumed run), so both go through
  one `webhook_out::enqueue_cron(...)`, called once per webhook target of the
  job. The approval producer is the notifier of §8, on the turn's thread. A dead
  receiver cannot slow either. **Who records what:** the producer records an
  enqueue refusal itself, as a ledger `delivery` row `failed` with `queue_full`
  or `enqueue_failed` (disk full, I/O error), after its append attempt and
  outside both locks; the sender never sees an event that was not enqueued.
  - **Crash gap and reconcile.** A crash after `Done` is recorded but before
    `enqueue_cron` loses the event with no row. A **reconcile pass** closes it:
    in `tole serve` every 5 min, and at the end of every `tole cron tick` that
    drains (so it also runs in short-lived ticks), it scans the fires that settled
    in the last 6 h (the ledger fold the tick already has) whose job has a webhook
    target and which have **neither** an outbox `enqueue` for that (destination,
    id) **nor a terminal `delivery` row**. A ledger row of any status, including
    `skipped` (a silent run, empty `ok` output), is terminal: otherwise silent
    runs would be re-enqueued every 5 min for 6 h and notify against the silent
    rule. Mechanics, because `enqueue_cron` takes `outbox.lock` blocking on its
    own file descriptor (flock on a second descriptor in the same process would
    self-deadlock): the pass first **collects candidates** (ledger fold, then one
    outbox fold, no lock held), releases everything, and then calls `enqueue_cron`
    for each candidate; `enqueue_cron` does the check-then-append **atomically
    under the outbox lock** (it re-reads the bytes appended since the pass's fold
    and skips an existing (destination, id)). The pass is therefore idempotent
    and never holds the lock while calling it. Cost: the ledger fold already
    done under `jobs.lock` plus one outbox fold (see Growth). The same pass also
    covers the opposite gap (N10 below): an outbox `done`/`dead` with no ledger
    row gets its row written. A run older than 6 h is not announced; `tole cron
    runs` still shows it.
- **The sender.** One long-lived thread per `tole serve` process, started next
  to `serve_ticker` (`serve.rs`, ~187) **whether or not `--cron` is given**: a
  serve without `--cron` still needs it for approval events. ureq is blocking
  and serve is thread-based; there is no async runtime to join. It owns the
  approval timer of §8.
- **Cron events: a durable outbox, which the sender treats as untrusted input.**
  `<cron dir>/outbox.jsonl`: append-only JSONL, mode 0600, one line per
  `enqueue` (destination, id, the redacted body once, deadline), `lease`
  (destination, id, at, `lease_until`), `attempt` (destination, id, **n**, at,
  outcome code, **next_attempt_at**: a result line), `done`, `dead`. Every line
  after `enqueue` is keyed by **(destination, id)**, never by id alone. The
  attempt number `n` is counted **only on `attempt` result lines**, never on
  leases. The body is stored once so every attempt is byte-identical. State survives restart:
  attempt count and next-attempt time are in the file, and the startup fold
  resumes them (pending = enqueued, no `done`/`dead`, before its deadline).
  - *Trust.* 0600 does not stop the same user's interactive agent whose
    workspace contains the data dir (the cron trust check, `cron/trust.rs`,
    guards cron agent jobs only), and the sender signs whatever a line says. So
    before every send the sender re-validates the line: the line is at most
    32 KiB (each read is bounded; a longer line is skipped and counted as
    `dead:bad_line`; a torn tail is ignored until completed); the body parses as
    schema v1 with `v == 1`; `body.destination` equals the line's destination
    **and** a destination in the loaded user config; `body.type` is in that
    destination's `events`; `body.id` equals the line's id and passes
    `webhook::valid_delivery`; the body is at most 16 KiB; `body.at` is not in
    the future or older than the 6 h deadline. A line that fails is `dead` with a
    code, never sent. Residual, stated plainly: a process running as the owner
    can still append a well-formed event for a configured destination (it can
    equally edit the user config); validation bounds *what* it can make the
    sender sign, not *whether* the owner's own account can.
  - *Multi-process and locks.* `tole cron tick` can be a separate short-lived
    process (system crontab, design §4). A `tole serve` sender notices new lines
    by checking the file length every **5 s** (one `stat`, reading only the tail
    from its remembered offset, as `serve_support/events.rs` does). When no serve
    is running, `tole cron tick` drains for at most **20 s** at the end of a tick.
    One lock, **`<cron dir>/outbox.lock`**, an advisory file lock (`std::fs::File`
    locks, released by the OS when the process dies, so there is no stale lock):
    - **Every append** (cron tick, serve, `resume_parked`, the sender's own
      `lease`/`attempt`/`done`/`dead` lines) takes it **blocking** (`lock()`, as
      `with_lock` does on `jobs.lock`, `delegation/ledger_file.rs` ~133) for the
      short append only. **Torn tail:** with several append-only writers, a
      crash mid-append can leave a last line without `\n`, and the next writer's
      line would be glued onto it. Under the lock, before appending, the writer
      checks that the file's last byte is `\n` and, if not, writes `\n` first;
      the fold counts the fragment as one `bad_line` and goes on.
    - **A drain pass** takes it with **`try_lock()`** (as `session/storage.rs`
      ~1121 does) **per pass, not for its lifetime**: under the lock it reads the
      tail, selects **one** due event per destination and appends a `lease` line
      (`lease_until = now + the destination's timeout_secs + 5 s`), then releases
      it. Network I/O happens **without any lock**; the `attempt` result line is
      appended afterwards under a short blocking lock, and only then is the next
      event of that destination leased. Leasing one at a time keeps a lease from
      expiring while its event waits behind others. Immediately before sending,
      the drainer **re-checks that its own lease is still valid** (`now <
      lease_until`); if not, it re-leases or skips. Another drainer skips leased
      events. A pass that does not get the lock does nothing.
    - **Clock and crashes.** Leases use the wall clock. A backward jump is bounded
      by the fold's cap on far-future leases (below); a crash between a lease and
      its result leaves the event stuck for at most the largest timeout + 5 s, then
      it is retried.
    - **Lock order.** The outbox lock and the ledger lock (`jobs.lock`) are
      **never nested**: no path holds one while taking the other (producers
      append after the tick's settle section has released the ledger lock; the
      sender appends its ledger row after releasing the outbox lock), so there is
      no order to invert.
    - Leases narrow double sends; they do not make delivery exactly-once (an
      expired lease, a lost 2xx, or a send racing a restart can repeat an event;
      the receiver dedupes on (destination, id)).
    - **Fold hardening.** The fold ignores a `lease` whose `lease_until` is more
      than now + timeout + 5 s + 60 s of skew ahead, and ignores any `lease`,
      `attempt`, `done` or `dead` line with no prior `enqueue` for that
      (destination, id). Forged `done`/`dead` lines or a far-future lease are an
      **availability** attack by the same account, inside the accepted residual of
      the same-account threat model (above); these two rules are the cheap
      hardening, not a defence.
  - *Bounds.* The in-memory window is 256 pending events per destination
    (id, file offset, next-attempt time); the rest stay on disk and are loaded as
    slots free, so cron events are not dropped at 256. The file side is bounded
    at **10 000** pending events per destination: past that the new event is
    refused by the producer, which records ledger `failed` / `queue_full`.
  - *Growth, no compaction, no checkpoint in v1.* One `enqueue` per event, and per
    attempt one `lease` and one `attempt` line, plus a terminal line (about 150
    lines for an event that retries for the whole 6 h, typically 3). The file is
    **never rewritten, renamed or truncated**: compaction was dropped, because a
    line appended by another process between the compactor's read and the rename
    would land in the old inode and be lost. With no rewrite the outbox is
    **append-only**, as `AGENTS.md` section 6 asks of storage. **Cost:** every
    fold, including each `tole cron tick` (about once a minute), reads the whole
    file: O(file size). It is bounded by a **hard cap of 64 MiB**: past it the
    producer refuses new events (ledger `failed`, `queue_full`) and `tole doctor`
    errors; below it, doctor warns from 32 MiB. A persisted checkpoint offset
    (a cache file the fold resumes from, with a full fold as the fallback when it
    is lost or corrupt) would remove the O(file) cost, but it adds a second file
    and a staleness rule to a path already carrying a lock protocol, so it is
    **not in v1** and is an owner question (section 10, item 12). Growth is a
    documented residual risk in the threat model (like `hook_seen` and the
    ledger). With nothing pending, the owner may move the file aside.
- **Approval events: volatile.** `ApprovalQueue` is in memory: after a serve
  restart every pending approval is gone (denied), so replaying
  `approval_requested` would announce something that cannot be decided. Events
  live in a bounded in-memory queue (256 per destination, newest dropped when
  full, counted). The gateway reconciles with `GET /approvals` at start (and
  after any reconnect).
- **Retry.** Per event, until its deadline. Retry on: connect or TLS error, DNS
  failure, timeout, 5xx, 408, 429. **No retry** on another 4xx (the receiver
  understood and refused), on a 3xx (redirects are never followed), on a
  response over the size cap, and on an address the policy refuses.
  - `cron_run_finished`: waits 2 s, 10 s, 30 s, 2 min between the first
    five attempts (about 2.7 min in all), then **5 min steps** (each +/- 20 %
    jitter) until the 6 h deadline: about 71 more, so at most about 76 `attempt`
    results (one request in flight per destination, so the cost is small). `Retry-After`
    on 429/503 is honoured, capped at 10 min. Deadline passed: `dead:expired`.
    The 24 h dedupe horizon the receiver keeps (section 3) is longer than the
    6 h deadline plus the 300 s window, so a late retry is still recognised.
  - Approval events: 2 s, 10 s, 30 s, 2 min, then stop; also stop at the
    approval's `expires_at` (900 s). A stale `approval_requested` is worse than
    none, so it is `expired`, not sent.
- **Ordering: none guaranteed.** The sender is FIFO per destination, but a
  failed event is rescheduled while later ones go on, and approvals and cron
  events are separate queues. `approval_resolved` can arrive before
  `approval_requested`, and cron results can arrive out of order. Receivers key
  state by `approval_id` / `fire_id`; `resolved` wins whatever the arrival
  order. Head-of-line blocking would let one poisoned event stall everything.
- **Ledger.** `Ledger::deliver` (`ledger/mod.rs`, ~1087) computes `attempt`
  itself (earlier `delivery` records for that fire and target, plus one),
  `deliver::deliver()` returns a `(DeliveryStatus, Option<String>)`, `tick.rs`
  records one row for every target right after the settle, and `DeliveryStatus`
  has no `Pending`. So: the tick and `resume_parked` **skip** webhook targets
  when recording (they call `enqueue_cron` instead) and record **nothing at
  enqueue**; the sender writes the terminal row later (`l.deliver(fire_id,
  "webhook:<name>", sent | failed, code, now)`, under the ledger lock, from any
  process). A crash between the outbox `done` and this ledger row would lose the row, so the
  reconcile pass writes it from the outbox's terminal line (known gap
  otherwise: `tole cron runs` would show nothing for that run). The row's
  `attempt` is therefore 1; the real number of HTTP
  attempts lives in the outbox, not in the ledger. `tole cron runs` shows the
  target as `webhook:<name>` and, until the terminal row exists, nothing for it
  (a delivery in flight is visible in `tole doctor`, not in the ledger). `error`
  is a code from a closed set (`http_<status>` for a terminal or last HTTP
  status such as `http_500`, `timeout`, `dns`, `refused_address`,
  `redirect_refused`, `queue_full`, `enqueue_failed`, `expired`, `bad_line`,
  `bad_destination` (a URL, header or scheme no later attempt can fix),
  `not_implemented`; `transport` and `key_unavailable` appear only on outbox
  `attempt` lines, never in the ledger: a key that stays unavailable is retried
  until the deadline and ends as `expired`, on purpose, so fixing the
  variable or file in time still delivers), never a URL, header or response byte. A failed delivery never changes the run status. The ledger row for a cron event is written only when the event id is the event id of the fire it names (`cron_event_id`); a line that fails this check settles with no row, and a forged first `enqueue` for a predictable id makes the real producer's `enqueue` a no-op (availability, inside the same-account residual). Approval events
  have no `fire_id`: their terminal state is the in-memory counters and one
  stderr line from serve with the same code and the destination name.
- **Doctor.** Lists each destination (name, host:port only, events,
  loopback/private flags; never the path, query or secret source) and warns on:
  failed or dead events in the last 24 h (count, last code); a pending outbox
  line older than 10 min; a `webhook:<name>` job whose destination is gone; a
  non-loopback `http` URL (cannot load, shown as an error). **Connectivity:**
  `tole doctor` makes no network call by default; `tole webhook test <name>`
  (soon, section 10) sends one signed `webhook_test` event and prints the status
  code, so a gateway operator can check the wiring without a cron run.
- **Shutdown.** The sender is not joined on exit. Pending cron events are on
  disk; an attempt in flight is abandoned (the receiver may have seen it, so it
  is re-sent). Volatile approval events die with the process.

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
  third-party crate**. `vetted_get` is GET-only, so add a sibling `vetted_post`
  in `web.rs` (same agent construction) with **its own params struct**
  (`PostParams`: url, body, headers, timeout, response cap, `AddrPolicy`) and
  change the private resolver policy from the all-or-nothing `opt_out: bool` to
  `AddrPolicy { loopback: bool, private: bool }`. **`Hop` is not touched**: it is
  a public struct with public fields of a published crate, so a new field would
  break literal constructors. `web_fetch` behaves as today (`allow_private` and
  `TOLE_WEB_ALLOW_PRIVATE` map to both flags true). **`TOLE_WEB_ALLOW_PRIVATE`
  is ignored for webhook destinations** (it is read where
  `fetch_agent_vetted` builds the resolver, `web.rs` ~258, in
  `allow_private_from_env` ~304 and in the backend fetch tool ~1171): the policy is
  per destination, from the user config, and nothing in the environment can
  widen it. The env proxy stays off: through a proxy the resolver only
  vets the proxy host (the cora finding already recorded in `vetted_config`).
- **Errors the sender can act on.** Today `SsrfResolver` (`web.rs`, ~206-217)
  maps both a policy refusal and a DNS failure to `HostNotFound` and prints the
  host and address with `eprintln!`. For the sender the resolver returns
  **distinct** errors, `refused_address` (terminal) and `dns` (retryable), and
  prints nothing (the sender reports a code, section 4; the existing print stays
  for `web_fetch` only).
- **Loopback rule (one wording, used everywhere).** A *loopback host* is a
  literal loopback IP (`127.0.0.0/8`, `[::1]`) or the name `localhost`. Only a
  loopback host, with `allow_loopback = true`, may use `http` and may resolve to
  a loopback address. Any other host must be `https` and a loopback result for
  it is refused: a hostname that a hostile DNS owner points at `127.0.0.1` cannot
  aim signed POSTs at local ports.
- **Address policy per destination.** Beyond the loopback rule, every other non-public range (RFC 1918, CGNAT, link-local incl. the cloud
  metadata address, ULA, multicast, the 6to4/Teredo/NAT64 embeddings) needs
  `allow_private`. The policy is applied inside the connection's resolver, on
  every attempt, so a hostname that resolves to a public address at load time
  and to `169.254.169.254` at attempt time is refused (DNS rebinding), and a
  host with several addresses is refused if **any** is out of policy.
- **Redirects:** never followed (`max_redirects(0)`, `http_status_as_error
  (false)`); a 3xx is a terminal failure `redirect_refused`. The `Location`
  value is not read.
- **TLS:** `https` is required for every host that is not a loopback host (see
  the loopback rule), validated against the webpki roots ureq/rustls ships; no
  option to skip verification, no custom CA in v1. `allow_private` does not
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
  short redacted summary. Anything more the gateway needs it asks serve for over
  the authenticated API (after the redaction item above).
- **Why a summary, and why 512 bytes:** the owner needs to recognise the
  request in a notification. The `description` is display text derived from tool
  input and is **not redacted today**: `approval_json` (`approvals.rs`, served at
  `serve.rs` ~1592) returns it as is, and `cron/tool.rs` (~390) only escapes
  control characters. So `GET /approvals` can show an unredacted value. This
  note redacts the webhook summary itself (`redact`, then the 512 B cap) and
  proposes redacting `description` in `GET /approvals` too, as a separate small
  item (section 10, item 11). The summary is **untrusted** display text: the
  receiver renders it as data (escapes it for its chat markup) and never parses
  it.
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
| Flooding the receiver / being flooded by our own events | Bounded: one request in flight per destination; cron events 256 in memory and 10 000 pending on disk per destination, about 76 attempts each over 6 h; approval events 256 in memory, at most 5 attempts and never past their 900 s expiry. | Queue bounds, `queue_full` record by the producer, doctor warning. |
| Tampered outbox (an interactive agent or other process of the same user appends or edits lines) | Could make the sender sign a forged event. Cannot reach a destination the config does not name, an event type the destination does not list, or an oversized or malformed body. Can still add a well-formed event for a configured destination (the same account can edit the user config). | Sender re-validates every line before signing (section 4, Trust); 0600 only as a first layer; residual stated. |
| A notification mistaken for authority | A webhook can say "approval X is pending"; nothing in it executes or grants. | §8. |
| Destination secret reused as inbound hook key | Cross-protocol confusion. | Loader refuses a destination whose key equals any `[hooks.*]` key held in the process; docs say one secret per purpose. |

## 8. The single approval gate

- The webhook is a **notification**. It carries no credential, no approval link
  or token; `POST /approvals/{id}/decision` with the serve credential stays the
  only way to allow or deny. A gateway that is told "approval X is pending"
  gains nothing it can use except by asking serve.
- **Emission points, and what the queue must change** (`approvals.rs`):
  - `queue_entry` emits `approval_requested` (it already has session, tool,
    description).
  - `decide()` must report whether it actually **transitioned** the entry
    (`DecisionOutcome::Decided` gets a `transitioned: bool`; today a repeat
    returns the stored status), and only a transition emits
    `approval_resolved` (approved or denied).
  - **Expiry.** Today `prune_expired` marks `Expired` and, in the same call,
    drops terminal entries older than `EXPIRY_SECS` (a `retain`), and returns
    only the ids, which carry no session or tool; and it runs lazily (list,
    decide, queue). So `approval_resolved expired` cannot be built from what it
    returns, and an idle serve would never emit it. Change: `prune_expired`
    returns **full clones** of the entries it moved to `Expired` (taken before
    the `retain`; the fn is private today, ~97, so it gets a `pub` wrapper for the
    timer), and the sender calls it on a **timer, about every 30 s**,
    emitting one `approval_resolved` (`status = expired`) per clone. The lazy
    calls elsewhere also emit what they return. Each expiry emits once because
    the entry leaves `Pending` once.
  - The queue holds an optional `Arc<dyn Fn(ApprovalEvent) + Send + Sync>`
    notifier set by serve; the closure only enqueues (no I/O, no blocking, no
    error to the caller) and is **called after the entries mutex is dropped**,
    never under it. With no destination listing approval events it is
    absent and the path is unchanged.
- An emission failure never changes an approval, never retries a decision and
  never delays the fail-closed `Verdict::Deny` of `QueueApprover::decide`.
  Unanswered means denied (`EXPIRY_SECS`) whether or not the receiver was told.
- **Session ids and rollover.** An approval's `session_id` is the tole session
  that parked. The gateway starts a new session after 4 h of owner silence
  (epic #910), so an old `session_id` stays meaningful to the gateway only
  through its own chat-to-session mapping; the receiver must not assume it is
  the chat's current session.
- **Not in v1: delegated-child approvals (`dap-*`).** They live in a durable
  queue behind a separate route; `GET /approvals` lists them but they emit no
  webhook. This is a documented v1 gap. `approval_requested.data` carries
  `source` (`"serve"` now; `"delegation"` later) so adding them is additive, and
  the gateway's start-up reconciliation via `GET /approvals` already sees them.
- Design rules kept: `Destructive` is unaffected (never queued through serve,
  never allowlistable); no new approval path or bypass; `tole-core` stays
  platform-agnostic (sender, config, outbox and notifier in `tole-cli`; only
  `vetted_post` and `AddrPolicy` in `tole-core/web.rs`, which already owns the
  SSRF code).

## 9. Test plan, PR split, review

**Unit:** config loader (every refusal in section 1, user-only layering, project
warning, secret never in `Debug`/errors); `Target::parse`/`check` including the
agent-job rule, unknown destination and the preloaded map; event id derivation
(charset, length, determinism; the two fire ids that collide under `safe_name`
do not collide here); payload builders (caps, truncation at a char boundary
after redaction, 16 KiB envelope, `summary` dropped when over cap, silent rule
by status); signing against the **test vector** (a real hex value computed once
and pinned, also checked against `webhook::verify`); `AddrPolicy` table
(loopback only for a literal loopback IP or `localhost`, every private form
incl. v4-in-v6, a rebinding stub resolver, a multi-address host with one bad
address, distinct errors for policy refusal and DNS failure); redirect refused;
response over 4 KiB; retry schedule, `Retry-After` cap, 4xx no-retry, deadline
expiry (injected clock); outbox fold (torn tail, resume with attempt count,
done/dead, expired, oversized line, a line failing each validation rule of
section 4, leases, a far-future lease, a torn tail followed by an append, two
destinations for one fire id, the reconcile pass including a `skipped` run);
two-process append and drain with a lock race; queue bounds and the 64 MiB cap;
`prune_expired` returning full clones and `decide` reporting `transitioned`.

**Real-binary e2e** (`crates/tole-cli/tests`, the built `./target/release/tole`,
a receiver thread verifying the v2 signature with an independent
implementation, checking the window and deduping): a cron job with `deliver =
["webhook:gateway"]` yields exactly one verified delivery with the expected
payload, also when the tick is a separate `tole cron tick` process and serve
drains the outbox; an approval parked in `tole serve` yields
`approval_requested`, a decision yields `approval_resolved`, and an idle serve
yields `approval_resolved expired` within about 30 s of expiry. **Failure
injection:** 500 then 200 (same id, new timestamp, same body); 400 (no retry,
ledger `failed`); never answers (timeout; the tick and an approval turn are not
delayed, asserted); closed port; DNS failure retried; serve killed mid-retry and
restarted (the cron event arrives, same id; the approval event does not); queue
full; a host resolving to a private address without the flag refused; 302 not
followed (the redirect target records no hit); a hand-appended outbox line for a
destination not in the config, with the wrong `destination`, or oversized is
not sent. **Leak check (#781):** a canary secret in the environment, a tool
result, the job prompt and the approval description; neither the canary nor the
destination secret may appear in received bodies, the outbox, the ledger,
doctor output, stderr or `config check`.

**Estimated PRs** (each aiming at about 600 changed code lines or fewer, tests
counted; the first estimate was too low, so more PRs, smaller each):

1. `feat(core)`: `AddrPolicy` + `vetted_post` with its own `PostParams` in
   `tole-core/web.rs`; **`Hop` is left untouched** (public fields of a published
   crate); `web_fetch` behaviour unchanged. Distinct resolver errors (policy
   refusal, DNS failure). Also the event-id hash: `sha2` is a dependency of
   `tole-core` only (`Cargo.toml` ~40), so expose one small core helper (a `pub`
   item, a semver commitment) instead of adding `sha2` to `tole-cli`.
   **Independent review.** (~450)
2. `feat(cli)`: `[webhook_out]` config, user-only layering, project warning,
   shared key loader, role secret, `Target::Webhook` with the preloaded
   destination map in `TickOptions` and `deliver::check`, agent tool schema text,
   `config check`, doctor config lines. **`Target::Webhook` fails closed here:**
   until the sender lands, a webhook target records `failed` / `not_implemented`
   (never a silent `sent`). (~500)
3. `feat(cli)`: the pure pieces, no network I/O: event ids, payload builders and
   caps, redaction order, signing and the pinned test vector, outbox format and
   fold (torn tail, validation rules). (~600)
4. `feat(cron)`: the sender (thread, retry, drain lock, tick drain, ledger rows,
   `tick.rs` / `resume_parked` integration, doctor warnings), replacing the
   `not_implemented` stub. Realistically 900 to 1 200 lines: two PRs (sender
   core, then tick integration). **Independent review** of both.
5. `feat(serve)`: approval notifier, `prune_expired` change and 30 s timer,
   `decide` `transitioned`, redaction of `description` in `GET /approvals` (open
   question 11), approval events, `tole webhook test`, the e2e suite
   with failure injection and the leak check, threat-model rows, README status
   line and `docs/epics.md` in the same PR. Expect it to split in two as well
   (approval path; e2e and docs). **Independent review** (touches the approval
   path).

Each PR body carries `cora callers` / `cora impact` output for the touched
public items (`ApprovalQueue`, `DecisionOutcome` (the new `transitioned`
field), `Target`, `deliver::deliver`/`check`, `TickOptions`, the new core
helper, `vetted_post` and `PostParams`; `Hop` is not touched), each verified by grep as AGENTS.md
section 4 requires. New `tole-core` items are `pub(crate)` where possible
(published crate, semver).

**Needs independent review:** PR 1 (outbound path), the sender PRs (redaction,
outbox validation, data path), the approval PR (approval path). The outbox
validation of section 4 is a named review focus.

## 10. Open questions for the owner

The independent review's answers are adopted below; each stays open for the
owner to overrule.

1. **Outbox:** durable for cron results, volatile for approvals, plus
   reconciliation via `GET /approvals` at gateway start. Recommended.
2. **Delegation approvals (`dap-*`):** a documented v1 gap (section 8);
   `approval_requested.data.source` makes adding them additive.
3. **Approval content:** a 512 B redacted `summary` and `source`. The cron `job` name is
   dropped from approvals: `queue_entry` knows only session, tool, description,
   fingerprint and storage path, and a gateway can map a parked session to its
   run through `GET /cron/runs` (`session_id`). Recommended.
4. **Agent-created jobs:** `allow_agent_jobs` stays false by default.
5. **Plain http:** only for loopback (a literal loopback IP or `localhost`, with
   `allow_loopback`); no private-range http.
6. **Outbox rotation:** none in v1. The growth is recorded as a residual risk;
   the file is append-only with no compaction and a hard 64 MiB cap (section 4).
7. **Secret rotation:** a single secret per destination in v1.
8. **`tole cron tick` drains for up to 20 s** when no serve does, taking
   `outbox.lock` per pass only (section 4). Recommended.
9. **Environment proxy:** off, as `web_fetch`.
10. **Soon, not v1 core:** a `webhook_test` event with `tole webhook test
    <name>`, and a `cron_run_blocked` event for runs blocked before dispatch
    (`delivery_untrusted`, `not_enabled`, which never reach a settle). Names are
    reserved; the `events` list rejects unknown names until they ship.
11. **`GET /approvals` is unredacted** (section 6): redact `description` there
    too, as a separate small item before or with the approval PR. Recommended.
12. **Outbox checkpoint.** Without one, every fold reads the whole file (up to the
    64 MiB cap). Recommendation: not in v1; add a cache-only checkpoint offset
    if real use gets near 32 MiB.
