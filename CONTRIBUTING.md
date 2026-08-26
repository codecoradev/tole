# Contributing to cora-agent

## Workflow

- **Branches**: `develop` is the integration branch; `main` is the release branch (tags only). All work happens via branch → PR → squash-merge into `develop`.
- **Commits**: Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`).
- **Pre-commit**: `cargo fmt` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test --workspace`. CI enforces the same.
- **Code review**: every PR is reviewed (cora scan + maintainer) before merge. CI must be green.

## Language Standard

All repository artifacts are written in **English**: commit messages, PR titles/bodies, issues, code comments, and documentation files. Keep chat discussions in whatever language you prefer — files must be English.

## Design Rules

- The core crate (`cora-agent-core`) must stay platform-agnostic: no stdin/stdout, no CLI assumptions. Host interaction goes through traits (`Storage`, `Provider`, `Tool`, `Approver`).
- Session storage is append-only (JSONL, one file per session). Never mutate or delete entries; corrections are new entries.
- Every non-ReadOnly tool must go through an `Approver`. There is no bypass path.
- Adding code is a last resort — first check whether configuration or an existing mechanism solves the problem.

## Issue Tracking

Epics live in `docs/epics.md`; each epic has a tracking issue on GitHub. Keep both in sync: acceptance criteria checked in the issue when done.

## License

MIT. By contributing, you agree your contributions are licensed under the MIT License.
