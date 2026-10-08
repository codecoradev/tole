# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.1] — 2026-10-08

### Changed
- The MCP server path (`RegistryServer::execute_checked`) now authorizes tool
  calls through the same crate-internal gate as `drive`/`resume_turn`.
  Behavior change for `RegistryServer::new` embedders: pre-hooks configured on
  an embedder-supplied registry are now enforced there (a configured deny-hook
  must not be bypassable on one path). No in-repo face is affected, since
  none can attach pre-hooks. Destructive refusal and error strings are
  unchanged (#303, part 3 of 3).
- Internal refactor, no behavior change: `drive` and `resume_turn` now share
  one crate-internal tool-call authorization gate (`gate.rs`) with a typed
  denial and a `Permit` required to execute non-ReadOnly tools; tool risk is
  read once per call. Durable entry shapes and error strings are unchanged
  (#303, part 2 of 3).
- **Behavior change:** `agent_poll` is now `Risk::Write` (a successful poll
  consumes the mailbox: it writes `meta.json` and forgets mailbox memories),
  matching the rule that a ReadOnly poll never mutates. `--trust internal`
  allowlists the exact name `agent_poll`, so poll loops stay prompt-free and
  `agent_start` still prompts; users on the `read_only` preset who approved
  `agent_start` are now prompted on every `agent_poll`; plan mode drops it
  together with `agent_start` (#300).

### Fixed
Rescan-2 MAJORs (#276–#287):
- Security: server-supplied text is sanitized before it reaches MCP approval
  prompts (#286); the subprocess env scrubber matches SECRET/TOKEN/
  PASSWORD/API_KEY-shaped names by substring (#284, `TOKENIZERS_PARALLELISM`
  kept); memory `recall` passes the query after `--` (#280); a failed or
  timed-out request-body read on the MCP HTTP face is a 400, not an empty
  body (#277); MCP registration sync waits longer than the serial reactor's
  worst-case queueing (#285).
- Redirects: `web_fetch` resolves relative `Location`s per RFC 3986 against
  the current hop and refuses non-http(s) hops (#281).
- `git` rejects NUL bytes in argv with a clear error (#282).
- **Behavior change:** `AllowlistApprover::new(patterns, Deny|Ask)` now
  ignores `patterns` — the default verdict is final (fail closed). Embedders
  that used `new(p, Deny)` as an allowlist must use `allow_only(p)` (#283).
- `chat` registers and hydrates `todo_read`/`todo_write` (#276); an agent
  mailbox is marked consumed only after a successful recall (#287).
- Tests: the perf `resume_replay` gate actually replays 25 tool calls and
  asserts it (#278); `turn_loop` temp dirs are unique per call (#279).

Architecture-review follow-ups (#293–#299):
- The `gh` tool is registered only when a GitHub origin is detected; the
  hardcoded `codecoradev/tole` fallback on the run/chat/resume/mission and
  `tole mcp` faces is gone (#293).
- `tole serve --transport mcp` honors `--plan-mode` in its server-level
  registry (only ReadOnly tools remain) (#294).
- One session-id rule everywhere: `[A-Za-z0-9_-]`, at most 64 bytes
  (`tole_core::storage::is_valid_session_id`); an id accepted by serve/ACP
  is no longer rejected by `tole chat --resume` / `tole status` (#295).
- `tole acp` caps live sessions at 256 with non-busy eviction (shared helper
  with serve and the MCP session tools) and prunes the approval state of
  evicted sessions (#299).

## [0.7.0] — 2026-10-06

### Added
- `tole upgrade` + startup update notification (issue #220): a
  cache-backed (24 h) banner on startup when a newer crates.io
  release exists (`/releases/latest` redirect primary, API fallback,
  network failures silent, `TOLE_NO_UPDATE_CHECK=1` opts out), and
  `tole upgrade [--check] [--yes]` which resolves the latest version
  from crates.io and re-runs `cargo install tole-cli`, verifying the
  binary afterwards. Non-cargo binaries get an explicit note.
- `web_fetch` / `web_search` (#215): read-only internet access — fetch
  is direct HTTPS (512 KB cap, content-type allowlist, HTML→text), search
  probe-gated on `TOLE_WEB_SEARCH_URL` (fleet backend contract). No
  backend, no tool.
- Run ergonomics (#216): `run --prompt-file <path|->`, `--name <alias>`
  (header-pinned; `tole sessions` shows it, `resume` accepts it), and
  `--timeout <secs>` wall-clock cap (checkpoint cancel — settles
  resumably, never dead).
- Mobile-control guide (#202): docs/mobile-control.md defines the REST
  surface uteke-mobile consumes (sessions, approvals, cancel, cost
  report) with the auth model and cross-repo acceptance; threat model
  gains the phone-as-approval-surface section.
- Remote approvals (#200): serve-face Write approvals become a queue —
  `GET /approvals` + `POST /approvals/{id}/decision` (allow = one-shot
  fingerprint + approvals-only resume; deny = recorded verdict), expiry
  to denied (15 min), durable audit registers, and a CLI consumer
  (`tole approvals list/allow/deny`). MCP/ACP parity follow-up.
- Mission budget tiers + cost report (#201): `--max-tokens` ceiling joins
  `--max-steps`/`--max-minutes`; conservative defaults with `--trust
  internal` headroom (explicit flags win); a durable cost report (steps,
  turns, tokens, wall time, tool-call counts by risk tier) lands on the
  session at every settle path and `tole status` renders it.
- `tole mission` (#199): budgeted autonomous turn-chaining toward a goal —
  `--max-steps` / `--max-minutes` budgets (exhaustion settles resumably),
  optional `--verify <cmd>` gate (exit 0 = completion; failures return to
  the model with output, 3 strikes settle `verify_failed`), durable
  mission summary on the session, `--resume <id>` continuation.
- `todo_write` / `todo_read` (#198): durable mission task list persisted as
  ordinary session entries — `todo_write` echoes the full list as its result
  (the durable record), state re-hydrates from the transcript on
  open/resume, at most one task `in_progress`; `todo_write` is Write
  (covered by `--trust internal`), `todo_read` is ReadOnly.
### Fixed
Pre-tag full-codebase scan gate (all MAJORs triaged valid and fixed):
- Per-session `TodoState` — todo list no longer leaks across concurrent
  serve/ACP sessions (#226).
- MCP server: session-registry-first routing, unknown session id fails
  closed, ambiguity refusal no longer bypassable (#227).
- Child agents: argv depth guard closes quoting/`env -i`/`exec -c`/
  substitution escapes (#228); spawn cap flock-serialized, mailbox
  consumed flag persisted and consume lock held end-to-end (#234, #260).
- Evals tier 2: judges require exit 0 and strip approval-banner echo (#229).
- `git` tool: stdout capped at 20k chars with a truncated flag (#230);
  colon pathspec magic refused in the add jail (#247).
- Turn loop: poll steps exempt from `MAX_STEPS`, trait-driven `is_poll`
  (#231); replay approval gate keyed to current tool risk (#249).
- OpenAI streaming: usage/reasoning reset per response (#232).
- `gitea` tool surfaces 4xx/5xx error bodies (#233).
- `verify_package`: length-guarded edit-distance, bare-name typo
  comparison (#235).
- Hardening: storage session-id charset enforced at the boundary (#248);
  web redirect cap is a hard error (#250); MCP result cap counts
  separators (#251); `gh` tool probe-gated, no hardcoded repo (#254);
  session list uses `try_lock` busy flags (#256); systemone caps keyed on
  exact URL authority; approval URLs refuse plaintext `https://`
  downgrade (#246); monotonic counter in approval entry ids; bounded
  detached ACP `/models` probe; `tole mission` exits nonzero on non-done
  statuses and refuses unsupported hook/memory flags.

## [0.6.0] — 2026-10-06

### Added
- `gitea` tool — the Gitea counterpart of `gh`, over the instance's
  REST API (`TOLE_GITEA_TOKEN` / `GITEA_TOKEN` + a Gitea `origin`
  remote; probe-gated: absent token or non-Gitea remote degrades to one
  warning, no phantom tool). Same six ops as `gh` (issue_view /
  issue_list / pr_view read-only; issue_comment / issue_create /
  pr_create writes), Risk::Write through the approval gate, routes
  whitelisted in one auditable function, per-call `repo` override
  validated (`owner/name`, traversal/dash refused), token never shown
  in approval lines. Self-hosted instances with explicit ports are
  parsed from the remote (`http://host:3000/owner/repo`); GitHub
  remotes never register it (that's `gh`).
- `gh` hardening: `number`/`limit` accept JSON integers (models send
  counts as numbers); optional per-call `repo` override (validated,
  shown in the approval line) replaces the registration-time lock —
  closing the README "per-repo wiring is a known gap" note.
- `mcp-http` is now a **default feature** of `tole-cli`: a plain
  `cargo install tole-cli` / `cargo build -p tole-cli` exposes all four
  documented faces, including `tole serve --transport mcp` (previously
  opt-in — a registry install errored on that transport). Embedders are
  unaffected: `tole-core` defaults do not change, and the mobile
  cross-compile targets only build `tole-core`.
- `--trust` presets (#160): one-word trust for the fleet's ecosystem
  tools — `internal` (uteke_*/cora_search/mcp_cora_*/verify_package/
  job_*/tole_session_*), `read_only` (every safe read), `none`
  (default). Pure sugar over `--allow` globs; Destructive is never
  auto-allowed; unknown presets fail loudly; flag wins over the
  `TOLE_TRUST` env. (Landed on develop 2026-10-02, backfilled here.)
- SKILL.md support (#161/#162): `--skill <path>` loads a skill file
  into the system prompt (fenced, 16 KiB cap, loud frontmatter errors);
  discovery over `<workspace>/skills/` + `~/.codecora/tole/skills/`
  registers the ReadOnly `load_skill` tool with a one-line index;
  `--no-skills` disables everything. (Landed on develop 2026-10-02,
  backfilled here.)

- `systemone_decide` (#172/#173) — typed decisions (choice / score /
  noul + confidence) from any System One backend, active when
  `SYSTEMONE_API_KEY` is set; `SYSTEMONE_BASE_URL` picks the backend
  (hosted Jev default, self-hosted compatible). ReadOnly, approval-free.
- Depth-1 child agents (#171/#174): `agent_start` / `agent_poll` spawn
  and harvest durable child sessions from within one session —
  structurally no grandchildren (registry depth cap), results via
  ephemeral per-child uteke mailboxes, optional per-child git worktrees
  behind the parent-only `--agents-worktree` flag.
- ACP native model picker + per-session approval controls (#176/#177/#179):
  `TOLE_MODELS` (comma-separated ids) advertises a model switch via ACP
  session config options, persisted durably per session across resumes;
  an approval selector (`ask` / `auto`, session-scoped) plus an
  `allow_always` option on Write permission requests let the editor
  relax the gate without weakening the Destructive tier.
- Cooperative cancellation on all three server faces (#178/#184/#185):
  ACP `session/cancel`, REST `POST /sessions/{id}/cancel`, and the
  `tole_session_cancel` multi-session MCP tool. All paths set the same
  per-session token; the blocking turn observes it at checkpoints and
  settles `stopReason: "cancelled"` as a normal durable turn end.
  Spec-conformant: single in-flight tool calls are not interrupted,
  unknown sessions 404, cancelling an idle session is idempotent;
  `--trust internal` covers the new tool via the `tole_session_*` glob.

### Fixed
- **`TOLE_TRUST` env was documented but never read** (found by
  activation testing 2026-10-05): `--trust` help says "flag wins over
  the TOLE_TRUST env", yet no code read the variable — an env-only
  setup silently kept prompting for fleet tools. The env now applies
  whenever no `--trust` flag is passed (comma/whitespace separated for
  multiple presets; an explicit flag still wins wholesale; unknown
  presets in the env fail loudly, same as the flag).
- Full-feature sweep findings (2026-10-05):
  - **Silent no-op flags on the server faces** now fail loudly at
    startup (the scan-3 #9 rule, previously enforced only for
    pretool/posttool hooks): `--skill`/`--no-skills`/`--mcp-server` on
    `tole mcp`/`serve`/`acp`, plus `--on-pretool`/`--on-posttool` on
    `tole serve` (missed by the earlier bail) and `--on-turnend` on
    `tole mcp`. The cora AUTO-PRESET does not trip the `--mcp-server`
    refusal — only explicit flags do.
  - `tole serve` (both transports) and `tole acp` now honor an explicit
    `--sessions-dir` (default stays the per-session-cwd layout);
    `tole serve --transport mcp` now honors `--workspace` as the
    jail-of-jails root (default stays the server cwd); `--on-turnend`
    stop gates are wired into serve/acp session turns (the same
    registry-level gates the run host uses).
  - **Multi-session MCP ambiguity refusal** (the #138-documented
    behavior, previously unimplemented): a registry-tool call WITHOUT
    `session_id` while 2+ sessions are open is refused with a clear
    error instead of silently executing against the server-level
    registry (server-cwd jail).
  - Skills discovery now defaults to the CURRENT DIRECTORY when
    `--workspace` is absent — the same default the file-tools jail
    uses (a project with `<cwd>/skills/` used to silently get no
    discovery).
  - `--on-pretool` help text corrected: an exit-2 deny parks the turn
    resumably at the denial (mirroring an approval denial); it does
    not auto-replan in-flight.
- **MCP-over-HTTP accept-loop hardening (#190):** the `tole serve
  --transport mcp` face now matches the REST face's #136 Wave-2
  controls — a 32-connection semaphore cap (refused at capacity),
  a 30s header-read timeout on the HTTP/1 connection, and a 30s
  bound on the auth-header and request-body read phases. The
  connection stays on hyper's raw HTTP/1 builder (H1-only — no h2c
  sniffing) with hyper_util's TokioTimer wired via the builder's
  own `.timer()`: hyper panics per connection when
  `header_read_timeout` is set without a timer (caught by the
  pre-tag live smoke test, invisible to CI). SSE response
  streaming is intentionally untouched: long
  `tole_session_prompt` turns still stream incrementally; only
  connection establishment and request intake are time-bounded.
  Live-verified: 401 without a token, initialize over SSE, the
  h2c preface refused, the 33rd idle connection reset at the cap,
  recovery after release. Closes the slowloris
  task/fd-pinning class the full-codebase scan
  flagged as MAJOR (scan finding #17, pre-0.6.0-tag gate).
- Build/CI fixes riding the same train: the no-`mcp` `tole-cli`
  profile compiles again (`mcp_server_command` is now gated behind the
  `mcp` feature, #175/#180); project-sync board lookup resolves the
  runtime Done-option id instead of a hardcoded value, and the
  `set_done` mutation takes `optionId` as `String!` (the previous `ID!`
  call failed against the live Projects API) — failures are now loud,
  not silent (#181/#182, #183).

## [0.5.0] — 2026-10-02

### Added
- `verify_package` tool (#144, ReadOnly): checks a package name against
  the crates.io / npm registry before any install — the slopsquatting
  defense (hallucinated names get pre-registered with malware; 43% of
  hallucinated names recur, 13% are one character from a real package).
  Not-found answers include registry candidates; one-character
  candidates surface a typo-squat warning; 429/5xx honestly report
  `rate_limited` (never "not found"). The default system prompt tells
  the model to verify before installing.
- Turn-end stop gates (#145, `--on-turnend`, default OFF): deterministic
  script gates that fire when the model produces its final message,
  BEFORE it commits. Deny (exit 2) forces continuation — the reason
  becomes the model's next input and the loop re-runs (the Claude Code
  "stop hook / gate forces the model to fix it" pattern). Bounded: 3
  denials per turn then the turn settles durably as `StopGateBlocked`
  (prompt-resumable). GATE semantics: exit 0 = pass, ANY non-zero exit
  = deny with stdout as the reason (a verification gate's exit code is
  its verdict — cargo check exits 101, tests exit 1); only true
  infrastructure failures (spawn error, timeout) stay non-blocking;
  30s per-hook timeout. Payload carries the final-text preview (8 KiB)
  and the per-turn tool/risk summary.
- Memory-loop decision typing (#143): a session that executed any
  Write/Destructive tool is stored to uteke with `--type decision` and
  a `wrote` tag (plain context sessions unchanged). The flag is durable
  and session-scoped — set on fresh execution AND crash-replay of a
  Write, never reset by turn machinery — and derived from tool risk,
  not model claims. Summary content contract is unchanged: first
  prompt + final answer only.

## [0.4.0] — 2026-10-01

### Added
- Harness memory loop (`--memory uteke` / `TOLE_MEMORY`): on the first
  turn of a fresh session, memories relevant to the prompt are recalled
  from the owner's uteke store (namespace `repo-<dir>`,
  `TOLE_MEMORY_NAMESPACE` to override) and injected into the user
  message inside a clearly marked fenced block — the durable log stores
  exactly what the provider saw. When the session settles with a final
  answer, a compact summary is stored back (`--type context`, tagged
  `tole,session`). Host-initiated on both ends (the model cannot
  trigger or suppress it); every failure degrades to stderr and the
  turn proceeds without memory.
- cora auto-preset: when the `cora` binary is on PATH, tole attaches the
  local `cora mcp` server automatically — the full code-intel surface
  (brain search, callers, impact, affected tests, dead-code, review;
  registry names `mcp_cora_*`). Opt out per run with `--no-auto-mcp`;
  an explicit `--mcp-server cora=...` replaces the preset for that name.
  MCP tools keep the never-trusted trust model: `Risk::Write`, approval
  gate always applies.
- The `tole-cli` default feature set now includes the `mcp` client
  (`default = ["shell-tools", "mcp"]`): the auto-preset and
  `--mcp-server` work on a plain `cargo build -p tole-cli`. Embedder
  profiles are unaffected — `tole-core` defaults do not change.

### Added
- `tole serve --transport mcp` — **multi-session MCP over Streamable
  HTTP** (issue #137): one authenticated MCP connection addresses N
  durable tole sessions. Session tools (`tole_session_new`,
  `tole_session_prompt`, `tole_session_status`, `tole_session_list`)
  ride alongside the registry tools; a `session_id` argument routes a
  tool call to THAT session's registry (workspace jail + approver), and
  an ambiguous no-id call with 2+ open sessions is refused. Served with
  hyper directly (no axum) behind the same bearer-token auth as REST;
  Destructive stays structurally absent (non-interactive approvers).
- `tole serve`: tole as a **token-authenticated HTTP daemon** (issue
  #96, v1) — REST endpoints for the session host: create/list sessions,
  run turns, poll status. Turn execution is serialized per session
  (concurrent prompts get 409), the session map + durable JSONL live
  server-side, and the allowlist approver keeps Destructive tools
  structurally absent (a server has no human to ask). Zero new
  dependencies (hand-rolled HTTP/1.1). Per-connection read/write
  timeouts (30s). MCP-over-HTTP is the follow-up.
- `tole acp`: tole as an **Agent Client Protocol agent** over stdio
  (issue #95) — editors (Zed et al.) drive durable tole sessions:
  `session/new`/`session/load` map to the JSONL session store, prompts run
  full tole turns, the final answer streams as an `agent_message_chunk`,
  and Write/Destructive tool calls surface as
  `session/request_permission` requests — the editor human approves, with
  Destructive consent being a genuine per-call decision. Provider config
  is only required when a prompt actually runs. The session map survives
  across prompts (a first-run regression where fresh state replaced the
  map after one turn — caught by CodeCora review — is fixed, along with
  session-id path-traversal and mutex-poisoning hardening). The
  session-map lock is held only briefly — a running turn keeps its OWN
  storage lock, so the reader loop stays live for permission routing
  (the first implementation deadlocked protocol routing for up to the
  permission timeout whenever a client opened a session while a
  permission request was pending — caught by CodeCora review). Sessions
  reject concurrent turns (busy) and panic-safe un-busy via Drop.
- `tole mcp`: tole as an **MCP server** over stdio (issue #94) — the
  registry's hardened tools (jailed file ops, argv-validated git, detached
  jobs, memory loop, cora/uteke integrations) become callable by any MCP
  client. ReadOnly tools always callable; Write tools pre-authorized via
  `--allow` patterns (Destructive structurally absent — registration
  behind a non-interactive approver is refused). Verified live: 11 tools
  listed, read_file round-trip, write without `--allow` denied with an
  actionable message.

### Fixed
- **write_file had no wire schema**: it was the only registered tool
  without a `spec()` override, so providers received a property-less
  schema and could legally answer `arguments: {}` — every write failed
  with "missing 'path'" and identical retries tripped the loop guard
  (found live, GLM via bifrost). Spec declares path+content required;
  regression test pins it.
- CodeCora scan triage (2026-09-18, 54 files): 8 of the 10 MAJOR findings
  fixed — derived `Debug` on `OpenAiConfig` leaked `api_key` via `{:?}`
  (manual redacting impl); the uteke recall query could inject CLI flags
  (leading-dash guard); the uteke room-link spawn skipped env scrubbing;
  `job_poll` (ReadOnly) truncated/rewrote the job log (bounded read only —
  behavior change: runaway logs are no longer trimmed on poll; clear them
  as an operator); the MCP result cap was applied after a full block copy
  (incremental, single-oversized-block safe); subprocess capture is capped
  at 32 MiB per stream with a marked truncation suffix, and the drain no
  longer blocks indefinitely on a grandchild holding the pipe (2s grace —
  unterminated output is dropped); the `sh -c` payload scan now recurses
  and strips shell quotes (`sh -c "/bin/rm -rf /"` and nested-shell
  payloads are refused); `gh pr_create` actually passes the advertised
  `head` field (validated like `base`). Remaining MAJOR/MINOR findings are
  tracked on the scan-triage issue.
- docs: architecture.md no longer says the Destructive tier may be
  allowlisted (contradicted the never-allowlistable invariant).

### Fixed
- Remaining CodeCora scan findings (2026-09-18 sweep): `scrub()` no
  longer mangles text when the secret is empty; non-string tool-call
  arguments are serialized instead of silently replaced with `{}`;
  `OpenAiProvider` reuses one ureq agent (connection reuse) instead of
  building one per request; failed startups no longer leave a stray
  empty session file (registry/provider are built before the session is
  created); `resume <id> "<prompt>"` now stores the memory summary like
  `run`; chat states explicitly when a typed message was dropped after
  exhausted mid-flight retries; session ids use `strip_suffix` (a
  `x.jsonl.jsonl` file no longer yields an unusable id);
  `binary_available` honors the executable bit (unix) / `.exe`
  (windows); edit_file's approval line shows the actual old→new change;
  edit_file temp files are unique per attempt and legacy stale temps are
  swept; delete_file on a symlinked path removes the LINK, not the
  referent; job_start kills the spawned job when the pid file cannot be
  written; MCP tool-request descriptions truncate without materializing
  the whole payload, and the transport-error class shares one constant;
  tole-cli is now lib+bin so integration tests drive the REAL jailed
  tools and approver instead of drifting re-implementations; evals
  Tier 2 runner is Python 3.9-compatible, cleans its mkdtemp session
  dirs, uses a portable timestamp, and the baseline diff no longer
  flags newly-passing missions as regressions or skips zero baselines
  silently; threat-model ENV/JOB-2 rows updated to match the code.

### Changed
- `cora_search` follows the same startup-probing contract as the uteke
  tools: a missing `cora` binary degrades to a one-line warning instead
  of registering a phantom tool, and the native single-tool fallback is
  skipped when the cora MCP surface is attached.

## [0.3.0] — 2026-09-12

Reliability & long-running work: the post-soak optimization batch driven by
live E2E findings (2026-09-11 vetio missions + 2026-09-12 optimization pass).

### Added
- MCP client (`mcp` feature): connect external MCP servers over stdio
  via `--mcp-server name=command [args...]` and use their tools from
  the registry. Trust model: every MCP tool is Risk::Write (approval
  gate always fires — server metadata is never trusted), results are
  fenced like native tool output, server env is scrubbed (#74, #77).
- `--workspace <dir>` global flag — the file tools' jail root
  (read_file/write_file/edit_file/delete_file) is now configurable;
  defaults to the process cwd. Canonicalized and strictly validated; the
  TOCTOU-safe jail walk is unchanged (#57, #63).
- `job_start` / `job_poll` tools — detached long-running jobs (own process
  group, stdin null, stdout+stderr to `tole-jobs/<id>/log` inside the
  workspace) with ReadOnly liveness+log-tail polling; zombie states count
  as finished and reads are bounded to an 8 KiB window (#59, #56, #64).
- `tole resume <id> "<prompt>"` — continue a settled session with new
  instructions; headless multi-mission flows keep one durable session.
  Bare `resume` keeps the approvals-only recovery semantics (#55, #65).
- Usage ledger wiring: `Provider::last_usage` (default `None`) +
  `OpenAiProvider` response-usage capture; one durable `UsageRecord` per
  provider step anchored to the last committed entry — `tole status` now
  reports real token totals instead of 0/0 (#66).
- Live-mission binary hygiene rules in CONTRIBUTING.md (#60, #61).
- Agentic eval harness (issue #73, #76): Tier 1 deterministic contracts
  wired into a dedicated Evals CI job (wire-shape stability = prefix-
  cache contract, resume equivalence, abort-path resumability, approval
  matrix); Tier 2 live BYOK missions scored on success/steps/usage-
  ledger tokens; Tier 3 per-release baselines with a >25%-regression
  diff gate (`evals/`).

### Changed
- Retry classification: 429/rate-limit responses join timeouts as
  transient and get the one automatic per-turn retry; the durable audit
  entry is now "provider transient failure, retrying" (#58, #62, #66).
- The turn loop no longer clones the full transcript for every provider
  step; `complete()` borrows the storage slice (#66).

### Fixed
- `AllowlistApprover` constructor semantics (#70): documented the
  decision pipeline (pattern match → Allow; `default` applies only to
  NON-matches, so `new(vec!["x"], Decision::Deny)` ALLOWS "x") with a
  regression test, and added unambiguous `allow_only`/`deny_only`
  constructors — the misleading shape that let the guarded-replay
  livelock test look correct for months.
- Replay safety derives from tool risk (#68): a crash mid-sandwich no
  longer blindly re-executes a Write/Destructive tool on resume —
  Guarded intents require fresh approver consent, and a denied/absent
  approver settles the intent instead of livelocking the session.
- File-tool jails validate by path components (#68): Windows
  drive-prefixed/rooted relatives can no longer escape via `join`, and
  benign names containing ".." are no longer falsely rejected.
- JSONL torn-line recovery (#68): a newline-terminated corrupt final
  line is truncated on open instead of surviving and permanently
  bricking the session on the next append.

## [0.2.0] — 2026-08-27

Chat-first replan (v1.1): tole becomes a durable conversational harness —
the chatbot core for the uteke-mobile integration (all-Rust, runs inside
Flutter via flutter_rust_bridge, LLM BYOK).

### Added
- `tole chat` — durable multi-turn REPL: one session file, many turns;
  `/exit`, `/status` commands; `--resume <id>` / `--last`; state-aware
  dispatch that resolves interrupted turns (bounded retries) so a failed
  turn never wedges the conversation (#42).
- System prompt (B2): `--system` flag > `TOLE_SYSTEM_PROMPT` env > none;
  pinned in the session header, replayed on every resume — persona
  survives process restarts (#43).
- `tole sessions` — directory listing (id, pc, seq, turns, mtime) and
  enriched `tole status` (turns, pinned-prompt indicator, usage totals) (#44).
- `run_command` tool — generic dynamic commands: shlex-style argv split
  (no shell), cwd-jailed, hard timeout, `Risk::Write` ceiling; approval
  prompt shows the exact argv (#45).
- uteke first-class tools: `uteke_recall` (room-scoped; renamed from
  `uteke_search` so the name matches the verb — the old spec advertised
  a `room` parameter that was never wired) and `uteke_document`
  (markdown → document → room link, content via stdin) (#45).
- Startup probing: CLI-backed tools register only when their binary
  exists — no phantom tools (#45).
- `shell-tools` feature gate: subprocess-backed tools compile out for
  embedders (`--no-default-features`) — mobile/FFI profile (#47).
- `git` tool: status/diff/add/commit with path-jail validation and a
  dedicated 120s commit timeout (pre-commit hooks) (#41).
- `gh` read ops: `issue_view`, `issue_list`, `pr_view` (#39).

### Fixed
- Chat mid-flight resolve no longer drops the user's freshly typed
  message when the interrupted turn fails to clear (#42 follow-up).
- `uteke_document`/`uteke_recall`: pipe-deadlock on large documents and
  missing subprocess timeout (new shared `run_with_timeout_stdin`
  helper); room/slug argv-flag-injection validation (#45 follow-up).
- `git add` path jail: absolute paths and `..` traversal rejected (#41 follow-up).

### Changed
- Environment contract: `TOLE_*` canonical, `OPENAI_*` fallback;
  `CORAGENT_*` removed entirely (pre-release cleanup) (#36–#38).
- Removed unused `tokio` and `rusqlite` dependencies from tole-core —
  the core is synchronous by design and storage is JSONL (#46).

## [0.1.0] — 2026-08-26

Initial release: durable agent harness foundation.

### Added
- Write-once JSONL session storage with crash-safe replay (torn-tail
  truncation), state machine, effect sandwich (intent → effect →
  settlement), approval gates with risk tiers (ReadOnly / Write /
  Destructive), scoped pre-auth allowlists (`--allow`, E6/A1),
  OpenAI-compatible provider with tool calling, crash-resume,
  step-budget and loop guards, secret redaction on the wire,
  file tools (`read_file`, `write_file`, `edit_file` with hashline
  anchoring, `delete_file`), `cora_search`, E8 MVP gate passed.

