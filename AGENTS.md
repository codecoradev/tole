# AGENTS.md

Guidance for AI coding agents working in this repository. Humans: see
[CONTRIBUTING.md](CONTRIBUTING.md) — everything there applies to agents too.
Follows the ecosystem AGENTS.md standard (Gatehouse pattern): repo layout,
branch & merge discipline, pre-commit checks, tooling, cross-session
continuity, infra facts.

## 1. Repo layout

- `crates/tole-core` — platform-agnostic library (state machine, JSONL
  storage, `Tool`/`Approver`/`Storage`/`Provider` traits, tools, provider).
  Published to crates.io; embedders build `--no-default-features`.
- `crates/tole-cli` — headless CLI host (the `tole` binary). Published to
  crates.io; during work always run the freshly built
  `./target/release/tole`, never a PATH-resolved one.
- `docs/` — architecture, PRD, epics (roadmap + tracking issues), threat
  model, E8 retro.
- `evals/` — agentic regression harness (Tier 1 CI contracts, Tier 2 live
  missions, Tier 3 baselines).
- `.cora.yaml` — cora review configuration; known false-positive categories
  are encoded there as rules — extend the rule instead of sprinkling inline
  ignores.

## 2. Branch & merge discipline

- `develop` = integration, `main` = release (tags only). Work happens as
  branch → PR → squash-merge into `develop`. No direct pushes, no force
  pushes. Branch names (CI-enforced): `feat/*`, `fix/*`, `docs/*`, `chore/*`,
  `perf/*`, `security/*`, `refactor/*`, `test/*`, `build/*`, `ci/` — note
  `feat/`, not `feature/`.
- **Language:** every repository artifact (commits, PRs, issues, code
  comments, docs) is written in English.
- **Commits:** Conventional Commits; the PR title becomes the squash message.
- **PR body:** sections in order — **What / Why / Changes / Testing**
  (enforced by the PR Description check).
- **Issues:** larger changes are issue-first; use the issue templates; epics
  live in `docs/epics.md` with tracking issues.
- **Merge gate — never merge on assumption:** CI green AND a *real* Cora
  verdict in the PR comments ("✅ No issues found" or concrete findings —
  "Review could not complete" is a blocker), code-scanning alerts
  investigated (real → fix with regression test; FP → dismiss with
  evidence), CI auto-fix max 3 attempts then escalate. Large change-sets
  (~40+ files) time out Cora Review (10-min limit) — split into ~15–20-file
  PRs.

## 3. Mandatory pre-commit checks

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cora review --staged          # exit 0 clean · exit 2 blocking → fix · exit 1 cora error (non-blocking)
```

CI enforces the same set. There is no pre-commit hook installed by default —
run these yourself.

## 4. Tooling — cora & uteke first

These sibling tools are the ecosystem's force multipliers. Use them instead
of reinventing or brute-forcing:

**cora (code intelligence) — recall before edit, verify after:**

- `cora index --stats` — confirm the repo is indexed before relying on
  search (0 symbols → run `cora index`).
- `cora brain "<topic>" --json` — semantic recall of existing patterns
  **before writing code; prefer this over blind grep**.
- `cora callers <sym>` / `cora impact <sym>` / `cora affected <files>` —
  mandatory before any refactor: who breaks, and which tests to rerun.
- `cora query "main -> *"` / `cora trace <sym>` / `cora arch` — structure
  and path questions without reading whole files.
- `cora dead-code` — after removals; `cora debt` — trend checks.
- `cora review --staged` before every commit (mandatory, see §3);
  `cora scan` after significant or security-adjacent changes.

**uteke (semantic memory) — continuity across sessions:**

- `recall` before starting unfamiliar work — the decision may already be
  recorded; `remember` durable outcomes and decisions of real work sessions
  (namespace `repo-tole`, the ecosystem-wide `repo-<name>` convention).
- Rooms `codecoradev-conventions` and `codecora-workflow-standard` hold the
  ecosystem standards (branch strategy, PR/issue rules, merge gate,
  governance) — they are the source of truth when this file and reality
  disagree.
- Long-form knowledge (runbooks, retros) goes through `uteke_document`
  (markdown → durable, chunked, searchable).

## 5. Cross-session continuity

Repo files are canonical over mirrors. Durable outcomes and decisions of
real work sessions belong in uteke (`repo-tole`); the ecosystem standards
records live in uteke rooms (above). Start a session on an unfamiliar area
with a recall; end a session that made decisions with a remember.

## 6. Design rules — do not bend

- `tole-core` stays platform-agnostic: no stdin/stdout, no CLI assumptions.
  Host interaction goes through traits (`Storage`, `Provider`, `Tool`,
  `Approver`).
- Session storage is append-only JSONL. Never mutate or delete entries;
  corrections are new entries.
- Every non-ReadOnly tool goes through an `Approver`. No bypass path.
  `Destructive` is structurally unregistrable without an interactive
  approver and never allowlistable — do not weaken the three enforcement
  layers (registry / `AllowlistApprover` / host prompt).
- MCP tools are always `Risk::Write`. Server-supplied metadata is never
  trusted for risk classification.
- Adding code is a last resort — first check whether configuration or an
  existing mechanism solves the problem.

## 7. Docs sync

The README status line and `docs/epics.md` must be updated **in the same PR**
that makes them stale. This regression has happened twice (#33 and again
2026-09) — do not ship a third one.

## 8. Infra facts (verified only, with dates)

- **2026-09-18:** cora 0.13.0 no longer reads the legacy `~/.cora/auth.toml`
  / `~/.cora/config.yaml`. Until re-provisioned with `cora auth login`,
  exports are required: `CORA_API_KEY`, `CORA_BASE_URL`, `CORA_MODEL`
  (verified working — review returns a real verdict with these three set).
- **2026-09-12:** only `tole-core` was on crates.io; as of the 0.3.0
  releases **both** `tole-core` and `tole-cli` (0.3.0) are published. The
  registry copy still lags the checkout — live missions run the freshly
  built `./target/release/tole` (see CONTRIBUTING → Live-mission binary
  hygiene).
