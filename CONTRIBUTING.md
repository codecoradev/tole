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
  warnings` → `cargo test --workspace`. CI enforces the same set, plus
  `scripts/arch-check.sh` (crate kinds and the crate dependency matrix, #1250; file and crate size against `scripts/arch-size-allow.txt`, #1251; `unsafe` opt-ins against the list in `AGENTS.md`, #1252; the files of the scoped `index-breaking-change` entry in `.cora.yaml` stay binary-only, #1249) and
  its self-test `python3 scripts/test_arch_lint.py`, which run as steps of the
  `Check` job; run the self-test when you touch `scripts/arch-*`. A change to
  `crates/tole-desktop` or `crates/tole-desktop-model` also runs
  `scripts/test-desktop-check.sh` and `scripts/desktop-check.sh` (#1028; see
  [docs/design/desktop/README.md](docs/design/desktop/README.md); run
  `cargo fetch` once first). A broken dependency-graph or size rule makes the
  script exit 1; the literals check is advisory (it warns, never fails). The separate
  `Desktop` workflow (`.github/workflows/desktop.yml`; not part of `ci.yml`, not
  a required check unless the owner adds it to the branch protection) runs them
  and the window build.
  A desktop UI change is also reviewed on the rendered window:
  `scripts/desktop-render.sh <dir>` (offscreen, one PNG per screen and state;
  not a gate, see the same README).
- **`unsafe`** (#1252): `unsafe_code` is denied for the whole workspace, so a
  new `unsafe` block fails `cargo clippy` and `cargo build` outside a module
  that opted in. Prefer a safe std API or an existing wrapper. If libc is the
  only way, put one `#[allow(unsafe_code)]` with a `// SAFETY:` comment on the
  named module or function that wraps the call (a test helper: on the test
  function or module), keep a `// SAFETY:` comment on each `unsafe` block, and
  add the file to the opt-in list in `AGENTS.md` section 6 in the same change;
  `scripts/arch-check.sh unsafe` fails when the list and the code differ.
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
  `~/.codecora`. `crates/tole-cli/tests/hermetic_audit.rs` enforces it: a test file
  that names `CARGO_BIN_EXE_tole` and neither uses the helper nor is listed there
  (`OWN_ENVIRONMENT`, with the reason its own environment is hermetic) fails, on any
  machine, with or without those tools. `hermetic_helper.rs` tests the helper itself.

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
  see #993 for the options. No file that spawns the binary is left unconverted: the
  audit above lists the ones with an environment of their own.
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

### Release sync PRs (#1022)

A release sync (`chore/release-*` into `main`, one squash commit with the tree
of `develop`) is one big diff in which every file was already reviewed by Cora
in the PR that put it on `develop`, and it is more than the Cora Review job can
cover. When the job runs on such a PR it **skips the review by tree identity**:
the step `Is this a release sync with the tree of develop?` in
`.github/workflows/cora-review.yml` runs `scripts/cora-release-sync.sh` (read
from `develop`, tested by `scripts/test-cora-release-sync.sh`) and skips only
when ALL of these hold; any mismatch or doubt runs the normal review:

- the event is `pull_request`, the base is `main`, the head branch is
  `chore/release-*`, and the PR is not from a fork;
- the PR head is exactly one commit on top of the current tip of `main`;
- the tree of the PR head equals the tree of a commit in the first-parent
  history (newest 200) of `origin/develop`, fetched from this repository.

The proof is the two tree hashes, never the PR text. The check keeps the name
`Cora Review` and its summary says "skipped, tree identical to develop at
`<sha>`"; that is a skip, not a Cora verdict, and the merge evidence is the
tree check plus green CI. Check it by hand (same result as the job):

```bash
git fetch origin develop main <pr-branch>
git rev-list --count origin/main..origin/<pr-branch>          # 1
git rev-parse origin/<pr-branch>^                              # == git rev-parse origin/main
git rev-parse origin/<pr-branch>^{tree}                        # == the next line
git log --first-parent -n 200 --format='%H %T' origin/develop | grep "$(git rev-parse origin/<pr-branch>^{tree})"
```

A second commit (a version bump on top of the sync, say), a differing file, a
fork or another branch name runs the review as before. The workflow file of a
`pull_request` run comes from the merge ref, so a change to it only takes
effect for a release branch built after it landed on `develop`. Since #1021 the
trigger lists `develop` only, so a PR into `main` gets no `Cora Review` run at
all until `main` is listed again; the skip applies from then on.

Residual risks of the skip, for whoever merges the release PR:

