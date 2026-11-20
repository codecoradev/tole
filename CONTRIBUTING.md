# Contributing to tole

Thank you for contributing! This document is the single entry point for the
workflow; the ecosystem-wide standards it follows live in the team knowledge rooms
(source of truth when this file and reality disagree).

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
  warnings` → `cargo test --workspace`. CI enforces the same set. A change to
  `crates/tole-desktop` or `crates/tole-desktop-model` also runs
  `scripts/test-desktop-check.sh` and `scripts/desktop-check.sh` (#1028; see
  [docs/design/desktop/README.md](docs/design/desktop/README.md); run
  `cargo fetch` once first). A broken dependency-graph or size rule makes the
  script exit 1; the literals check is advisory (it warns, never fails). The separate
  `Desktop` workflow (`.github/workflows/desktop.yml`; not part of `ci.yml`, not
  a required check unless the owner adds it to the branch protection) runs them
  and the window build.
- **Test directories** (#781): a test that needs a scratch directory uses
  `TestDir` (`crates/*/tests/support/test_dir.rs` or `tests/common/test_dir.rs`,
  and the `crate::test_dir` module in unit tests), never a hand-built
  `std::env::temp_dir().join(..)`. It removes itself when dropped, also on a
  panic; `TOLE_KEEP_TEST_DIRS=1` keeps the directories for debugging. A child
  process that may still use the directory is killed and reaped first (see
  `ChildGuard`). CI runs the tests with a private `TMPDIR` and fails if a
  `tole-*` or `cora-*` entry is left behind.
- **Waiting in tests** (#812): a test that waits for something to happen
  (a child to write a file, a server to be busy, a process to exit) polls for
  the condition with `wait_until(WAIT, "what", || cond)` or `wait_for`
  (`test_dir.rs`, next to `TestDir`), never `thread::sleep(n)` followed by an
  assertion and never a hand-written loop with its own short deadline. The
  bound is generous (60 s) and only a failing run waits for it. A mock that
  has to hold an answer while the test acts waits on a condition too (a gate
  the test opens), not a sleep of a guessed length. A fixed sleep stays only
  for time that must pass by itself (a whole-second clock tick, a delay that
  is the thing under test).
- **Scripts a test runs** (#840): write a fake binary or wrapper with
  `write_executable(path, contents)` (or `copy_executable` for a copied
  binary), both in `test_dir.rs`, never `std::fs::write` plus
  `set_permissions(.., 0o755)`. A concurrent `fork()` of another test thread
  can still hold the write fd, and the `exec` then fails with `ETXTBSY`
  ("Text file busy"); the helpers return only once no writer is left.
- **Hermetic e2e tests** (#993): a test that spawns the `tole` binary, or code
  that spawns external tools (`cora`, `uteke`, `git`, a shell), builds its
  `Command` with `common::hermetic_command(program, &home, &extra_env)`
  (`crates/tole-cli/tests/common/hermetic.rs`), never a bare `Command::new`.
  It sets `HOME` and `CODECORA_HOME` to the test's own home, drops every `PATH`
  directory holding `cora` or `uteke` (the rest stays, so `sh` and `git` work)
  and clears the XDG overrides; `extra_env` is applied last, so a test that
  wants a shim or a decoy overrides there. Without it a test passes in CI and
  fails, or writes real state, on a machine that has those tools and a
  `~/.codecora`.

  ```rust
  let home = dir.join("home");
  let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &home, &[
      ("TOLE_BASE_URL", base.as_str()),
  ]);
  cmd.args(["run", "hello"]).current_dir(&dir);
  ```

  To harden a `Command` built elsewhere, call `common::apply_hermetic_env(&mut cmd,
  &home, &extra)`. The exception is a test that runs agent code in the test
  process (`cron_agent.rs`, `child_handoff_e2e.rs`): there is no child to
  configure, and it must not mutate the process environment to fake one;
  see #993 for the options. Unconverted files are tracked in #993.
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

- One logical change per PR; large change-sets (~40+ files) can time out the
  Cora Review job (30-minute limit) — split into ~15–20-file PRs.
- **PR size.** Keep a PR under about 600 changed lines of code (additions
  plus deletions, not counting `CHANGELOG.md`, `docs/`, `Cargo.lock`); split
  anything larger, e.g. into a refactor PR and a behavior PR, or one PR per
  subsystem. Cora Review reads the whole diff, and it is a merge gate. On
  recent PRs it took 12 to 14 minutes at +492/-22 and at +1366/-26 (Build and
  Test take 4 to 6), and the longest reviews ended with "Review could not
  complete" (empty result), which is not a pass and costs a rerun. Since cora
  0.21 (#705) `review.exclude` in `.cora.yaml` removes `CHANGELOG.md`, `docs/`
  and the lockfiles from the reviewed diff, and a diff above
  `review.max_chunk_chars` is reviewed in chunks of whole files, so those
  files no longer count toward the size; a review that did not cover the whole
  diff now fails the job instead of passing. Keep the code part of a PR small
  all the same.
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
6. CI must have run on the merge result: the branch is up to date with
   `develop` and green on that head (see "Series of dependent PRs" below).
7. Squash-merge, delete the branch, sync `develop`.

### Series of dependent PRs

Two PRs can each be green alone and still break `develop` together (a
semantic conflict, for example two PRs editing the same tests; #806). So:

- After each squash merge of a series (or of any PR that touches files other
  open PRs touch), update the next PR with the new `develop`
  (`update_pull_request_branch`, or `git merge origin/develop` into the
  branch; never a force-push) and wait until it is green again before
  merging it. A green run from before the update does not count.
- After every merge, look at the push-to-`develop` CI run (it runs on every
  merge since #762). A red `develop` stops merging until it is fixed or
  reverted; do not stack more PRs on it.
- Preferred, and an owner setting: a merge queue on `develop`, so CI runs on
  the merge result and this rule is enforced instead of remembered. `CI`,
  `Security` and the PR metadata workflows already run on `merge_group`
  events with the same check names, so the queue can be switched on without
  touching the workflows. `Cora Review`, `Message Standard`, `PR Description`
  and `Conventional Commits` judge the PR itself and are skipped (they report
  success) on a queue entry; they stay enforced on the PR head. Why a queue
  and not "require branches to be up to date": a queue costs one CI run
  (about 8 minutes) per PR, while strict mode makes every open PR re-run
  after every merge, and the 12 to 30 minute Cora Review job with it.
  Owner click-path: Settings, Rules, Rulesets, `develop protection`, tick
  "Require merge queue" (merge method Squash, build concurrency 5, minimum
  group size 1, status check timeout 60 minutes), keep the ten required
  checks as they are. A repository ruleset can only be changed by the owner;
  until it is, the rules above apply.

## Local review before a PR

The CI Cora Review job takes 12 to 30 minutes on a large diff and can end with
"could not complete", so findings that arrive late cost a full CI cycle each.
Get them earlier, on your own machine:

1. `scripts/review-local.sh` reviews `git diff origin/develop...HEAD` (use
   `--base REF` for another base). It splits the diff by crate or directory
   into chunks of at most 600 changed lines (`--max-lines N`), runs
   `cora review` on each chunk and prints only the findings
   (`--severity` sets the minimum, default `minor`; `--dry-run` prints the
   chunk plan). Exit `0` = no findings, `2` = findings, `1` = error or no LLM
   configured (not a pass).
2. Fix real findings; for a false positive, extend the rule in `.cora.yaml`
   instead of sprinkling inline ignores.
3. State the result in the PR **Testing** section. Agent-written PRs must do
   this; without an LLM key, review through `cora mcp` (`review_prepare` /
   `review_finalize`).
4. For a refactor, run `cora callers <symbol>`, `cora impact <symbol>` and
   `cora affected <files>` first when you move, rename or remove a public item,
   change a ledger, session, lock or config format, or touch the sandbox,
   approval, allowlist or secret-redaction paths. These commands are noisy on
   Rust (#669): treat the output as a pointer and verify callers and tests
   with grep before acting.

The local run does not replace the CI Cora Review job, which remains the merge
gate (see Merge gate above).

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

   **Credentials.** The job prefers Anthropic workload identity federation:
   when `ANTHROPIC_FEDERATION_RULE_ID`, `ANTHROPIC_ORGANIZATION_ID`,
   `ANTHROPIC_SERVICE_ACCOUNT_ID` and `ANTHROPIC_WORKSPACE_ID` are available
   (organization or repository secrets or variables), it exchanges its GitHub
   OIDC token for a short-lived Anthropic access token and sends it only to
   `https://api.anthropic.com/v1` (model `ANTHROPIC_MODEL`, default
   `claude-opus-5-5`). Otherwise, or when the exchange fails, it uses
   `CORA_API_KEY`, `CORA_BASE_URL` and `CORA_MODEL`; an empty secret fails
   the job rather than passing silently. The federation rule's subject prefix
   must use GitHub's numeric ids, e.g. `repo:codecoradev@<id>/tole@<id>:` —
   `repo:codecoradev/tole:` never matches. Run the manual
   `anthropic-wif-smoke.yml` workflow to check the exchange.

Review behavior is configured in `.cora.yaml` (focus areas, tole-specific
rules that encode intentional patterns — env-resolved credentials,
argv-validated subprocess tools, known-value redaction at the egress sinks — plus ignore rules for
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

## Live-mission binary hygiene

Running live agent missions against a stale binary produces misleading
results (the model truthfully reports tools that no longer exist, or old
behavior that was already fixed). Before any live validation or mission run:

- Always build and invoke the **repo checkout binary**, not a PATH-resolved
  one: `cargo build --release -p tole-cli && ./target/release/tole ...`.
  Both `tole-core` and `tole-cli` are published to crates.io, but the
  registry copy lags your checkout.
- `tole serve --transport mcp` works on a plain build since #168 —
  `mcp-http` is a default `tole-cli` feature; no extra flags needed.
- If a `~/.cargo/bin/tole` shim is installed, re-sync it after every version
  bump: `cp target/release/tole ~/.cargo/bin/tole` — verify with
  `tole --version` matching the workspace version in `Cargo.toml`.

## Release validation

Before claiming a build is ready to release, run the provider doctor at the
full tier with the repo checkout binary:

```bash
cargo build --release -p tole-cli && ./target/release/tole doctor --tier full
```

It must exit 0 (chat, streaming and a real tool-call loop all green); the
result is recorded in the coverage ledger (`tole doctor --summary`).

Release checklist, also before tagging: the latest push-to-`develop` CI run
is green (and the release PR's own run), and no merged PR left `develop` red.

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

Contributions are licensed to the project under the MIT license (inbound = outbound). No additional agreement is required.
