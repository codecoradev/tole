# Contributing to tole

## Workflow

- **Branches**: `develop` is the integration branch; `main` is the release branch (tags only). All work happens via branch → PR → squash-merge into `develop`.
- **Commits**: Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`).
- **Pre-commit**: `cargo fmt` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test --workspace`. CI enforces the same.
- **Code review**: every PR is reviewed (cora scan + maintainer) before merge. CI must be green.

## Cora Workflow

Cora (`cora` CLI) is the standard review gate for this repo, wired in at three levels:

1. **Pre-commit hook** — `cora review --staged` runs on every commit (installed via git template; `cora hook uninstall` per-repo to remove).
   - Exit `0` → clean, commit proceeds.
   - Exit `2` → blocking issues (major/critical). Fix them; `--no-verify` skips fmt/clippy too, so prefer fixing. Known FP categories are encoded in `.cora.yaml` rules.
   - Exit `1` → cora itself errored (no key, config). Non-blocking; commit proceeds.
2. **Manual scan** — run `cora scan` locally after significant changes (new module, security-adjacent code). Scan is full-codebase; use `cora review` (diff-based) for large changes.
3. **CI check** — `Cora Review` job (`.github/workflows/cora-review.yml`) reviews every PR via `codecoradev/cora-review-action@v1` and reports SARIF security findings. A red Cora Review check means findings must be triaged before merge.

Review behavior is configured in `.cora.yaml` (focus areas, Tole-specific rules that encode intentional patterns — env-resolved credentials, argv-validated subprocess tools, wire-only redaction — plus ignore rules for known FP categories).

Exit-code reference:

| Code | Meaning | Action |
|------|---------|--------|
| 0 | No issues | Commit proceeds |
| 2 | Blocking issues (major/critical) | Fix before commit |
| 1 | Cora error (no key, config error) | Non-blocking, commit proceeds |

## Language Standard

All repository artifacts are written in **English**: commit messages, PR titles/bodies, issues, code comments, and documentation files. Keep chat discussions in whatever language you prefer — files must be English.

## Design Rules

- The core crate (`tole-core`) must stay platform-agnostic: no stdin/stdout, no CLI assumptions. Host interaction goes through traits (`Storage`, `Provider`, `Tool`, `Approver`).
- Session storage is append-only (JSONL, one file per session). Never mutate or delete entries; corrections are new entries.
- Every non-ReadOnly tool must go through an `Approver`. There is no bypass path.
- Adding code is a last resort — first check whether configuration or an existing mechanism solves the problem.

## Issue Tracking

Epics live in `docs/epics.md`; each epic has a tracking issue on GitHub. Keep both in sync: acceptance criteria checked in the issue when done.

## License

MIT. By contributing, you agree your contributions are licensed under the MIT License.
