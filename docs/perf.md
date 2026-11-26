# tole — Performance Record (E9)

Threshold benchmarks, CI-enforced (`perf_threshold` and `perf_replay` test
binaries). These are **regression gates**, not microbenchmarks: generous
budgets that fail when the harness itself regresses. Provider latency is
excluded (MockProvider — no network); what is measured is the harness's own
durability tax.

## Budgets (2026-09-28, v0.3.0-era baseline)

| Scenario | Budget | What it covers |
|---|---|---|
| Text-only turn (1 prompt → 1 final) | < 2,000 ms | one full state-machine cycle + JSONL commits |
| 10-call tool chain | < 2,000 ms | 10 × the full intent→effect→settle sandwich, each with its own durable commit |
| Session replay (open, ~50+ entries) | < 2,000 ms | crash-resume replay cost — decides whether long sessions stay practical |
| Long-session replay (open of ~1,000 and ~3,000 entries, unoptimized) | < 5,000 ms | `perf_replay` (#1019): opening a log built by the real turn loop; the default run also checks that the cost grows linearly (see "Long-session replay") |

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

## Long-session replay (#1019, 2026-11-26)

Opening a session folds its whole JSONL log into memory
(`JsonlStorage::open`), and the first turn after a resume scans the entries
once more (taint seeding, the denial check). The question of #1019 is whether
that cost, which grows with the age of a session, needs a checkpoint entry.
`crates/tole-core/tests/perf_replay.rs` measures it: it builds sessions through
the real turn loop and JSONL writer (`MockProvider`, no network), then times the
writable `open`, the read-only `open_ro` (listings, status) and one text-only
turn on the reopened session (`first turn`, run with a `TaintTracker` set, as
real hosts do). The numbers below come from it.

Fixtures (deterministic; the same file every run):

- one fixture turn is 26 entries: a user message, 12 tool calls (an `intent`
  and a `tool_result` each) and a final answer; 20,000 entries are about 770
  such turns and 100,000 about 3,850;
- `small`: every tool result is about 200 bytes, so the cost per entry shows
  alone;
- `mixed`: 60% of the results about 200 B, 30% 4 KiB, 10% 32 KiB (a file read or
  a command log), 1.5 KiB final answers, and two image references on the user
  message of every fifth turn (a reference is a path, a size and a hash in the
  log, never the blob).

Machine: Intel Xeon @ 2.1 GHz, 4 cores (`nproc` 4), Linux 6.18 x86_64, rustc
1.97.0, `cargo test --release` (opt-level 3, thin LTO). The files are in the
page cache (a fixture is read right after it was written or used before), the
disk is a virtual `/dev/vda`. Another build ran on the same machine during the
runs (load average about 2), so the figures carry about +-30% noise. Each
`open` and `open_ro` cell is the range of ten runs, each the median of 3 to 5
opens. The `first turn` and `RSS` cells are three runs with one profile and one
size per process and the fixture built by an earlier process (the only way the
`RSS` column is valid: earlier builds and opens of the same process leave heap
behind that a later open reuses, and the test prints `n/a` for such rows).
`RSS` is the `VmRSS` growth of that process around one open, with the storage
kept alive. `first turn` is a single sample per run and includes an fdatasync,
so it has a floor of about 5 ms and a size dependence below that is not visible.

| Profile | Entries | Log | open | open_ro | first turn | RSS |
|---|---|---|---|---|---|---|
| small | 1,014 | 0.5 MiB | 3.9-5.7 ms | 3.3-5.3 ms | 5.5-6.1 ms | 2 MiB |
| small | 5,018 | 2.4 MiB | 19-32 ms | 17-30 ms | 5.6-6.2 ms | 10 MiB |
| small | 20,020 | 9.6 MiB | 73-136 ms | 71-109 ms | 8.9-12.5 ms | 36-37 MiB |
| small | 100,022 | 48.6 MiB | 368-525 ms | 367-556 ms | 18.9-19.6 ms | 179 MiB |
| mixed | 1,014 | 2.8 MiB | 7.0-11.2 ms | 6.2-10.3 ms | 4.9-6.5 ms | 5 MiB |
| mixed | 5,018 | 13.2 MiB | 34-53 ms | 33-50 ms | 5.7-6.2 ms | 20 MiB |
| mixed | 20,020 | 51.1 MiB | 129-159 ms | 122-203 ms | 8.2-17.2 ms | 76 MiB |
| mixed | 100,022 | 261.7 MiB | 662-859 ms | 632-778 ms | 19.6-20.5 ms | 381 MiB |

Reading the table:

- Replay is linear. From 5k to 100k entries (20 times) the median `open` grows
  19 to 20 times. It costs 3.7 to 6.8 us per entry for `small` entries and 6.4
  to 11 us per entry for `mixed` (at 100k: 3.7 to 5.3 and 6.6 to 8.6 us; the
  spread is mostly the +-30% noise, the small sizes carry the highest per-entry
  figures), which is about 3 ns per byte of log at 100k `mixed` (roughly 350
  MB/s of JSON parsed). A log line is parsed once into `serde_json::Value`s;
  nothing in `open` is quadratic. `open_ro` costs the same as `open` (the lock
  and the torn-tail check are not the cost).
- The first turn (on a copy of the log, flushed first) sits on a floor of about
  5 to 6 ms, its own durable commits, up to 5,000 entries. Above that a growth
  shows: 8 to 17 ms at 20,000 entries and 19 to 20 ms at 100,000 (about 0.14 us
  per entry over the floor), the same for both profiles. It appeared when the `TaintTracker` was
  set (without one `seed_taint` returns at once and the column was flat), so it
  is consistent with the scan of the transcript before the first provider call.
  It is not broken down further. The stop-gate scan and the host's assemble
  work (config, tool roster, MCP servers) are not part of this column.
- Memory grows with the log: about 1.8 KiB per `small` entry (the entry, its
  `Value` tree, the id index) and about 3.9 KiB per `mixed` entry, which is
  about 1.5 times the size of the log file (it holds the strings and the
  tree). 100,000 entries hold 180 MiB (`small`) to 380 MiB (`mixed`) in the
  process.
- The unoptimized build that CI runs is about 6 times slower (`small`: 1,014
  entries 24 to 30 ms, 3,016 entries 71 to 83 ms, about 24 us per entry), which
  is the default run of the test: it builds the two `small` sizes in about 4 s,
  asserts `open` under 5,000 ms (60 times the observation) and, when the
  smaller open takes 20 ms or more, that the cost grows by at most 3 times the
  growth of the log in bytes. An O(n^2) replay fails it; machine noise does not.

Run it yourself. The large sizes are opt-in. Building 100k entries takes one
to two minutes per profile, so `TOLE_REPLAY_DIR` keeps the fixtures and reuses
them (a fixture is trusted by its name, which carries a generator version).
Build them once, then run one profile and one size per process for a valid
`RSS` column:

```text
export TOLE_REPLAY_DIR=/path/to/scratch
TOLE_REPLAY_SIZES=1000,5000,20000,100000 TOLE_REPLAY_PROFILES=small,mixed \
  cargo test -p tole-core --release --test perf_replay -- --nocapture   # builds
TOLE_REPLAY_SIZES=100000 TOLE_REPLAY_PROFILES=mixed \
  cargo test -p tole-core --release --test perf_replay -- --nocapture   # measures
```

**Recommendation (for the owner to confirm; no checkpoint is built here).**
No checkpoint is needed at the sizes measured. A 20,000-entry session (about
770 twelve-call turns) opens in about 0.1 to 0.15 s with 36 to 76 MiB of
memory, and 100,000 entries (a 262 MiB log with large tool outputs) open in
under 1 s, linearly, with about 15 ms more before the first provider call. That
is within the 2 s budget of the first table, which was set for 50 entries and
is met by 2,000 times as many. If a session grows without bound the first
limit is memory (about 2 to 4 KiB per entry, 380 MiB at 100k `mixed` entries),
not time; a checkpoint that only saved the parse would not lower it, because
the provider is still fed from the entries. Primary trigger to revisit: a
measured `open` over 1 s on a supported host (on this machine about 125,000
`mixed` or 220,000 `small` entries), or a reported slow resume. Secondary,
for orientation: the memory of the process that holds the sessions (a
`tole serve` with many long sessions). If either trigger is reached, write the
design note #1019 describes (an append-only checkpoint entry that carries the
folded state, with a verifier that refolds from the full log).

