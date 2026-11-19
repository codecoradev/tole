# bench: release perf budget gate (#921)

Makes "lightweight" a checked number. CI builds the release `tole`, measures
four metrics against a local mock provider, and fails when one is over its
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
process. No network, no secrets (the API key and serve token are dummy
strings). Every run uses a fresh `HOME`, workspace and sessions dir.

| Metric | How |
|---|---|
| `cold_start_ms` | wall time of `tole --version`, median of 15 spawns, page cache warm |
| `idle_rss_mb` | `VmRSS` of `tole serve` once `/health` answers and 1 s has passed, median of 3 |
| `active_rss_mb` | peak RSS (`wait4` `ru_maxrss`) of one `tole run` doing 30 provider round-trips, each a `read_file` call (a turn is capped at 32 steps) |
| `per_turn_ms` | `(wall(30 round-trips) - wall(2 round-trips)) / 28`, medians of 11 runs; the slope cancels process start-up |

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

Needs Python 3.11+ (stdlib only) and Linux (`/proc`, `wait4`). Exit code 1
means a metric is over budget; `--report-only` writes the report and never
fails. `perf-report.json` carries the metrics, the raw samples, the CPU/RAM of
the machine and the per-metric verdicts. CI uploads it as `perf-report-<arch>`.

## Ratchet

Ceilings only move down.

1. Run 1 recorded a `baseline` per metric. The `ceiling` is the baseline times
   a headroom factor (x1.3 for RSS, wider for timing) so noise does not turn
   the build red. The factor is stated in the metric's comment.
2. After a change that makes tole leaner, lower the ceiling in the same PR to
   the new baseline times the same headroom. The gain is then locked in.
3. The `Perf budget ratchet` job compares `bench/budget.toml` with the base
   branch and fails the PR when any ceiling went up, or a budget was removed.
4. Raising a ceiling is the exception: the PR explains why in its body, and a
   maintainer adds the `perf-budget-raise` label after reading the diff of
   `budget.toml`. The change is visible in review because it is a diff of one
   file.

Never hand-pick a ceiling from a target. A ceiling is a measurement plus
headroom.

## Adding a platform

Run the harness on that platform (a job for it measures and uploads a report
even before it has a budget), copy the medians from the uploaded `perf-report-<arch>` into a
new `[<platform>.<metric>]` set of tables as `baseline`, and set each
`ceiling` with the same headroom. Until then the job prints a warning and
passes: a platform with no budget is measured, not gated.

## Out of scope here: deliverable B (install and first-run timing)

Issue #921 part B is not built here. A follow-up needs:

- a clean environment per run (a fresh container or VM, empty `HOME`, no
  cargo cache) and the install channel to time: `cargo install tole-cli` (the
  canonical one, `tole upgrade` tracks it) and the release archive plus
  checksum from `release.yml`, each as its own number;
- a timer from "install command starts" to "first successful session turn"
  through the default first-run flow, with a mock or a scoped test key for the
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
