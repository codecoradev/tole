# AGENTS.md

Guidance for AI coding agents working in this repository. Humans: see
[CONTRIBUTING.md](CONTRIBUTING.md) — everything there applies to agents too.

## What this repo is

`tole` is a durable Rust agent harness (`crates/tole-core` platform-agnostic
library + `crates/tole-cli` host). Sibling ecosystem tools: `cora` (code
intelligence), `uteke` (semantic memory). Ecosystem conventions are recorded
in uteke (room `codecoradev-conventions`, namespace `hermes`) — that record is
the source of truth when this file and reality disagree.

## Ground rules

- **Language:** every repository artifact (commits, PRs, issues, code
  comments, docs) is written in English.
- **Branches:** `develop` = integration, `main` = release (tags only).
  Work happens as branch → PR → squash-merge into `develop`. No direct
  pushes, no force pushes. Branch names (CI-enforced): `feat/*`, `fix/*`,
  `docs/*`, `chore/*`, `perf/*`, `security/*`, `refactor/*`, `test/*`,
  `build/*`, `ci/` — note `feat/`, not `feature/`.
- **Commits:** Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`,
  `refactor:`, `test:`).
- **PRs:** one logical change per PR (larger work = issue first). Body
  sections: **What / Why / Changes / Testing**.
- **Merge gate — never merge on assumption:** CI green, bot AND human
  comments triaged, code-scanning alerts investigated (real → fix with a
  regression test; false positive → dismiss with evidence), CI auto-fix
  max 3 attempts then escalate.

## Verification before every commit

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI enforces the same set. There is no pre-commit hook installed by default —
run these yourself.

## Mandatory review gate: cora

`cora` is the standard review tool of this ecosystem. Before every commit:

```bash
cora review --staged          # exit 0 clean · exit 2 blocking findings → fix · exit 1 cora error (non-blocking)
cora scan                     # after significant changes (new module, security-adjacent code)
```

- Known false-positive categories are encoded as rules in `.cora.yaml` —
  extend those rules instead of sprinkling inline ignores.
- The CI job `Cora Review` reviews every PR; a red check must be triaged
  before merge.
- **Known issue (2026-09-18):** cora 0.13.0 no longer reads the legacy
  `~/.cora/auth.toml` / `~/.cora/config.yaml`. Until re-provisioned with
  `cora auth login`, exports are required:
  `CORA_API_KEY`, `CORA_BASE_URL`, `CORA_MODEL`.

## Design rules — do not bend

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

## Live missions / dogfooding

- Before any live validation run, rebuild the checkout binary and invoke it
  by path (`cargo build --release -p tole-cli && ./target/release/tole ...`).
  Never a PATH-resolved `tole` — only `tole-core` is on crates.io.
- Durable outcomes and decisions of real work sessions belong in uteke,
  namespace `repo-tole` (the ecosystem-wide `repo-<name>` convention).

## Docs sync

The README status line and `docs/epics.md` must be updated **in the same PR**
that makes them stale. This regression has happened twice (#33 and again
2026-09) — do not ship a third one.

## Issue tracking

Epics live in `docs/epics.md`, each with a tracking issue on GitHub. Check
the acceptance criteria in the issue when done, in the same PR.
