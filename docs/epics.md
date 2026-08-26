# tole — Epics & Sprint Plan (6 Weeks)

> Solo dev part-time, capacity ~10 hours/week. English, technical terms as-is.
> Ground truth roadmap: Phase 0 DONE (scaffold + CI). Phase 1a → 1b → 2 → 3.

## Non-Goals (repeated from roadmap)

- **Not** multi-agent orchestration / hierarchical planner.
- **Not** sandbox container execution (Docker/gVisor) — tools run via approval gates.
- **Not** web UI/dashboard — CLI only.
- **Not** a non-LLM-first provider (no rule-based planner).
- **Not** binary distribution beyond the cross-build CI targets mentioned (aarch64-android, aarch64-apple-ios) — no store packaging (Play Store/App Store).

---

## E1 — Storage Layer (JSONL Session File + Storage Trait)

- **Phase:** 1a
- **User Stories:**
  - US1.1: As a developer, I want a JSONL session file (header + append-only commit lines) with a versioned format, so that schema evolution is controlled and sessions are debuggable with standard tools.
  - US1.2: As a developer, I want format versioning in the JSONL header with forward-compat checks on open, so that upgrading the binary does not corrupt old sessions.
- **Acceptance Criteria:**
  - [ ] JSONL format documented (header, entry/register/usage lines) in `docs/` or rustdoc.
  - [ ] Open file with older format version → replay/migrate cleanly (roundtrip test).
  - [ ] Open file with newer format version → hard error with a clear message (test).
  - [ ] All operations go through a single `Storage` trait; no backend-specific code in callers.
- **Est effort:** M (~12 hours)
- **Dependencies:** — (Phase 0 scaffold).

## E2 — State Machine: Program Counter, Seq CAS, Effect Sandwich

- **Phase:** 1a
- **User Stories:**
  - US2.1: As a developer, I want a program counter + sequence number with CAS (compare-and-swap), so that concurrency/double-apply of effects is detected.
  - US2.2: As a developer, I want the effect sandwich pattern (persist intent → execute effect → persist result), so that a crash mid-effect can be resumed deterministically.
- **Acceptance Criteria:**
  - [ ] `seq` increments monotonically; stale write (seq mismatch) rejected with an error, not a silent overwrite (unit test).
  - [ ] Test: crash simulated between "persist intent" and "execute" → resume produces a final state identical to a run without crash (deterministic replay test).
  - [ ] Program counter validates illegal step transitions → error, not panic.
- **Est effort:** L (~16 hours)
- **Dependencies:** E1.

## E3 — Mock Provider + Tier A Tests

- **Phase:** 1a
- **User Stories:**
  - US3.1: As a developer, I want a mock LLM provider (scripted responses), so that the agent loop can be tested without an API key/network.
- **Acceptance Criteria:**
  - [ ] Mock provider follows the same trait as the real provider.
  - [ ] Tier A tests (fast, deterministic, no network) cover: agent loop, state transitions, storage roundtrip.
  - [ ] `cargo test --features mock-provider` green < 30 seconds in CI.
- **Est effort:** M (~10 hours)
- **Dependencies:** E2.

## E4 — `cora search` Tool (ReadOnly) + Real LLM Provider

- **Phase:** 1b
- **User Stories:**
  - US4.1: As an agent, I want a `cora search` tool (ReadOnly, calling cora brain/code search), so that I can query codebase context without mutation risk.
  - US4.2: As a user, I want a real LLM provider (trait impl, e.g. an HTTP client to a configurable provider), so that the agent runs with a real model.
- **Acceptance Criteria:**
  - [ ] Tool registry enforces `ReadOnly` permission — write-capable tools are refused registration without an approval gate (test).
  - [ ] Real provider configured via env/config; API key is never logged (test greps log output).
  - [ ] Integration test (Tier B, network-optional, skipped without a key): single turn end-to-end — prompt → provider → `cora search` tool call → answer.
- **Est effort:** L (~14 hours)
- **Dependencies:** E3.

## E5 — End-to-End Single Turn + Crash-Resume Test

- **Phase:** 1b
- **User Stories:**
  - US5.1: As a developer, I want a full single-turn end-to-end test (mock provider) + crash-resume, so that E2's determinism guarantee is proven at the integration level.
- **Acceptance Criteria:**
  - [ ] Scenario test: kill process mid-turn → restart → run completes with a result identical to a normal run (golden-file compare).
  - [ ] Test enters CI as Tier A (using mock provider).
- **Est effort:** M (~8 hours)
- **Dependencies:** E2, E4.

## E6 — Approval Gates (Approver Trait + Allowlist + Interactive CLI)

- **Phase:** 2
- **User Stories:**
  - US6.1: As a user, I want every Write/execute tool to pass through an `Approver` trait, so that dangerous actions always require approval.
  - US6.2: As a user, I want an `AllowlistApprover` impl (pattern-based) and an `InteractiveApprover` (y/N prompt in the CLI), so that I can choose a limited-automatic or manual mode.
