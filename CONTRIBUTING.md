# Contributing to tole

Thank you for contributing! This document is the single entry point for the
workflow; the ecosystem-wide standards it follows live in the uteke rooms
`codecora-workflow-standard` and `codecoradev-conventions` (source of truth
when this file and reality disagree).

## Workflow

- **Branches**: `develop` is the integration branch; `main` is the release
  branch (tags only). All work happens as branch → PR → squash-merge into
  `develop`. No direct pushes, no force pushes.
- **Branch names** (CI-enforced): `feat/*`, `fix/*`, `docs/*`, `chore/*`,
  `perf/*`, `security/*`, `refactor/*`, `test/*`, `build/*`, `ci/` — note
  `feat/`, not `feature/`.
- **Commits**: Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`,
  `refactor:`, `test:`). The PR title becomes the squash commit message.
- **Pre-commit**: `cargo fmt` → `cargo clippy --workspace --all-targets -- -D
  warnings` → `cargo test --workspace`. CI enforces the same set.
- **Code review**: every PR gets a Cora review (below) plus maintainer review
  before merge. CI must be green.

## Issues

- Use the issue templates ([bug report](.github/ISSUE_TEMPLATE/bug_report.yml),
  [feature request](.github/ISSUE_TEMPLATE/feature_request.yml)).
- **Issue-first**: larger changes (new features, architecture, new
  subsystems) start as an issue for discussion before any PR. One PR = one
  logical change; split accordingly.
- Epics live in [docs/epics.md](docs/epics.md), each with a tracking issue on
  GitHub. Check the acceptance criteria in the issue when done, in the same
  PR.

## Pull requests

- One logical change per PR; large change-sets (~40+ files) deterministically
  time out the Cora Review job (10-minute hard limit) — split into ~15–20-file
  PRs.
- Body sections, in order: **What / Why / Changes / Testing**. A PR without
  them fails the PR Description check.
- Base every PR on `develop`. PRs into `main` are only accepted from
  `develop` or `chore/release-*` (release flow).

## Merge gate — before merging any PR

Never merge on schedule or assumption:

1. `gh pr checks <N>` — all checks pass.
2. The Cora review verdict must be **real**: the review result lands as an
   issue comment — it must say "✅ No issues found" or list concrete
   findings. "Review could not complete" / LLM error = blocker → re-run
   until a real verdict (or consciously accept with documented justification).
3. Code-scanning alerts on the PR head — investigate each one.
4. Validate every finding against the code: real → fix with a regression
   test; false positive → dismiss **with evidence**.
5. CI auto-fix loop: max 3 attempts, then stop and escalate.
6. Squash-merge, delete the branch, sync `develop`.

## Cora Workflow

Cora (`cora` CLI) is the standard review gate for this repo, wired in at
three levels:

1. **Pre-commit hook** — `cora review --staged` runs on every commit (installed
   via git template; `cora hook uninstall` per-repo to remove).
   - Exit `0` → clean, commit proceeds.
   - Exit `2` → blocking issues (major/critical). Fix them; `--no-verify`
     skips fmt/clippy too, so prefer fixing. Known FP categories are encoded
     in `.cora.yaml` rules.
   - Exit `1` → cora itself errored (no key, config). Non-blocking; commit
     proceeds.
2. **Manual scan** — run `cora scan` locally after significant changes (new
   module, security-adjacent code). Scan is full-codebase; use `cora review`
   (diff-based) for large changes.
3. **CI check** — `Cora Review` job (`.github/workflows/cora-review.yml`)
   reviews every PR via `codecoradev/cora-review-action@v1` and reports SARIF
   security findings. A red Cora Review check means findings must be triaged
   before merge.

Review behavior is configured in `.cora.yaml` (focus areas, tole-specific
rules that encode intentional patterns — env-resolved credentials,
argv-validated subprocess tools, wire-only redaction — plus ignore rules for
known FP categories).

Exit-code reference:

| Code | Meaning | Action |
|------|---------|--------|
| 0 | No issues | Commit proceeds |
| 2 | Blocking issues (major/critical) | Fix before commit |
| 1 | Cora error (no key, config error) | Non-blocking, commit proceeds |

**Known issue (2026-09-18):** cora 0.13.0 no longer reads the legacy
`~/.cora/auth.toml` / `~/.cora/config.yaml`. Until re-provisioned with
`cora auth login`, exports are required: `CORA_API_KEY`, `CORA_BASE_URL`,
`CORA_MODEL`.

## Language standard

All repository artifacts are written in **English**: commit messages, PR
titles/bodies, issues, code comments, and documentation files. Keep chat
discussions in whatever language you prefer — files must be English.

## CLA

Contributions require a signed Contributor License Agreement —
[CLA_INDIVIDUAL.md](CLA_INDIVIDUAL.md) or
[CLA_CORPORATE.md](CLA_CORPORATE.md). The `cla-check` workflow enforces this
per PR (bots are skipped). The CLA is a license agreement, not a copyright
assignment: you retain ownership of your contributions.

## Live-mission binary hygiene

Running live agent missions against a stale binary produces misleading
results (the model truthfully reports tools that no longer exist, or old
behavior that was already fixed). Before any live validation or mission run:

- Always build and invoke the **repo checkout binary**, not a PATH-resolved
  one: `cargo build --release -p tole-cli && ./target/release/tole ...`.
  Both `tole-core` and `tole-cli` are published to crates.io, but the
  registry copy lags your checkout.
- If a `~/.cargo/bin/tole` shim is installed, re-sync it after every version
  bump: `cp target/release/tole ~/.cargo/bin/tole` — verify with
  `tole --version` matching the workspace version in `Cargo.toml`.

## Design rules

- The core crate (`tole-core`) must stay platform-agnostic: no stdin/stdout,
  no CLI assumptions. Host interaction goes through traits (`Storage`,
  `Provider`, `Tool`, `Approver`).
- Session storage is append-only (JSONL, one file per session). Never mutate
  or delete entries; corrections are new entries.
- Every non-ReadOnly tool must go through an `Approver`. There is no bypass
  path. `Destructive` is structurally unregistrable without an interactive
  approver and never allowlistable — do not weaken the three enforcement
  layers (registry / `AllowlistApprover` / host prompt).
- MCP tools are always `Risk::Write`. Server-supplied metadata is never
  trusted for risk classification.
- Adding code is a last resort — first check whether configuration or an
  existing mechanism solves the problem.

## License

MIT. By contributing, you agree your contributions are licensed under the
MIT license, and you sign the CLA above (enforced per PR by `cla-check`).
