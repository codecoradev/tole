# Automation (cron, scheduler, webhook) — design note

Status: **proposal, nothing implemented.** Scope: unattended runs of tole
(scheduled and event-triggered). Chat channels (Telegram, Slack, …) are
explicitly **out of scope** for this note.

**Placement (owner decision 2026-11-11):** the scheduler lives **inside
tole**, not in a separate repository: the ledger core in `tole-core`, the
host (ticker in `tole serve`, `tole cron tick`) in `tole-cli`. This
supersedes the 2026-11-09 proposal to put the scheduler in a new repository.

Model to follow: [Hermes Agent](https://hermes-agent.nousresearch.com/docs/user-guide/features/cron)
(`cron`, `webhook subscribe`, `no_agent` jobs, per-job model pin, catch-up
window). Where Hermes and tole's design rules (`AGENTS.md` §6) disagree, the
design rules win.

## 1. Goals and budgets

| Goal | Concrete budget (proposed; measure against `docs/perf.md` before merge) |
|---|---|
| Light | No new async runtime, no SQLite, no `chrono`. Idle cost = one sleeping timer armed for the next due job; zero polling. |
| Auditable | Scheduler core readable in one sitting, one module, deterministic (injected clock, no hidden globals). All state is JSONL readable with `jq`. |
| Durable | Crash at any byte boundary loses no job and double-fires none. Proven by a crash-injection test (kill between every append). |
| Safe | Unattended ≠ unapproved. Same three enforcement layers; no new bypass path. |

**Priority order when goals conflict:** correctness and durability, then
runtime performance (latency, idle and per-run overhead), then auditability,
then binary size and LOC. Size and line counts are **reported, not gated**:
each PR records measured binary delta, idle wakeups/RSS and scheduler
latency against `docs/perf.md`, and a larger or longer design is accepted
when it is measurably faster or safer. Shrinking at the cost of performance
or correctness is not a goal.

## 2. Problem → guardrail reference (so it does not repeat)

Each row is a failure users actually hit in a competitor; the right column is
the rule this design commits to. Sources are linked or named.

| # | Observed failure | Source | Guardrail here |
|---|---|---|---|
| P1 | **Silent skip.** Serial `tick()` blocked by a long job; later jobs missed their grace window and were fast-forwarded with an INFO log. | Hermes [#9086](https://github.com/NousResearch/hermes-agent/issues/9086) | Runs execute concurrently (bounded). A job is never skipped without an appended `missed` record carrying the reason, shown in `tole cron list`. |
| P2 | **Delivered, then marked failed.** Heartbeat/fence timeout overwrote a successful run with "Interrupted by shutdown"; watchdogs alerted and retries duplicated work. ~12% of one operator's runs; fix took 5+ competing PRs. | Hermes [#105861](https://github.com/NousResearch/hermes-agent/issues/105861) (+#100401) | Append-only: a `done` record is never overwritten. Run outcome and delivery outcome are **separate fields**. No heartbeat/lock-timeout ownership; ownership = pid + start-time lease checked on recovery only. |
| P3 | **Duplicate or lost fires across processes.** `jobs.json` rewritten ~3×/min by three writer classes; in-flight guard was per-process. | #105861 follow-up (auditd measurement) | One file, one writer role: the appender. Claim = append a `fire` record with `fire_id` *before* running; a second claimer sees it. No rewrite-in-place ever. |
| P4 | **Heartbeat bleeding.** Always-on heartbeat burns tokens 24/7; bootstrap files re-injected each call ($300–500/mo unoptimized). | OpenClaw users (r/openclaw cost threads, vibecoding.app roundup) | No heartbeat. Jobs are explicit. Per-job model pin + budget; `no_agent` jobs cost zero tokens; webhook `filters` run before any LLM call. |
| P5 | **Tasks "just don't run."** Daily task set up, never executed, no signal. | OpenClaw user reports | Cron validated on save; `next_run` shown immediately; `tole cron status` reports whether a ticker is alive; a stale ticker is a `tole doctor` failure. |
| P6 | **Amnesia after compaction** — standing instructions summarized away (an agent deleted email after "don't act until I say so"). | OpenClaw / Meta alignment anecdote | A job's instruction lives in the job record and is re-sent verbatim as the pinned prompt every run; never subject to compaction. |
| P7 | **Over-proactive/unsafe unattended agent.** "Don't ask permission" defaults; exposed instances; prompt injection via payloads. | OpenClaw security reports (SecurityScorecard, Microsoft) | See §5: allowlist-only, parked approvals, ReadOnly default for webhook runs, payloads wrapped as untrusted. |
| P8 | **Agent self-grants power.** A job/route created by the agent could widen its own tools. | Hermes design (toolsets only by manual edit) | Agent-created jobs are created **paused** and can never exceed the creator's allowlist; elevated tools are config-file-only. |
| P9 | **Poll-reload gap.** One-time schedule created just after the worker's 5-min reload was missed, and `runAt` disabled itself before the first run, so a failed run could not be retried. | Nakama docs (automations) | No reload polling: the timer is re-armed from the log on every append. One-shots disable *after* a terminal record, not before. |
| P10 | **Misconfigured job spends tokens.** | Hermes `blocked_config` | Pre-dispatch validation (skill present, delivery target valid, MCP/tools resolvable). Failure ⇒ `blocked` record, one alert, **zero tokens**. |
| P11 | **Setup tax / weekly maintenance rituals.** | OpenClaw, Nakama landing | One binary, no daemon required: `tole cron tick` from systemd timer or crontab works; `tole serve` can host the ticker in-process. |

## 3. Data model (append-only JSONL, no mutation)

`~/.tole/cron/jobs.jsonl` — a log of job definitions and run records; current
state is a pure fold over it (same pattern as sessions). Entries:

- `job` — `{id, name, schedule, prompt, mode: agent|script, allow[], model?,
  budget{steps,minutes,tokens}, workdir?, skills[], timezone?,
  catch_up: skip|one, grace_minutes, deliver[], paused, created_by}`
  (a later `job` with the same `id` supersedes; `paused`/`removed` are new
  entries, never deletions).
- `fire` — `{fire_id, job_id, scheduled_for, claimed_at, lease{pid,start_time}}`
  appended **before** execution (the at-most-once claim; `fire_id` is the
  idempotency key everywhere downstream).
- `done` — `{fire_id, session_id, status: ok|error|cancelled|budget|awaiting_approval|interrupted, usage}`
- `delivery` — `{fire_id, target, status: sent|failed|skipped, error?}` —
  appended separately; never changes `done.status` (P2).
- `missed` / `blocked` — `{job_id, scheduled_for, reason}` (P1, P10).
- `hook_seen` — `{route, delivery_id, at}` webhook idempotency, durable
  (survives restart, unlike an in-memory TTL cache).

A run **is a normal session**: its transcript is the existing JSONL session,
resumable and replayable; `done` links to it. No second history store.

Layering: schedule math, fold, claim/recovery state machine live in
`tole-core` as pure functions taking `now` and a `Storage`-style handle
(platform-agnostic, deterministic tests). The ticker, signal handling, HTTP
and process spawning live in `tole-cli`.

## 4. Scheduler

- **Triggers:** `manual`, `schedule` (5-field cron / `every <dur>`), `at`
  (one-shot). Same shapes as Hermes and Nakama.
- **Loop:** compute next due from the fold → sleep until then (single timer) →
  append `fire` for each due job → spawn runs concurrently, bounded
  (default 4, `cron.max_concurrent`). A new `job` append wakes the timer
  (in-process) or is seen at next tick (external).
- **`tole cron tick`:** one pass then exit, for crontab/systemd-timer
  hosting. Takes an exclusive `flock` only around "read due + append `fire`"
  (milliseconds), never around execution.
- **Recovery** (on tick/start): a `fire` with no `done` whose lease pid is dead
  (or start-time mismatch) gets an `interrupted` `done`; the run's session is
  resumable with the existing crash-resume (E5). Policy per job: `retry_once`
  or leave for the operator. No automatic infinite retry.
- **Catch-up:** `catch_up=skip` (default) records `missed`; `one` fires a
  single late run within `grace_minutes`. Always logged (P1).
- **Time zones: explicit, and pluggable.** Every job has an explicit
  `timezone` (default: `cron.timezone` from config, else `UTC`); `tole cron
  list` always prints it next to `next_run` (local and UTC). Accepted forms:
  `UTC`, a fixed offset (`+07:00`), or an IANA name (`Asia/Jakarta`). The
  source of zone rules is a config choice, `cron.tz_source`:
  - `system` (default on Unix): parse TZif files from the host's
    `/usr/share/zoneinfo` at runtime — a few hundred lines, **no embedded
    database**, so zero binary cost and rules follow OS updates.
  - `bundled` (cargo feature `tz-bundled`, for Windows/minimal containers/
    embedders without zoneinfo): ships rules inside the binary at a measured size cost.
  - `offset-only`: UTC and fixed offsets only; an IANA name is rejected.
  A name that cannot be resolved by the chosen source is rejected **on save**
  with a message naming the source, never silently treated as UTC.
  DST semantics (Hermes-compatible: wall clock is preserved, `0 9 * * *`
  stays 09:00 local across changes), recorded in each `fire`:
  a local time that does not exist (spring-forward gap) fires once at the
  first valid instant after the gap; an ambiguous local time (fall-back)
  fires once, on the first occurrence. `every <dur>` jobs are plain
  durations and unaffected. These cases get table-driven tests.
  **As implemented, part 1 of #580:** the zone layer is in `tole-core`
  (`tz`):
  - UTC, fixed offsets (at most ±14:00) and rules from TZif data. TZif
    versions 1 to 4 are read, and the POSIX TZ footer that slim zoneinfo
    files use for every future date is evaluated. A POSIX TZ string can
    also be used alone.
  - `Zone::from_local` gives one instant, the first instant after a gap,
    or both instants of an overlap.
  - Sources are a trait, `ZoneSource`. `OffsetOnly` is the default with no
    data; `ZoneinfoDir` reads `/usr/share/zoneinfo` at run time. It follows
    alias links only inside the directory, refuses anything but a regular
    file of at most 512 KiB, and never waits on a FIFO.
  - Leap-second zones are refused.
  - `bundled` is not built: an embedder or a later feature plugs bundled
    data in as another `ZoneSource`, so core embeds no database.
  **Part 2 (schedules and the CLI):**
  - **Schedules.** A `Zoned` schedule walks matching wall-clock minutes. A
    minute in a gap fires at the gap's end, and a minute in an overlap fires
    at its first instant. A minute whose instant has already passed is
    skipped, so nothing fires twice, even when the cursor sits inside the
    repeated hour. `@every` ignores the zone.
  - **The ledger** resolves each job's `timezone` through its
    `ZoneSource` (`Ledger::open_with_zones`). A `fire` of a zoned job
    records `tz {zone, offset, case}`, where `case` is `gap` or
    `first_of_two`.
  - **Old jobs are untouched.** A job without `timezone` uses the plain UTC
    code path and writes byte-identical records.
  - **Bad zones.** A zone the source cannot resolve is refused on save,
    naming the source. A hand-written one leaves the job unscheduled and
    shown without a next run, never silently UTC.
  - **CLI.** `tole cron create|edit --timezone`. `cron.timezone` is the zone
    of a job created without one; an edit never adds one. `cron.tz_source`
    is `system` (default on unix, `$TZDIR` honoured) or `offset-only`, and
    both are user-config only. `tole cron list` shows each job's zone and
    its next run as local time and UTC.
  - **Not built:** `bundled`; the source is pluggable for it.
- **Cron parsing:** small in-tree parser for 5-field cron (+ `@every`),
  property-tested against a brute-force minute walker. Prefer this over a
  dependency; revisit only if the audit cost of ~200 LOC exceeds a vetted crate
  with no transitive deps.

## 5. Unattended safety (non-negotiable)

- Unattended runs use the existing `AllowlistApprover`: the job's `allow[]`
  (e.g. `write:edit_file`) is the **only** pre-authorization. `Destructive`
  remains unregistrable and never allowlistable.
- A non-allowlisted Write does **not** fail the job or auto-approve: it parks as
  a pending approval in the existing `/approvals` queue (15-min expiry →
  denied), the run ends `awaiting_approval`, and approve/deny resumes it
  through the shipped remote-approval path.
  **As implemented (#579):** parking needs the in-memory queue of a
  running server, so it applies to runs hosted by `tole serve --cron`. A
  standalone `tole cron tick` has no queue: it refuses a Write the job's
  `allow` list does not cover, and the run settles `awaiting_approval`.
  Neither path ever auto-approves.
  **Resume (#581):** an `allow` on a parked cron call (`POST
  /approvals/{id}/decision` for a session `tole serve` does not host)
  continues the run in place:
  1. Under the ledger lock, a new `resume` record `{fire_id, resumes,
     job_id, claimed_at, lease}` claims the continuation. The fold accepts
     it only when the `resumes` run settled `awaiting_approval` for the same
     job and has no earlier `resume`, so a parked run resumes at most once
     and a hand-written `resume` line for any other run is ignored. The
     continuation is an ordinary pending fire (same `scheduled_for`): it
     settles with its own `done`, dies with its lease like any run
     (`recover` → `interrupted`), and its outcome becomes the job's latest.
     The parked `done` is never changed (append-only).
  2. The run's session is reopened (it must still be parked mid-turn, not
     at `final`) and the parked turn is resumed, with no new prompt. The
     session file sits in the job's workdir, which the job can write, so
     it is opened once with `JsonlStorage::open_bounded`: on unix no
     symlink is followed and a FIFO is not waited on, the handle must be a
     regular file within 64 MiB, and the parked-turn check, the replay and
     every append use that one handle. A file swapped after an earlier
     check is refused at that open. The
     turn replans from the denial; the approval is a one-shot for exactly
     the approved call (same tool, same input), so the model can repeat that
     call once and nothing else. A model that takes another way runs nothing
     the approval did not cover.
     **An allow does not run the call by itself.** The approved call runs
     only if the resumed model makes that same call again. If the model
     changes the input or does something else, the approved call never
     runs, and the run settles on whatever the model does instead.
  3. The continuation goes through the same run, settle and deliver path
     as a tick (budget, daily cap, skill pins and profile re-checked). It
     runs under the same turn plan as a fresh run: the job's model pin, its
     `max_steps` step budget, its token budget and its `max_minutes` limit.
     Each budget is counted for the continuation alone, not for the whole
     run: the continuation is a run of its own with its own `done`, and a
     resume needs a human allow each time. Its `done` reports only the
     continuation's tokens, so the daily cap does not count the parked
     run's tokens twice.

  Not resumable: script jobs (they never park); runs parked by a
  standalone `tole cron tick` (no queue, so no approval to give: re-run
  them); a denied or expired approval (a resume without an allow parks
  again, it never runs the call); approvals lost with a `tole serve`
  restart (the queue is in memory); a run whose session already ended;
  a run of a removed job; and a parked run that was already resumed (its
  continuation can park and be resumed in turn).
- `mode: script` (Hermes `no_agent`): argv via the `run_command` machinery
  (no shell, cwd jail, timeout, output cap), stdout is the result, **zero
  LLM tokens**. Empty output or a `[SILENT]` first line = nothing delivered.
- Per-job `budget` reuses mission budgets (`--max-steps/--max-minutes/--max-tokens`);
  a global daily token ceiling (`cron.daily_token_cap`) stops all agent jobs
  and records `budget`. **As implemented (#579):** the cap is a user-config
  key (a project config cannot set it), counted per UTC day from the `tokens`
  of settled runs and checked when a run is claimed; a run gets at most what
  is left, so runs in flight together can overshoot it by their own usage. Model pin per job so cheap work never inherits the chat
  model.
- Agent-created jobs: created `paused`, `created_by` recorded, `allow[]` ⊆
  creator's; enabling is a human action. Config-sourced jobs follow the
  existing rule that faces refuse config-sourced hooks/skills/MCP unless the
  config is trusted.
  **As implemented (#581 part 2):**
  - **The agent path.** An agent creates a job only through the
    `cron_create` tool, a Write tool approved like any other. It is
    registered for the session faces (`run`, `chat`, `serve` sessions) at
    depth 0 outside plan mode. A cron run, a child agent and the MCP server
    face do not have it.
  - **What the host sets.** The host, not the tool input, sets the job's
    workdir (the session workspace), `paused = true`, `created_by = agent`
    and `creator_allow` (the session's own allow list). The input type
    refuses every other field by name: an id, a script, skills, a profile,
    a pause state or any provenance field. `allow` may name only the
    creator's allow patterns, and the creator's deny (`!`) patterns are
    always kept. A `command:` delivery is refused.
  - **Enabling.** `tole cron resume` is the enable. It records
    `enabled_by = cli`, and only the CLI writes that field.
  - **Every job not saved through `tole cron create`** (`created_by` other
    than `cli`, including a hand-written line with none) is checked before
    each run and is `blocked` unless:
    - it was enabled by its owner (`not_enabled`);
    - its `allow` stays within `creator_allow` and keeps its deny patterns
      (`allow_exceeds_creator`);
    - it is an agent job, not a script (`script_untrusted`).
  - **`tole cron edit` of an agent's job** keeps its creator, grant and
    enable, and refuses an `--allow` beyond the creator's.
  - **Provenance is fixed by the creating line.** The ledger fold
    (`tole_core::ledger::PROVENANCE`) keeps a live job's `created_by` and
    `creator_allow` from the line that created it. A later line, an edit or
    a hand-written line cannot change or drop them; a job saved again after
    its removal is a new job.
  - **A run's jail must not hold the cron dir.** For every agent job, a run
    whose workdir holds the cron dir is `blocked` (`ledger_in_workdir`),
    because its file tools could write job records.
  - **The trust root.** The ledger file itself is the trust root: a
    process that can write `jobs.jsonl` directly (the owner, or a tool the
    owner allowlisted to run commands) can write a line claiming
    `created_by = cli`, as it can edit any of the owner's files.
  - **Config-sourced jobs do not exist.** No config defines jobs:
    `[cron]` takes only scheduler settings (`daily_token_cap`,
    `max_concurrent`, `timezone`, `tz_source`), unknown keys
    are an error, and a project config's `[cron]` is ignored. A cron run
    reads no project config from its workdir: its settings come from the
    job, the user config and the process config (a project config of the
    directory `tole cron tick` or `tole serve` started in reaches it only
    through the existing trust gate).
  - **MCP servers (#636).** A cron run attaches no MCP servers today. When
    jobs gain MCP servers, they go through the plugin lock like every face:
    a server runs only when the owner approved it with
    `tole plugin approve`, pinned to its tool-manifest hash, and its tools
    stay `Risk::Write`, so they park in `/approvals` unless the job's
    `allow` names them.

## 6. Webhook (on `tole serve`, not a new server)

Route: `POST /hooks/<route>`. Defined in trusted config (and `tole hook add/rm/list/test`
writing the same append-only log). Follows Hermes' route model, minus channel
delivery:

- **Auth, required:** HMAC-SHA256 over `<timestamp>.<body>`, timestamp within
  ±300 s (replay-safe); GitHub `X-Hub-Signature-256` accepted as a
  compatibility mode. No secret ⇒ route refuses to load. No
  `INSECURE_NO_AUTH` except on loopback bind, matching Hermes.
- **Hardening:** body-size cap checked before read, per-route rate limit,
  idempotency by delivery ID persisted as `hook_seen`, constant-time compare,
  failures rate-limited per IP (reuse the serve auth limiter).
- **Cheap rejection first:** `events` allowlist → declarative `filters`
  (`equals/contains/in/regex/exists`, `all/any/not`) → only then spawn a run.
  Non-matching returns `200 {"status":"ignored"}`; no tokens spent.
- **Action:** fire an existing job (`cron_job`, Hermes' event-triggered cron).
  The rendered prompt template is *transient per-run context*; the job's own
  prompt, allowlist, model and budget apply. Returns `202` immediately.
- **Untrusted payload:** authenticated ≠ trusted. Fields are interpolated only
  through narrow `{dot.path}` templates, wrapped in an explicit untrusted-data
  delimiter in the prompt; webhook-triggered runs are **ReadOnly-only by
  default**. Wider tools for a route are a manual config edit (never via CLI
  or agent), P8.
- **Coalescing** (debounce bursts per entity key) is phase 2; script
  transforms are deliberately **not** in v1 (more attack surface to audit).

**As implemented, part 1 of #582 (the security gate):**
- **Routes are `[hooks.<route>]` in the user config only.** A project
  config's `hooks` is ignored with a warning, so a repository can neither
  open nor change an endpoint.
- **Each route has a key.** It is named by `hmac_key_env` or
  `hmac_key_file` (a regular file, not a link or a FIFO, at most 4 KiB, at
  least 16 bytes) and is never shown: not in errors, logs, responses,
  `config check` or the ledger.
- **A route that cannot load stops `tole serve` before it listens,** naming
  the route and the fix. That covers no key, both key sources, a key
  variable that is not set, a key that is too short, bad limits, or
  `insecure_loopback` on a non-loopback bind.
- **`insecure_loopback = true`** (no signature) loads only on a loopback
  bind and answers only loopback clients.
- **`POST /hooks/<route>` needs no bearer token; it is authenticated by
  signature.**
  - An unknown route gets 404, and a body over the route's
    `max_body_bytes` (default 256 KiB, at most 1 MiB) gets 413. Both
    answers come before a byte of the body is read.
  - **Signature schemes (`tole_core::webhook`):** native `X-Tole-Timestamp`
    + `X-Tole-Signature: v2=<hex>` over
    `v2\n<timestamp>\n<delivery id>\n<event>\n<body>` (empty event when
    there is no `X-Tole-Event`) within ±300 s (#678: the v1 `sha256=` form
    over `<timestamp>.<body>` signed neither the id nor the event and is
    refused), or GitHub `X-Hub-Signature-256` over the body. Both use
    HMAC-SHA256 on the `sha2` already in the tree and a constant-time
    compare.
  - **Delivery ids:** `X-Tole-Delivery` / `X-GitHub-Delivery`, required,
    1 to 128 of `[A-Za-z0-9._:-]`.
  - A failed signature counts toward the source IP's failed-auth limit.
- **Idempotency and rate limit:** a delivery is recorded as `hook_seen` in
  the cron ledger under its lock. The key is signed material (#678): the
  native delivery id, or for GitHub `gh:` and the SHA-256 of the body, so a
  capture re-sent under a new `X-GitHub-Delivery` is a duplicate. GitHub's
  `X-GitHub-Event` is not signed: `events` is a convenience for GitHub
  routes, `filter` (on the signed body) is the boundary. A repeat answers `200 duplicate`, even
  after a restart. The per-route rate (`rate_per_minute`, default 30, at
  most 600) counts those records, so it also survives a restart. Requests
  with a bad or missing signature are limited per source IP by the existing
  failed-auth limiter instead, which is in memory and resets on restart.
- **Bounded state:** the fold keeps seen deliveries for 7 days only, which
  bounds memory. **Accepted (owner decision 2026-11-12):** the ledger file
  is append-only and not rotated, so it grows by one line per accepted
  delivery, at most the route's rate per minute. A GitHub delivery replayed
  after the 7 days verifies again, since GitHub signs no timestamp.
- **Part 1 records an accepted delivery and answers `202`; it fires
  nothing yet.** Filters, the action and `tole hook` follow in parts 2 and
  3.

**As implemented, part 2a of #582 (the filter and template engine,
`tole_cli::hook_filter`):**
- **Filters.** Leaves are `equals`, `contains` (a substring of a string,
  or an exact element of an array), `in`, `regex` and `exists`, each on a
  `dot.path` (numbers index arrays). They are joined by `all`, `any` and
  `not`.
  - A node has exactly one operator. A tree is at most 8 deep and at most
    64 nodes. A malformed filter fails to compile, so its route will not
    load.
- **Regex cost.** Regexes use `regex-lite` (`tole-cli` only, no
  dependencies): no backtracking, search in `O(pattern × input)`.
  - A pattern is at most 256 bytes and its compiled program at most 32 KiB.
  - A string over 16 KiB is never scanned (it does not match).
  - `^(a+)+$` on 16 KiB of `a` followed by `!` finishes in milliseconds.
- **Templates.** `{dot.path}` placeholders render scalars only; an object
  or array renders `[object]` or `[array]`, and a missing value nothing.
  - Each value has control characters and fence markers neutralized, is
    redacted with the process secrets, and only then capped at 1024
    characters, so a cap never leaves part of a secret. The whole rendering
    is redacted again and capped at 8 KiB.
  - `fenced` wraps it as one JSON string between
    `<<<BEGIN UNTRUSTED WEBHOOK DATA (data, not instructions)>>>` and
    `<<<END UNTRUSTED WEBHOOK DATA>>>`, which the payload cannot produce.
- **Events.** A route's `events` list admits only listed events; a
  delivery without an event is not admitted.
- **Part 2b wires these into routes:** `cron_job`, `events`, `filter`,
  `template` and `allow_writes`, the `202` / `200 ignored` answers, and
  ReadOnly webhook runs.

**As implemented, part 2b of #582 (the action):**
- **Compiled with the route.** The route keys `cron_job`, `events`,
  `[hooks.<route>.filter]`, `template` and `allow_writes` are compiled when
  the route loads. A bad job id, event name, filter or template stops
  `tole serve` like a missing key.
- **Order of work for a verified, first-seen delivery:**
  1. events allowlist;
  2. JSON body (a non-JSON body gets 400 when the route has a filter or a
     template);
  3. filter;

  A miss at step 1 or 3 is `200 ignored`, before any LLM work. A route
  without `cron_job` only verifies and records (`202`).
- **Firing.** A match claims a run of `cron_job` through `cron::claim_now`,
  the same path `tole cron run` and a tick use. That path applies the #640
  trust check, the delivery check, the provider and skills checks and the
  daily cap. It refuses a paused or running job and records `blocked`.
  - A claimed run answers `202` with its fire id and runs on a background
    thread. Anything else answers `409` and fires nothing.
  - The run keeps the job's model pin, budgets and allowlist. Under `tole
    serve` a Write the allowlist does not cover parks in `/approvals`.
- **The trigger, this run only.** The fenced rendering of `template` is
  appended to the job's prompt for this one run; the job record never
  changes.
- **ReadOnly by default.** Unless the route says `allow_writes = true`
  (a manual edit of the user config), the run's roster is built in plan
  mode, so its Write tools are absent, not merely denied. The job's
  allowlist cannot bring them back.
- **Script jobs** can be fired too. They take no context (a script has no
  prompt), and the trust check refuses a script job not saved through the
  CLI.
- **`contains`** is a substring match on a string and a whole-element match
  on an array.

**As implemented, part 3 of #582 (`tole hook`):**
- **`tole hook add ROUTE`** appends one `[hooks.ROUTE]` section to the
  user config, never a project config.
  - The rest of the file, comments included, is kept, and the file is
    replaced through a temporary file and a rename.
  - The route is checked with `hooks::load`, the loader `tole serve` uses,
    before anything is written. A key variable unset in the current shell
    is checked with a stand-in, and `add` notes that serve will refuse to
    start until it is set.
  - An existing route is refused (remove it first).
- **The key is never on the command line.** `add` takes exactly one of
  `--key-env NAME` or `--key-file /absolute/path`, and there is no flag for
  the key value. No `tole hook` output shows a key, and `list` does not
  show where one comes from (a load error may name the variable or file,
  as serve's does).
- **P8 holds.** `allow_writes`, `insecure_loopback` and the filter have no
  flags. Widening a route stays a manual edit of the user config.
- **`list`** shows each route's job, events, the filter's shape (paths and
  operators only, never the compared values), `writes: yes|no` and
  `signed` or `insecure (loopback only)`.
- **`remove ROUTE`** cuts the route's `[hooks.ROUTE]` and
  `[hooks.ROUTE.*]` sections and keeps the rest. If the route is still there
  afterwards (an inline table, for example), it refuses and asks for a
  hand edit.
- **`test ROUTE`** is a local dry run.
  - It loads the route with the real key, reads `--payload` (default
    `{}`, capped at `max_body_bytes`), signs it with the native scheme and
    verifies the signature.
  - It applies the events allowlist and the filter, then prints the fenced
    rendering of the template, redacted with the process secrets and
    capped as serve does.
  - It fires nothing and writes no ledger line.
- **`test ROUTE --fire --url http://127.0.0.1:PORT`** posts the same signed
  delivery to that `tole serve`. Serve verifies it and fires through
  `cron::claim_now` like any delivery. Only a loopback `http://` host
  (`127.0.0.0/8`, `[::1]`, `localhost`) is accepted, so the command never
  sends a signed delivery off the machine.

## 7. Adoptable from Nakama (withnakama.com)

Nakama is team-oriented (multi-tenant dashboard, channels, ~200 MB idle
Docker) — a different product from a single-binary harness, so we copy
ideas, not architecture:

- **Adopt:** automation = *name + prompt + profile + one trigger*
  (`manual | schedule | runAt`); validate cron and delivery target **on save**;
  show `next_run` in the list; per-run status with unread/failed visibility;
  `notify_on: success|failure|both`; separate delivery status
  (`sent|failed|skipped`, `null` when unconfigured); a heartbeat that makes
  "is the scheduler alive?" a one-line status check.
- **Adopt later:** *workflows with receipts* — a declared step recipe where
  data steps store receipts and the final summarize step sees only receipts.
  Maps cleanly to `script` steps + one agent summary (cheap, auditable,
  replayable). Self-improving skills with an **approval** gate (also Hermes).
- **Avoid:** poll-reload of schedules (P9), `runAt` disabling before the
  first run (P9), a separate worker process that must be babysat (P11).
- **Composio:** Nakama and Composio expose SaaS via MCP; tole already is an
  MCP client, and per `AGENTS.md` §6 those tools stay `Risk::Write`. No tole
  work needed beyond cron jobs being able to attach MCP servers; those
  servers go through the plugin lock (#636, see §5).

## 8. Surface (mirror Hermes naming)

```
tole cron create "<schedule>" "<prompt>" [--name N] [--script CMD] [--allow write:tool]
         [--model M] [--budget-steps N --budget-minutes N --budget-tokens N]
         [--workdir D] [--skill S] [--profile P] [--catch-up skip|one --grace-minutes N] [--paused]
tole cron list | status | runs [job] | pause|resume|remove|run|cancel <job> | edit <job> … | tick
tole hook add|list|remove|test <route> …
```

Job names resolve case-insensitively but ambiguous names are refused with the
candidate IDs (names are not unique; Hermes learned this the hard way).
`tole doctor` gains: ticker alive, last tick age, jobs `blocked`/`missed`
counts, webhook routes without secrets.

## 9. Delivery without channels

Targets in v1: `session` (the run's own transcript — default), `file`
(append to a path under the workspace), `command` (argv, trust-gated like
other config commands; as implemented, only a job saved with `tole cron
create` may carry one, any other job is `blocked` with `delivery_untrusted`), and the pull route `GET /cron/runs` on `tole serve`
so uteke-mobile or any client can read results. Telegram/Slack-style targets
plug into the same `delivery` record later without changing the core.

## 10. Phasing (1 issue = 1 branch = 1 PR, ≲ 20 files each)

1. **Core fold + cron parser + claim/recovery** in `tole-core`, with the
   crash-injection and property tests. No CLI yet.
2. **`tole cron` CLI + `tick` + in-process ticker in `serve`**; `agent` and
   `script` modes; budgets; `doctor` checks.
3. **Unattended approvals** wired to `/approvals`; paused agent-created jobs.
4. **Webhook routes** (auth, idempotency, filters, `cron_job`), `tole hook`.
5. Phase-2 items: coalescing, IANA time zones, workflows-with-receipts,
   delivery targets beyond §9.

Each PR updates the README status line and `docs/epics.md` in the same PR
(`AGENTS.md` §7).

## 11. Open questions

1. ~~IANA time zones~~ — decided: explicit per-job zone, pluggable source
   (§4). Remaining detail: default `cron.tz_source` on Windows (`bundled`).
2. Should `tole cron` own a lockfile-based "singleton ticker" so `serve` and a
   system timer cannot both tick? (Recommendation: yes; the `fire` claim already
   makes double-ticking safe, the lock only avoids wasted wakeups.)
3. Retention: how long to keep `jobs.jsonl` before rotation? (Rotation must
   write a new segment, not rewrite — keeps the append-only rule.)

### Entry point: decision (c), both

This one is about the multi-round discussion (#588, design
[profiles-collaboration-secrets.md](profiles-collaboration-secrets.md)
A8.3), not the scheduler. The discussion runs as a host library
(`tole_cli::discussion_host`): inline participants, the scribe and the
claim-checker, the gate, the budget and crash resume. The owner wants the
mode open to agents too, not only to the owner. The options considered and
what each costs in security:

| | (a) owner-only `tole discuss` | (b) model-facing tool | (c) both |
|---|---|---|---|
| Who starts one | the owner, from a shell | any agent that has the tool, and so whatever text steers that agent | both |
| Prompt injection | none: the owner types the contract | a web page, issue or tool result can ask the agent to open one, and the contract (question, criteria, participants) is model-written | as (b) |
| Budget | the owner names it | carved from the caller's **remaining** budget, never added (A8.1) | as (b) for agents |
| Who may speak | user-config profiles | the same, and only profiles the caller may delegate to (a subset of its allow list) | as (b) for agents |
| Authority | read-only: inline voices have no tools | read-only participants by default; any Write a participant wants goes to the **owner's** approval surface, never to the caller's allowlist | as (b) |
| Depth | 0 | a discussion is one delegation level: the tool exists only where `depth < max_depth`, and its participants cannot start another | as (b) |
| Power | the owner's | never more than the creator holds: no new secrets, allow patterns, profiles or budget | as (b) |
| Approval | none (the owner typed it) | the tool is `Risk::Write` (it spends budget and may start children), approved or allowlisted like `delegate` | as (b) |
| Cost of a mistake | the owner's budget | bounded by the carve and `max_rounds`; an agent cannot loop discussions past its own budget | as (b) |

Every option keeps what the library already enforces: only the gate decides
report, escalate or continue; no digest or contribution text can trigger a
report, a write, a roster change or a tool call; the budget and
`max_rounds` stop with an escalation to the owner; dissent is never dropped;
participants are user-config profiles; the scribe and the claim-checker are
built-in or owner-overridden roles.

**Decision (owner, 2026-11-14): (c), in two steps, both shipped.**

1. `tole discuss start|resume|list|show` (owner-only, depth 0, read-only
   inline participants; `--dry-run` shows the cost, `resume` continues after
   a crash).
2. The model-facing tools `discuss`, `discuss_status` and `discuss_stop`
   (`tole_cli::discuss_tool`), under exactly the rules of `delegate`:
   - all three are `Risk::Write`, approved or allowlisted like `delegate`;
     the prompt shows the question, the participants and the budget;
   - they exist only where `delegate` does (the same host hands out both,
     so never at the goal's depth limit, in plan mode or without a
     provider);
   - the budget is carved from the caller's remaining budget in the
     delegation tree (`budget_tokens`, or half of what is left, never more),
     recorded as a running `consult` node; the tree is charged what the
     ledger counted when the discussion ends, and a crash is charged the
     whole carve; at most two discussions run at once per process;
   - participants are user-config profiles whose allow patterns the caller's
     own allow list covers (a refusal, where `delegate` trims), never a
     built-in role; they are inline voices with no tools and no process, so
     none can start a discussion, call a tool or write, and no Write of
     theirs can reach the caller's allow list (there is none to ask for);
   - the input has five fields; `allow`, `profile`, `tools`, `secrets` or
     any other field is refused, and the id is the host's, so a model adds no
     secret, allow pattern, profile or budget and cannot name or overwrite
     the owner's discussions;
   - management is limited to discussions the same session started (the
     tree's host-only node, not a task string a model can write):
     `discuss_status` returns state, tokens and the digests fenced as data;
     `discuss_stop` records the ledger's own `stopped` escalation, which
     the driver closes like any other, so no participant text can start,
     prevent or delay it. An interrupted discussion (its process died) is
     closed by a stop without a model call, or continued by the owner with
     `tole discuss resume`.

3. Independent (child-process) participants for agents (#704). `discuss`
   takes an optional `modes` array (one entry per participant, in order),
   each `inline` (default) or `reviewer`, a closed enum. The owner's
   `tole discuss` is unchanged. A reviewer is a `consult` child of the
   caller's goal started through the same host as `delegate`:
   - the profile must pass the same seat rule (the caller's allow list
     covers the profile's), and the budget is carved from the caller's
     remaining budget (at least 20000 tokens a turn, never added; the
     discussion also counts the child's spend, so it is charged twice,
     which only over-counts); depth, live-process cap, machine guard and
     leases apply as for `delegate`;
   - it starts **read-only and stays so**: `--plan-mode` (no Write tool
     exists for it), an empty allow list (the profile's patterns are not
     carried over, so no allow pattern of the profile or of the caller can
     approve a Write) and `TOLE_DISCUSSION_PARTICIPANT=1`; its only Write
     tool is `delegate_report` (one capped file: its contribution);
   - **a Write it wants is denied (fail closed), never routed to the
     caller's allowlist.** The owner's approval surface for a delegated
     child does not exist yet (a headless child has no approver, and the
     `/approvals` queue belongs to serve sessions), so until #714 builds it
     a reviewer cannot write at all. This is stricter than the decision
     above, not weaker;
   - no nested discussions: a session with the participant marker gets no
     delegation host (`Delegator::for_session`), so neither `discuss` nor
     `delegate` is registered for it, at any depth;
   - unknown fields are still refused; the opt-in is `modes` only.

## 12. Non-goals

Chat channels; a public skill marketplace; hosted cloud; heartbeat/"proactive"
mode; wake word / voice; computer-use; multi-tenant org model.