- The decision is made when the job runs. If `main` moves afterwards the job is
  not re-run (the same holds for a PR retargeted to `main`). Merge only a sync
  that is up to date with `main` (the owner's "require branches to be up to
  date" or a merge queue), or push the release branch again.
- Any of the newest 200 first-parent `develop` trees qualifies, not only the
  tip, so a release cut some merges back is accepted (all of that content was
  reviewed on `develop`, but main then lags the tip). Compare the sync with
  `develop` yourself when the release is meant to be the tip.
- The deciding script runs from `develop` with the job's environment minus the
  Cora token (`env -u ACCESS_TOKEN`); it reads only git objects and the
  checkout runs without credentials.

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
   this, **before the PR is opened**. Without an LLM key (an agent session),
   use Cora as the agent's reviewer through `cora mcp`:
   `scripts/review-local.sh --agent DIR` writes the chunks and calls
   `cora.review_prepare` for each; a reviewer who did not write the code (a
   fresh read-only subagent, or a person) answers each `DIR/chunk-N.prompt.md`
   with a JSON array in `DIR/chunk-N.findings.json`; `scripts/cora-agent-review.py
   finalize DIR` calls `cora.review_finalize` with the same diff, validates the
   findings, applies the `.cora.yaml` rules and prints the verdict (exit `0` none,
   `2` findings, `1` incomplete: not a pass). Run both inside the checkout or
   worktree of the branch. It is advisory like `review-local.sh`: the CI Cora
   Review stays the merge gate, and the independent read-only reviews that the
   approval, egress, auth, sandbox and guard paths need (AGENTS.md section 2)
   still apply. The deterministic `index-unused-import` findings that `prepare` lists
   for a diff fragment are often false positives (the import is used elsewhere in the
   file): the reviewer checks the file before reporting one.
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
known FP categories). The deterministic `index-breaking-change` check ("Symbol
`X` is being removed but has N caller(s)") resolves Rust calls by bare name
(#669): integration tests (`crates/*/tests/**`) are kept out of the symbol index
(`ignore.files`), and the binary-only modules `crates/tole-cli/src/main.rs`,
`serve.rs` and `acp.rs` (not part of the library API, the compiler catches their
callers) have a rule scoped to those three files in `ignore.rules`, so a refactor that
moves items out of them is not reported as a removal. Do not add a file the
library re-exports: `scripts/arch-check.sh cora` (part of `all`, a CI `Check`
step; needs PyYAML, a missing one is exit 2) checks two things with ground
truth, not text matching. First, the parsed `.cora.yaml` (PyYAML, duplicate
keys, merge keys, custom tags and files over 1 MiB refused) is held to
whitelists: the top level holds only the known keys (`llm`, `focus`, `rules`,
`ignore`, `review`, `rules_engine`, `hook`, `output`), `ignore` and `review` hold only known keys; `ignore.files` and
`review.exclude` hold only lock-file, `target`, docs and `crates/*/tests`
globs (so a glob over source files is refused, which would also drop them from
the symbol index or the reviewed diff); a plain `ignore.rules` string (or the
`rule` of another entry) holds no glob or regex character and is no part of the
rule id or of the finding text ("Symbol `X` is being removed but has N
caller(s)") and none of their words (index, breaking, change, symbol, removed,
removing, caller, callers); `index-breaking-change` itself is silenced only by an entry of the
form `rule`/`files`/`reason` whose `files` are exact `crates/<name>/src/**/*.rs`
paths, and no other string, key or value anywhere in the document may name it
(`index-breaking`, `breaking-change`, any case, `-` or `_`). Second, after
`cargo check --workspace --lib --bins --offline`, rustc's own dep-info (the list
of every source file it loaded, compared as real paths, so `mod`, `#[path]`,
`include!`, macros and symlinks all count) must list each scoped file for a bin
target and for no lib target of any workspace crate; build scripts are skipped.
A cargo or dep-info failure is exit 2, never a pass (cargo stdout lines that do
not start with `{`, such as a proc macro's `println!`, are skipped; a broken
line that does is exit 2). The guard runs `python3 -I`, which ignores the user
site: install PyYAML in the system or venv python (`apt install python3-yaml`),
not with `pip install --user`. This is a tripwire against
the known ways to silence the rule, not a proof: cora was not run against these
spellings, so what it does with a key or glob it was not tested on (anchoring,
case, a key this script does not know) cannot be verified here. Residuals: it
checks the default features and the host platform the `Check` job builds (a
`#[cfg(feature = ..)] mod serve;` or a `#[cfg(windows)]` module in a lib is not
seen), a `build.rs` or proc macro that generates library code from a scoped
file is not seen, and an `include_str!`/`include_bytes!` of a scoped file in a
lib is reported as if it were a module (fail closed).

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

The release sync PR (`chore/release-*` into `main`, one commit, the tree of
`develop`) is not reviewed again by Cora when the job runs on it: it is skipped
by tree identity (see "Release sync PRs (#1022)" under the merge gate for the
rules and how to check it by hand). Build the release branch as one commit
directly on the tip of `main`, from the `develop` commit you release; do not
add a second commit, or the whole range is reviewed.

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
- The `tole serve` wire is additive-only (#1024): a new route or key is
  fine, a removed or renamed one, or a changed meaning, raises
  `SERVE_PROTOCOL_VERSION` and gets a BREAKING line in `CHANGELOG.md`. The rule
  and the tests that enforce it are in "Wire compatibility" in
  [docs/mobile-control.md](docs/mobile-control.md).
- Adding code is a last resort — first check whether configuration or an
  existing mechanism solves the problem.

## License

Contributions are licensed to the project under the MIT license (inbound = outbound). No additional agreement is required.
