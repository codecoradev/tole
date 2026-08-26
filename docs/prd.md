# PRD — cora-agent

| | |
|---|---|
| **Product** | cora-agent — durable Rust agent harness |
| **Repo** | `codecoradev/cora-agent` (MIT, adaptation of pi harness spec) |
| **Document version** | v1.0 (Phase 0) |
| **Status** | Draft — approved before Phase 1 coding |
| **Owner** | Anaz (internal dev) |

---

## 1. Problem Statement

Current LLM agents (pi, Claude Code, etc.) are **ephemeral**: state lives in memory, a crash means loss of context, and there is no replayable audit trail. For internal CodeCora workflows that need **durability, safety, and auditability** (fixing GitHub issues, responding to cora-review findings, documentation Q&A), existing harnesses fall short:

1. **Not durable** — a crash mid-turn repeats work from scratch; expensive tool calls are re-executed.
2. **No approval gates** — tools execute directly; there is no ReadOnly/Write/Destructive tiering.
3. **Not embeddable** — monolithic CLI harnesses; Corin (desktop) and mobile (Flutter) cannot reuse the same core.
4. **Not integrated with the Cora ecosystem** — cora search (code graph), uteke (memory), and gh CLI are not first-class.

### Goals

- Rust harness with a **write-once conversation tree** (append-only entries, immutable) persisted as **one JSONL file per session** (industry pattern: Claude Code, Codex, pi).
- **Register state machine** — namespaced mutable cells (`lane/op/pending/fact`) as the operation program counter.
- **Risk-tiered approval gates**: ReadOnly / Write / Destructive.
- **Embeddable core**: platform-agnostic library (no stdin/stdout), host = CLI / Corin / flutter_rust_bridge FFI.
- Cora ecosystem integration: cora search tool, uteke memory, gh CLI.

### ⛔ Gate: 3 Internal Workflows must be defined BEFORE Phase 1 coding

| # | Workflow (placeholder) | Description | Status |
|---|---|---|---|
| W1 | `fix-gh-issue` | Agent reads a GitHub issue via gh CLI → cora search to locate code → patch → create PR. | **[DEFINE BEFORE PHASE 1]** |
| W2 | `cora-review-responder` | Agent receives findings from cora review diff → triage severity → draft PR comment reply. | **[DEFINE BEFORE PHASE 1]** |
| W3 | `docs-qnA` | Internal documentation QnA via uteke recall + cora brain search → answer with citations. | **[DEFINE BEFORE PHASE 1]** |

> Every workflow must have: a written trigger, tool call steps, maximum risk tier, and expected outcome. Without these, Phase 1 must not start.

---

## 2. Non-Goals (v0)

| Non-goal | Reason / when to re-evaluate |
|---|---|
| Multi-lane (parallel lanes) | v0 = **single lane**; lane register is prepared but only 1 active. Multi-lane post-MVP. |
| TUI / interactive UI | Host provides the UI; the core makes no UI assumptions. |
| Parallel tool execution | Tool calls are sequential per turn; simplifies the state machine. |
| Deferred redemption | Approvals must resolve synchronously at the gate; no redemption queue. |
| Cross-build CI (macOS/Windows) | Linux-only until **Phase 3**; Linux lint+test CI first. |

---

## 3. User Personas

| Persona | Needs |
|---|---|
| **Internal dev (Anaz)** | Run automated workflows via CLI; crash-safe resume; audit log; approval gates for risky operations. |
| **Embed host: Corin (desktop, Tauri)** | Embed the core crate as a library; interactive Approver via Corin UI. |
| **Embed host: Flutter mobile** | FFI via flutter_rust_bridge; minimal & serializable API surface. |

---

## 4. Functional Requirements (per Module)

| Module | ID | Requirement | Priority |
|---|---|---|---|
| **entry** | FR-E1 | Entry = write-once node on the conversation tree; once written it cannot be changed/deleted (immutable, append-only). | P0 |
| **entry** | FR-E2 | Entry stores role, payload (message/tool-call/tool-result), parent link, UUIDv7. | P0 |
| **register** | FR-R1 | Namespaced mutable cells: `lane`, `op`, `pending`, `fact`; written atomically with entries in one commit. | P0 |
| **register** | FR-R2 | The register is the only mutable state; acts as the operation program counter. | P0 |
| **state** | FR-S1 | Operation state machine with explicit transitions (e.g. idle → running → awaiting-approval → running → done/failed); invalid transitions are rejected. | P0 |
| **state** | FR-S2 | After a crash, the state machine + register are restored by replaying the session file and the operation resumes from the checkpoint. | P0 |
| **storage** | FR-D1 | JSONL backend, **one file per session** (default); optional SQLite backend behind the same `Storage` trait (feature-gated). | P0 |
| **storage** | FR-D2 | Versioned schema (`STORAGE_VERSION`), **migrate-on-open**; version newer than the binary → clear error. | P0 |
| **tool** | FR-T1 | `Tool` trait with risk tiers: `ReadOnly` / `Write` / `Destructive`; metadata (name, description, argument schema) exposed to the provider. | P0 |
| **tool** | FR-T2 | Tool execution is sequential; results (including errors) are recorded as tool-result entries. | P0 |
| **approval** | FR-A1 | `Approver` trait: the core provides the policy engine; `Allowlist` / `Interactive` impls live in the host. | P0 |
| **approval** | FR-A2 | Tool calls with tier ≥ configured threshold must pass the approver before execution; decisions are recorded in the tree. | P0 |
| **provider** | FR-P1 | `Provider` trait abstracting the LLM (stream/chat completion); concrete implementations in the host or a separate crate. | P0 |
| **provider** | FR-P2 | Provider receives a tree snapshot (or window) + tool metadata; never writes the register directly. | P0 |