Not measured: a cold page cache or a slow disk (the first read of a large log
adds its I/O time); macOS, ARM and Android hosts; sizes over 100,000 entries;
real sessions' usage rows (one more line per provider step, the mock provider
reports none; same parse path); compaction entries (they do not shorten the
replay, entries are never dropped); the stop-gate scan and the host's assemble
work before a resumed turn; the per-request cost of building and redacting the
provider body from a long history (#908), which a real provider pays at every
step and `MockProvider` does not; `tole serve` holding many long sessions at
once; the `compact` and `fork` paths.

## Secret redaction (#681, 2026-11-14)

`SecretSet::redact` is a byte scan over every needle form of every known
value. #681 added the embedded-base64, hex and `\/` / `\uXXXX` forms (up to 24
needles per value, was 8) and a second scan for text that holds an invisible
character. Measured with `cargo test -p tole-core --release --lib
secrets::tests::throughput -- --ignored --nocapture` (5 secrets, mixed
source-like text, Linux x86_64):

| Text | Before | After |
|---|---|---|
| 4 KiB | 480 MB/s | 350 MB/s |
| 64 KiB | 659 MB/s | 346 MB/s |
| 1 MiB | 664 MB/s | 346 MB/s |

A set with no secrets stays a borrowed no-op, and text without invisible
characters is scanned once (the check reads UTF-8 bytes, no decoding). The
unit test `a_large_text_with_invisible_characters_is_still_linear` redacts
4 MiB that carries one in every line, as a guard against a quadratic path.

