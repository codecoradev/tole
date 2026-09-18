# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

### Changed
- Retry classification: 429/rate-limit responses join timeouts as
  transient and get the one automatic per-turn retry; the durable audit
  entry is now "provider transient failure, retrying" (#58, #62, #66).
- The turn loop no longer clones the full transcript for every provider
  step; `complete()` borrows the storage slice (#66).

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
