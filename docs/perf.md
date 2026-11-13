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

## Automation ledger (#578, 2026-11-11)

| Scenario | Budget | What it covers |
|---|---|---|
| Fold of a 100k-record ledger (10k `job` + 30k × `fire`/`done`/`delivery`) | < 30,000 ms | opening the append-only automation log — the cost every `tole cron tick` pays |
| `ready(now)` over 10k jobs | < 2,000 ms | computing what is due, including the catch-up spans |

Observed on the dev container (Linux x86_64): fold **~1.2–1.4 s** in the
unoptimized test build that CI runs, **~220 ms** in release (≈2.2 µs per
record; 200k records ≈ 510 ms, so linear); `ready` over 10k jobs **~16 ms**
unoptimized, **~6–8 ms** release. The fold budget is ~20× the unoptimized
observation: it exists to catch complexity regressions (an O(n²) fold of
100k records would take minutes), not to race.

## Method

- `crates/tole-core/tests/perf_threshold.rs` — deterministic, no timing
  flakiness mitigation needed at the 40× margin (the ledger fold gate keeps
  ~20× over the unoptimized build; see above).
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

`aarch64-linux-android` (ubuntu + NDK) compile-checks per PR (`cross-compile`
job) since #82. The `aarch64-apple-ios` (macos-14) check is commented out in
`ci.yml` until an Apple runner is available to this repository.

## Binary size and dependencies

Reported, not gated. Release build of `tole` (`cargo build --release -p
tole-cli`, Linux x86_64), unique crates from `cargo tree -e normal --prefix
none | sort -u | wc -l`.

| Build (2026-11-13) | `tole` bytes | tole-cli crates | tole-core crates (`--no-default-features`) |
|---|---|---|---|
| develop before #625 | 18,184,632 | 264 | 58 |
| #625, without `web-builtin` | 18,185,208 | 264 | 58 |
| #625, default (`web-builtin`) | 19,482,712 (+1,297,504, +7.1%) | 299 (30 distinct new crates; 4 build-time proc-macros) | 58 |

The `web-builtin` delta is nginjen-lite and its `scraper` HTML stack
(html5ever, cssparser, selectors, phf, string_cache, tendril and helpers).
A slim build turns it off with `--no-default-features --features
shell-tools,mcp,mcp-http,tui`.