> Note: `provider` currently has no stub file in core (lib.rs documents it) — create `provider.rs` in Phase 1a.

---

## 5. Risk Tiers & Approval Matrix

| Tier | Definition | Example tools | Default approval |
|---|---|---|---|
| **ReadOnly** | Does not change world state | cora search, uteke recall, gh view | Auto-allow (allowlist) |
| **Write** | Changes state, reversible | patch file, gh pr create, uteke remember | Approver required (auto-allow for internal allowlist) |
| **Destructive** | Not reversible / broad impact | force push, drop database, delete branch, `rm -rf` | Approver required, **always interactive** for internal dev; cannot be allowlisted |

Decision matrix:

| Tool tier \ Approver | Auto-allow | Interactive |
|---|---|---|
| ReadOnly | ✅ execute | ✅ (may prompt) |
| Write | ✅ if in allowlist | ✅ prompt |
| Destructive | ❌ rejected | ✅ prompt + explicit confirmation |

---

## 6. Roadmap & Definition of Done

| Phase | Scope | DoD (verifiable) |
|---|---|---|
| **0 — Stubs & spec** (done) | Crate layout, stubs, spec.md | ✅ 7 stub files + lib.rs; green workspace build |
| **1a — Store + State machine + Mock provider** | entry, register, state, storage (JSONL default + in-memory), tool trait, approval trait, mock provider | Unit tests pass: (1) entry append-only enforced, (2) crash mid-op → restart → replay JSONL → resume from checkpoint (torn-last-line discarded whole), (3) invalid state transitions rejected, (4) compaction roundtrip (rewrite + atomic rename, seq gaps legal). Demo CLI: mini-loop with mock provider + 1 ReadOnly tool. |
| **1b — cora search tool + real LLM** | cora search tool (ReadOnly), real provider (LLM via API), W1 `fix-gh-issue` workflow | E2E: W1 runs fully in CLI with real LLM; cora search tool call recorded in tree; Write approval gate triggered on patch. |
| **2 — Workflows 2 & 3** | W2 cora-review-responder, W3 docs-QnA (uteke), gh CLI tools, allowlist policy | E2E: W2 & W3 fully complete; audit replay (read tree → timeline) available. |
| **3 — Hardening & embed prep** | Cross-build CI (macOS/Windows), FFI surface, public API docs, crate publication | Green CI matrix; minimal embed example (Corin spike or FFI test) compiles; complete API docs. |

### Roadmap positioning (strategic)

```
Internal tool (Phase 0-2) → OSS core release post-MVP (Phase 3) → Corin embed (CMO) → Flutter mobile
```

The core is always library-first; the CLI is merely the first host. The public API is frozen/semantically locked at OSS release.

---

## 7. Kill Criteria (from CFO)

The project is stopped/frozen if ≥2 are met:

1. **Crash-resume cannot be proven to work** after Phase 1a + 1 fix iteration.
2. **3 internal workflows unused** — no real usage >1 month after Phase 2.
3. **Maintenance >20% of saved time** the project should have produced (sustained negative ROI).
4. **External alternatives meet the needs** (an existing OSS harness is durable + embeddable enough and Cora integration can be built as a plugin).
5. **LLM cost per workflow unreasonable** compared to manual (no optimization path).
6. **Structural technical blocker** — the storage + state machine design is proven insufficient for real-world workflow complexity (not just a bug).

---

## 8. Success Metrics

| Metric | Target | How to measure |
|---|---|---|
| Crash-resume | Deterministic kill/restart test passes; ≥1 real incident of successful resume | Test suite + session logs |
| Internal workflows E2E | **>2 workflows** (of W1–W3) complete end-to-end without human intervention beyond approvals | Tree audit per session |
| Maintenance overhead | **<20%** of the time saved by the agent | Rough monthly time-tracking |
| Approval compliance | 0 Destructive executions without a recorded approval | Tree audit scan |
| Embeddability | Core crate compiles without CLI/UI feature; serializable API | Phase 3 CI check |

---

## 9. References

- `spec.md` (pi harness spec, MIT) — basis for the entry/register/state design.
- `crates/cora-agent-core/src/lib.rs` — module layout & official terminology.
- Cora ecosystem: `cora` (code graph & review), `uteke` (semantic memory), `gh` CLI.
