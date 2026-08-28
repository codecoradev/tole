# tole — Epics & Roadmap (v1.1, chat-first replan)

> Ground truth: Phase 1a/1b/2 complete (E1–E8, E10–E12, E4.5, gh read ops,
> TOLE_* env). 114/114 tests, CI 12/12. Replan 2026-08-27: chat-first
> direction per PRD v1.1 §0 Identity.

## Identity recap (PRD v1.1)

**tole = a durable conversational harness** — a personal-assistant chatbot
that can call tools, with persistent resumable sessions. Light coding via
tools (files + git + gh). Optional first-class integrations: uteke (memory,
documents), cora-code (code intel). Dynamic tools via a generic
approval-gated `run_command` — no config DSL.

## Completed (historical, do not re-plan)

E1 storage · E2 state machine · E3 mock provider · E4 cora search + real
provider · E4.5 native tool calling · E5 crash-resume · E6 approval gates ·
E7 tool suite · E8 MVP gate (real issue fixed, retro in `docs/e8-retro.md`)
· E10 loop guard · E11 secret redaction · E12 file CRUD (hash-anchored
edit) · gh read ops · TOLE_* env canonical.

---

## Track A — Coding-lite completion (small, closes old promises)

### A1 — Scoped pre-authorization
- **What:** `--allow <risk>:<tool>` (repeatable), e.g. `--allow write:edit_file`.
  Pre-authorizes specific Write tools for headless/CI runs.
- **Hard rule:** Destructive is **never** allowlistable — structural
  enforcement stays (registry + AllowlistApprover deny on sight).
- **Acceptance:** headless run completes with edit_file pre-authorized
  while other Write tools still deny; Destructive attempt with allow flags
  still denied. Unit tests for the flag parser + approver wiring.
- **Est:** S

### A2 — Git tool (light)
- **What:** `git` builtin: `status`, `diff`, `add`, `commit` ops, argv
  whitelist discipline identical to gh.rs (validated per-op args, no
  free-form flags). Push stays manual.
- **Why:** closes the last maintainer intervention in the coding-lite loop
  (branch/commit). Also the natural companion for the markdown→uteke
  document workflow (write_file → git commit → uteke_document).
- **Acceptance:** agent commits a change end-to-end in a sandbox repo;
  injection attempts (`--upload-pack`, leading-dash messages) rejected;
  fake-git e2e test (same pattern as fake-gh).
- **Est:** M

---

## Track B — Chat-first (the product direction)

### B1 — Chat mode (`tole chat`) — keystone
- **What:** multi-turn REPL: prompt → agentic turn (durable, exactly like
  `run`) → answer → next prompt. `--resume <id>` / `--last` to continue.
  Explicit resume (no implicit auto-continue) — deliberate state
  transitions. Clean EOF/`/exit`, Ctrl-C safe (resume-able mid-turn).
- **Acceptance:** multi-turn conversation in one session; SIGKILL mid-chat
  → `tole chat --resume <id>` continues with full context; `/exit` leaves a
  cleanly closed session.
- **Est:** M

### B2 — System prompt & agent identity
- **What:** resolution `--system` flag > `TOLE_SYSTEM_PROMPT` env >
  default. Persisted in the session header (once, not per-turn — token
  efficient, replay-accurate). Analysis note: persona must stay stable
  across resume; sessions recorded pre-B2 (no system prompt) must remain
  replayable (backward-compat test).
- **Acceptance:** persona consistent across turns and resume; old session
  replay still green.
- **Est:** S–M

### B3 — Session UX
- **What:** `tole sessions list` (id, age, turn count, last-message
  preview, usage) and `tole sessions show <id>` (timeline view). Branching
  deferred.
- **Acceptance:** list + inspect without opening JSONL by hand.
- **Est:** S

### B4 — Dynamic tools: generic `run_command` + startup probing + uteke first-class
- **What:**
  1. **`run_command` builtin**: input `{ "command": "..." }` → shlex-parsed
     to argv (no shell), cwd-jail, subprocess timeout + output cap. Risk
     ceiling = Write (Destructive unreachable). Approval prompt shows the
     exact argv; headless via A1 pre-auth.
  2. **Startup probing**: builtins whose binary is missing (gh/uteke/cora)
     are skipped with one warning line — no phantom tools burning turns.
  3. **uteke first-class**: `uteke_document` (save markdown into a room),
     plus existing recall. Renames `uteke_search` → `uteke_recall`
     (name/behavior consistency).
- **Why:** no TOML DSL (rejected — poor UX); any host CLI becomes usable
  with zero recompilation; uteke is the primary assistant integration.
- **Acceptance:** agent uses run_command to invoke a non-builtin binary
  (e.g. `figlet`) approval-gated; missing-binary builtin skipped with
  warning; uteke_document saves a markdown doc into a room (validated
  against the uteke CLI); rename with spec + tests updated.
- **Est:** M–L

---

## Track C — Deferred (revisit after Track B)

E9 hardening/OSS-prep (issue #9): cross-build CI (aarch64-android,
aarch64-apple-ios), timeout/truncation fuzz-lite, perf doc, LICENSE/README
publish. MCP integration if a non-CLI tool need appears. Session branching.

---

## Execution order

```
A1 → A2 → B1 → B2 → B3 → B4 → (Track C revisit)
```

A-track first: small, closes the coding-lite promises and unblocks headless
testing patterns reused by B4's run_command. B1 is the keystone; B2 touches
the session header (do it before B3/B4 so the format migrates once).