## Release perf budget (#921)

A CI gate on the release binary, separate from the threshold tests above:
cold start, idle RSS, active-session RSS and per-turn overhead, measured with
the real `tole` and a mock provider, against the ceilings in
`bench/budget.toml` (ratchet-only-downward). See `bench/README.md`.

## Method

- `crates/tole-core/tests/perf_threshold.rs` — deterministic, no timing
  flakiness mitigation needed at the 40× margin (the ledger fold gate keeps
  ~20× over the unoptimized build; see above).
- `crates/tole-core/tests/perf_replay.rs` — long-session replay (#1019): the
  fixtures come from the real turn loop, the default run is small and the large
  sizes are opt-in through `TOLE_REPLAY_*` (see "Long-session replay").
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

The crate counts were measured again after develop moved on (the #588
discussion ledger landed) and did not change. The `web-builtin` delta is
nginjen-lite and its `scraper` HTML stack
(html5ever, cssparser, selectors, phf, string_cache, tendril and helpers).
A slim build turns it off with `--no-default-features --features
shell-tools,mcp,mcp-http,tui`.

Re-measured on 2026-11-19 at 70cffd7 with the exact release command (`cargo
build --release -p tole-cli`, profile `strip = true`, thin LTO, 1 codegen
unit), on one machine (x86_64 Linux, 4 cores): 23,776,128 bytes stripped
(34,482,104 unstripped; the `strip` step in `release.yml` is a no-op after
cargo's own). The table above lists 19,482,712 bytes for the same command six
days earlier, so the binary grew about 4.3 MB (+22%); the cause is not
bisected. The old "~9 MB" in the `Cargo.toml` comment (#482) is stale.
`--no-default-features`: 10,180,024 bytes. Compressed, the local default build
is 9.8 MB as tar.gz, while the published v0.11.0 Linux x86_64 asset is
8,398,403 bytes; the difference is not explained (version, toolchain, gzip
level or strip step). Variants tried: `lto = "fat"` 21,543,544 bytes (-9.4%,
build 3m52s to 8m32s); `opt-level = "s"` 18,276,776 bytes (-23%; not
benchmarked here, expected to trade hot-path speed). `cargo bloat --crates`
figures cover the `.text` section only (17.1 MiB of the 22.7 MiB binary; the
rest is rodata and other sections): std 2.9 MiB, tole_cli 2.6, rmcp 2.2,
tole_core 2.1, serde_core 1.0, tole 0.9, tokio 0.5, serde_json 0.4, rustls 0.4.

Bisected and attributed since 2026-11-26: see [binary-size.md](perf/binary-size.md).
