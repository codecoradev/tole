# Automation (cron, scheduler, webhook) — design note

Status: **proposal, nothing implemented.** Scope: unattended runs of tole
(scheduled and event-triggered). Chat channels (Telegram, Slack, …) are
explicitly **out of scope** for this note.

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
- `mode: script` (Hermes `no_agent`): argv via the `run_command` machinery
  (no shell, cwd jail, timeout, output cap), stdout is the result, **zero
  LLM tokens**. Empty output or a `[SILENT]` first line = nothing delivered.
- Per-job `budget` reuses mission budgets (`--max-steps/--max-minutes/--max-tokens`);
  a global daily token ceiling (`cron.daily_token_cap`) stops all agent jobs
  and records `budget`. Model pin per job so cheap work never inherits the chat
  model.
- Agent-created jobs: created `paused`, `created_by` recorded, `allow[]` ⊆
  creator's; enabling is a human action. Config-sourced jobs follow the
  existing rule that faces refuse config-sourced hooks/skills/MCP unless the
  config is trusted.

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
  work needed beyond cron jobs being able to attach MCP servers.

## 8. Surface (mirror Hermes naming)

```
tole cron create "<schedule>" "<prompt>" [--name N] [--script CMD] [--allow write:tool]
         [--model M] [--budget-steps N --budget-minutes N --budget-tokens N]
         [--workdir D] [--skill S] [--catch-up skip|one --grace-minutes N] [--paused]
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
other config commands), and the pull route `GET /cron/runs` on `tole serve`
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

## 12. Non-goals

Chat channels; a public skill marketplace; hosted cloud; heartbeat/"proactive"
mode; wake word / voice; computer-use; multi-tenant org model.
