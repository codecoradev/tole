# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] — 2026-09-12

Reliability & long-running work: the post-soak optimization batch driven by
live E2E findings (2026-09-11 vetio missions + 2026-09-12 optimization pass).

### Added
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
