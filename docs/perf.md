# tole — Performance Record (E9)

Threshold benchmarks, CI-enforced (`perf_threshold` test binary). These
are **regression gates**, not microbenchmarks: generous budgets that
fail when the harness itself regresses. Provider latency is excluded
(MockProvider — no network); what is measured is the harness's own
durability tax.

## Budgets (2026-09-28, v0.3.0-era baseline)

| Scenario | Budget | What it covers |
|---|---|---|
| Text-only turn (1 prompt → 1 final) | < 2,000 ms | one full state-machine cycle + JSONL commits |
| 10-call tool chain | < 2,000 ms | 10 × the full intent→effect→settle sandwich, each with its own durable commit |
| Session replay (open, ~50+ entries) | < 2,000 ms | crash-resume replay cost — decides whether long sessions stay practical |

Observed headroom on the dev laptop (M-series, release mode): all three
scenarios complete in **< 50 ms** — the budgets are ~40× above observed,
so a failure means a real regression (e.g. an accidental O(n²) on the
transcript path), never machine noise.

## Method

- `crates/tole-core/tests/perf_threshold.rs` — deterministic, no timing
  flakiness mitigation needed at the 40× margin.
- If a budget must move, change it HERE and in the test in the same PR,
  with the measurement that justifies it.

## Fuzz-lite (E9: "panic-free on bad input")

`crates/tole-core/tests/fuzz_lite.rs` — deterministic LCG-seeded corpus
(no rand dep; same corpus every CI run):

- **400 hostile inputs × 5 tools** (read/edit/delete/git/run_command):
  path traversals, control chars, bidi/zero-width, flag-injection
  (`--upload-pack`, `--exec`), shell metacharacters, 300-char blobs —
  every call must settle Ok/Err, never panic.
- **600 fuzzed argv shapes** through `check_destructive_argv`: teardown
  payloads (`dd`, `mkfs`, `shutdown`, …) must never pass.
- **Truncation invariants**: a 40 MiB child stream comes back capped at
  32 MiB **with the marked suffix** — a bounded view is always visibly
  bounded.

## Cross-build CI

`aarch64-apple-ios` (macos-14) + `aarch64-linux-android` (ubuntu + NDK)
compile-checks per PR (`cross-compile` job) since #82.
