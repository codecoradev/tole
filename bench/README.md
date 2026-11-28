# bench: release perf budget gate (#921)

Makes "lightweight" a checked number. CI builds the release `tole`, measures
five metrics against a local mock provider, and fails when one is over its
ceiling in [`budget.toml`](budget.toml).

| File | Role |
|---|---|
| `budget.toml` | ceilings per platform and metric, with the rationale for each |
| `budget.py` | budget parser, comparator, ratchet check (pure, unit-tested) |
| `run.py` | harness (`measure`), ratchet CLI (`ratchet`), markdown table (`render`) |
| `release-report-template.md` | the CPU/RAM table a release report carries |
| `../.github/workflows/perf-budget.yml` | the gate: x86_64 and aarch64 jobs, plus the ratchet job |

## What is measured

The real `target/release/tole`, no test double of the binary. The only fake is
the provider: an OpenAI-compatible endpoint on `127.0.0.1` inside the harness
process. No network, no secrets (the provider key and the serve token are
random throwaway values generated per run). Every run uses a fresh `HOME`, workspace and sessions dir.

| Metric | How |
|---|---|
| `cold_start_ms` | wall time of `tole --version`, median of 15 spawns, page cache warm |
| `idle_rss_mb` | `VmRSS` of `tole serve` once `/health` answers and 1 s has passed, median of 3 |
| `active_rss_mb` | peak RSS of one `tole run` doing 30 provider round-trips, each a `read_file` call (a turn is capped at 32 steps); the child's own `/proc/<pid>/status` `VmHWM`, sampled about every millisecond |
| `per_turn_ms` | `(wall(30 round-trips) - wall(2 round-trips)) / 28`, medians of 11 runs; the slope cancels process start-up |
| `binary_size_mib` | size of the `--bin` file in MiB (1 MiB = 1,048,576 bytes; #991). The CI build has `strip = true`, so it is the stripped release binary; an unstripped local build is about 1.4 times larger and must not be compared with the ceiling. Deterministic for one toolchain, so the x1.11 headroom is for the floating `stable` toolchain and 5 to 12 weeks of first-party growth, not noise; it is re-baselined on purpose at each release (see `budget.toml`) |

Peak RSS is read from the child's `VmHWM`, not from `wait4`'s `ru_maxrss`:
`ru_maxrss` carries the parent's resident size across fork+exec, so it reported
the harness's own RSS (a 300 MB parent made `tole --version` read 323 MB). The
first draft of this gate used it and recorded ~28 MB for a session whose real
peak is ~21 MB; that is corrected here. `VmHWM` is the kernel's monotone
high-water mark, so a sample taken after the peak is exact.

Each session run must also reach the mock exactly `rounds + 1` times (one per
tool call plus the final answer); the harness fails otherwise, so a loop that
stops early or a changed tool-result shape cannot pass as a measurement. A
measured value that is NaN, infinite or negative is rejected by the comparator
(a slope that dips below zero from noise is floored at 0 by the harness).

`per_turn_ms` is the harness's own cost per provider step (request build,
response parse, one tool call, one durable JSONL commit). The mock answers at
once, so provider latency is excluded; the mock's small constant cost is
included.

## Run it locally

```bash
cargo build --release -p tole-cli
python3 bench/run.py measure --bin target/release/tole --out perf-report.json
python3 -m unittest discover -s bench -p 'test_*.py'
```

The mock harness does not exercise TLS, real provider auth, DNS or a real
network: it measures the harness's own cost and memory, not a provider's.

Needs Python 3.11+ (stdlib only) and Linux (`/proc`, `wait4`). Exit code 1
means a metric is over budget; `--report-only` writes the report and never
fails. `perf-report.json` carries the metrics, the raw samples, the CPU/RAM of
the machine and the per-metric verdicts. CI uploads it as `perf-report-<arch>`.

## Ratchet

Ceilings only move down.

1. The `baseline` of each metric is what the first measurement recorded; the
   `ceiling` is the baseline times a headroom factor (x1.3 for RSS, x4 for
   timing) so noise does not turn the build red. The factor is stated in the
   metric's comment.
2. After a change that makes tole leaner, lower the ceiling in the same PR to
   the new baseline times the same headroom. The gain is then locked in.
3. The `Perf budget ratchet` job checks the PR's `bench/budget.toml` with the
   BASE branch's `bench/run.py` and `bench/budget.py` (checked out into
   `base/`), never the PR's own copy, so a PR cannot weaken the comparator to
   pass its own change. It fails on a raised or removed ceiling.
   The first PR that adds `bench/` has no base copy; the job is a no-op there
   and says so, and enforces from the next PR on.
4. A PR that edits `bench/*.py`, `bench/budget.toml` or
   `.github/workflows/perf-budget.yml`, or raises a ceiling, needs the
   `perf-budget-raise` label, applied by a maintainer after reading the diff.
   The label is read live (`gh pr view --json labels`, `pull-requests: read`),
   not from the event payload. The alternative, subscribing to
   `labeled`/`unlabeled`, would start the build-and-measure jobs on every label
   change, and "re-run jobs" replays the old payload anyway, so a live read is
   the only way a re-run after labelling sees the label. Consequence: adding
   the label does not start a run; re-run the ratchet job.
5. Who may apply the label is repository policy. A `CODEOWNERS` entry for
   `bench/` and `.github/workflows/perf-budget.yml` would make review of those
   paths mandatory; that is the owner's call and is not added here.

Never hand-pick a ceiling from a target. A ceiling is a measurement plus
headroom. Timing metrics on a shared runner can fail once from noise: re-run
the job. If it fails again, treat it as a regression; do not raise a ceiling
to hide one.

The x86_64 baseline in `budget.toml` was measured in a local container, not on
the hosted runner (CI x86_64 numbers were similar or lower). Re-baselining from
the first `develop` CI runs is a follow-up.

## Required checks and merge queue

`ci.yml` runs on `merge_group`; `perf-budget.yml` does not, and the ratchet job
is `pull_request`-only. So `Perf budget (x86_64)`, `Perf budget (aarch64)` and
`Perf budget ratchet` must stay NON-required checks, or a merge queue would wait
forever on checks that never report. Making them required means adding
`merge_group` to the workflow (and deciding what the ratchet does there) first.

## Adding a platform

Run the harness on that platform with `--report-only` first (add its job to
the workflow with that flag), copy the medians from the uploaded
`perf-report-<arch>` into new `[<platform>.<metric>]` tables as `baseline`, set
each `ceiling` with the same headroom, and drop `--report-only`. Locally, a
platform with no budget measures and warns. On a CI runner (`CI=true`) it FAILS
the job: a gate that skips is no gate.

## Out of scope here: deliverable B (install and first-run timing)

Issue #921 part B is not built here. A follow-up needs:

- a clean environment per run (a fresh container or VM, empty `HOME`, no
  cargo cache) and the install channel to time: `cargo install tole-cli` (the
  canonical one, `tole upgrade` tracks it) and the release archive plus
  checksum from `release.yml`, each as its own number;
- a timer from "install command starts" to "first successful session turn"
  through the default first-run flow, with a mock provider (or a scoped throwaway account) for the
  provider step, recorded to the same report schema;
- a decision on the runner: install time depends on network and compile time,
  so it is a scheduled or release-time job, not a PR gate, and its number is
  tracked rather than budgeted at first.

## Attaching the report to a release

`run.py render perf-report.json` prints the table from
[`release-report-template.md`](release-report-template.md). The release
workflow does not attach it yet (tracked in the PR description): the artifacts
`perf-report-x86_64` and `perf-report-aarch64` of the commit being tagged hold
the numbers.
