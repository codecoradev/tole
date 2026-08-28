# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