- **Acceptance Criteria:**
  - [ ] All non-ReadOnly tools require an Approver; bypass = compile-time/test-time error path (test: unapproved write is not executed, `denied` event recorded).
  - [ ] Allowlist: glob/pattern match unit tests.
  - [ ] Interactive: CLI prompt shows command + brief diff before y/N.
- **Est effort:** M (~12 hours)
- **Dependencies:** E4.

## E7 — Additional Tools: uteke search (RO), file read (RO), gh CLI (Write)

- **Phase:** 2
- **User Stories:**
  - US7.1: As an agent, I want a `uteke search` tool (ReadOnly) to query memory, a `file read` tool (ReadOnly), and a `gh` CLI tool (Write: issue comment, PR create), so that I can fix a GitHub issue end-to-end.
- **Acceptance Criteria:**
  - [ ] 3 tools implemented each with tests (gh via dry-run/fake binary).
  - [ ] `gh` Write always goes through Approver (E6).
  - [ ] Path traversal on file read is rejected (test).
- **Est effort:** M (~12 hours)
- **Dependencies:** E6.

## E8 — MVP Gate: Fix 1 Real GitHub Issue End-to-End

- **Phase:** 2 (gate)
- **User Stories:**
  - US8.1: As a maintainer, I want the agent to fix 1 real issue (reproduce → fix → test → PR), so that the MVP proves its value.
- **Acceptance Criteria:**
  - [ ] Documented run (log + session replay) on a real issue in our own repo.
  - [ ] PR created via the `gh` tool, CI green, PR merged (or manually reviewed then merged).
  - [ ] Retro write-up: what was lacking → Phase 3 backlog.
- **Est effort:** M (~10 hours, including logged manual intervention)
- **Dependencies:** E7, E5.

## E9 — Hardening, Cross-Build CI, Perf, OSS Prep

- **Phase:** 3
- **User Stories:**
  - US9.1: As a maintainer, I want cross-build CI targets `aarch64-android` and `aarch64-apple-ios` green, so that the mobile path is proven.
  - US9.2: As a maintainer, I want hardening (error handling, tool timeouts, output limits) + a basic perf benchmark, so that it is ready for external users.
  - US9.3: As an external user, I want OSS prep (LICENSE, README, CONTRIBUTING, crate/binary publish), so that I can try it.
- **Acceptance Criteria:**
  - [ ] CI matrix builds those 2 cross targets successfully (build only, no run).
  - [ ] Tool timeout + output truncation tested; panic-free on bad input (fuzz-lite/manual).
  - [ ] Benchmark: single turn mock < documented threshold; results documented in `docs/perf.md`.
  - [ ] Repo public-ready: LICENSE, README quickstart, SECURITY.md.
- **Est effort:** L (~16 hours)
- **Dependencies:** E8.

---

## Sprint Plan (6 weeks, ~10 hours/week)

| Week | Focus | Epic | Deliverable | Checkpoint Review (end of week) | Kill / Pivot Criteria |
|---|---|---|---|---|---|
| W1 | Phase 1a — Storage | E1 | JSONL session file + Storage trait + compaction + tests | Format roundtrip green (replay, torn-line discard, compaction); schema doc exists | If JSONL proves >2x over budget (>20 hours), simplify to in-memory + snapshot only — do not delay. |
| W2 | Phase 1a — State Machine + Mock | E2, E3 | Seq CAS, effect sandwich, mock provider, Tier A green | Deterministic unit-level crash-resume; `cargo test` green in CI | If replay determinism keeps failing for >1 extra week: kill — the core architecture is not sound. |
| W3 | Phase 1b — cora search + real provider | E4, E5 | ReadOnly tool + real LLM provider + E2E single turn + crash-resume test | E2E single turn (mock & real-if-key) green | If the real provider is flaky >50% of runs: switch primary provider, don't debug endlessly. |
| W4 | Phase 2 — Approval Gates | E6 | Approver trait + Allowlist + Interactive CLI + tests | CLI demo: write request → y/N prompt → denied recorded | If the Approver design starts creeping toward complex RBAC: cut it, 2 impls are enough. |
| W5 | Phase 2 — Tools + MVP Gate | E7, E8 | uteke search, file read, gh Write + **MVP: 1 real issue fixed** | PR from agent merged + retro write-up | **MVP gate:** if after 2 intervention sessions the agent still fails completely, evaluate continue vs stop — do not automatically proceed to Phase 3. |
| W6 | Phase 3 — Hardening | E9 | Cross-build CI (aarch64-android, aarch64-apple-ios), timeout/truncation, perf doc, OSS prep | CI matrix green, LICENSE+README ready | If cross-build fails due to a C dependency: record the blocker, defer the target — must not slip beyond W7. |

### Allocation notes

- Buffer: each week, reserve ~2 hours for checkpoint review + backlog grooming.
- Total estimate: E1–E9 ≈ 110 hours vs capacity of 60 hours → **the effort estimate is ideal effort; if the W5 MVP gate is not reached, W6 (Phase 3) is postponed, not compressed.** The MVP gate is the only hard deadline.
