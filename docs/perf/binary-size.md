# Release binary size

Measured 2026-11-26 on `develop` at 146ee4c, for issue #991. This page records
measurements and a recommendation. It changes no default: the release profile
and the default features are the owner's decision.

Sizes are bytes of the stripped `tole` for `cargo build --release -p tole-cli`
(default features), x86_64 Linux, unless a row says otherwise. "MB" is
1,000,000 bytes.

## Summary

- The binary did not grow 4.3 MB in six days from one cause. Between v0.10.1
  (2026-11-04) and `develop` it grew from 11,416,376 to 24,402,272 bytes
  (+12,985,896, +114%). The #991 window (19.47 to 23.78 MB, 2026-11-13 to
  2026-11-19) is the last third of that.
- Three steps are separable, each measured by building the commit and its
  parent: the streamable-HTTP MCP client (#533, reqwest and the rmcp 3.9 bump)
  **+3.33 MB**; the `uteke-privacy` secret detector, which pulls in the whole
  `regex` stack (f525fbb) **+1.58 MB**; the built-in web tools (#672,
  `web-builtin`) **+1.31 MB**. The rest is first-party code, spread over
  hundreds of commits; between 268d75d and ac726bc no commit adds more than
  about 930 lines of source.
- Inside the #991 window the split is 2.15 MB (50%) first-party code before
  f525fbb, 1.58 MB (37%) the regex stack in that one commit, 0.57 MB (13%)
  first-party code after it.
- Everything ships by default. A user can switch off `mcp` (-9.15 MB),
  `mcp-http` (-1.98 MB), `web-builtin` (-1.31 MB) and `tui` (-0.63 MB); the
  regex stack and the first-party core cannot be switched off.
- The measured profile levers are larger than any single dependency fix:
  `opt-level = "s"` -5.55 MB, `lto = "fat"` -2.20 MB, `panic = "abort"` -2.78
  MB, all three -10.76 MB (see "Options").

## How it was measured

One machine: Linux 6.18 x86_64, 4 vCPU Intel Xeon 2.10 GHz, 16 GB RAM; rustc
and cargo 1.97.0 (2026-07-07); GNU strip 2.42. Same command and profile as
`release.yml` (`cargo build --release -p tole-cli`, host target
`x86_64-unknown-linux-gnu`): the root `[profile.release]` is `opt-level = 3`,
`lto = "thin"`, `codegen-units = 1`, `strip = true`, with `--locked` and `-j
2` (the patched `uteke-privacy` experiments run without `--locked`, because
the lockfile changes).

```bash
# one build per commit or feature set; symbols kept so nm can read them
git checkout --detach <rev>
CARGO_PROFILE_RELEASE_STRIP=false cargo build --release -j 2 --locked -p tole-cli [features]
strip --strip-all -o tole.stripped target/release/tole       # the number in the tables
size -A tole.stripped                                        # section sizes
tar czf tole.tar.gz tole                                     # as release.yml packages it
cargo tree --locked -p tole-cli -e normal --prefix none | sed 's/ (\*)//' | sort -u | grep -v '^tole' | wc -l
python3 bench/run.py measure --bin tole.stripped --report-only   # #921 harness, median of 5 runs
```

Checks that the method is sound:

- At 70cffd7 the unstripped binary is 34,482,104 bytes, exactly the figure in
  #991 and `Cargo.toml`; stripped it is 23,776,032 against 23,776,128 from
  cargo's own `strip = true` (96 bytes).
- Local tar.gz sizes are 0.5 to 0.7% below the published release assets
  (v0.10.1 4,898,983 against 4,923,963; v0.11.0 8,340,833 against 8,398,403;
  v0.12.0 9,818,446 against 9,886,436), so the numbers are comparable with
  what users download.
- `cargo bloat` is not installed here. Per-crate bytes come from `nm -S -C
  --defined-only` on the unstripped binary, summed by the first path segment
  of the demangled name (script at the end). Generic code is counted where it
  is instantiated, so `std` includes the monomorphisation of first-party
  generics, and symbol bytes leave out `.eh_frame`, relocations and unnamed
  rodata; the stripped file is about 1.25 times the symbol total.
- Experiments use `CARGO_PROFILE_RELEASE_*` variables, `--config`, or a
  `[patch.crates-io]` entry added to an uncommitted manifest and reverted. No
  manifest, profile or source change is part of this page's PR.
- Build wall time is not reported: dependency builds were partly served by a
  compile cache, so times are not comparable between rows.

## Size over time

| Build | Commit | Date | Stripped bytes | Step | tar.gz |
|---|---|---|---:|---:|---:|
| v0.10.1 | 95e92e6 | 2026-11-04 | 11,416,376 | | 4,898,983 |
| #533 HTTP MCP client | b04ff42 | 2026-11-10 | 14,743,208 | +3,326,832 | |
| #672 web-builtin, the "before" of #991 | 268d75d | 2026-11-13 | 19,473,344 | +4,730,136 | 8,051,006 |
| v0.11.0 | 52cd7bf | 2026-11-14 | 20,144,488 | +671,144 | 8,340,833 |
| #789 | a434107 | 2026-11-15 | 20,747,064 | +602,576 | |
| test(sandbox), parent of the regex step | ac726bc | 2026-11-17 | 21,626,848 | +879,784 | |
| `uteke-privacy` (regex stack) | f525fbb | 2026-11-18 | 23,209,768 | +1,582,920 | |
| #984, the "after" of #991 | 70cffd7 | 2026-11-19 | 23,776,032 | +566,264 | 9,782,940 |
| v0.12.0 | 33a7d04 | 2026-11-20 | 23,860,024 | +83,992 | 9,818,446 |
| develop | 146ee4c | 2026-11-26 | 24,402,272 | +542,248 | 10,057,395 |

b04ff42's parent, b573af2 (the develop commit before #533), builds to the
same 11,416,376 bytes as v0.10.1. The table's "before" (19,473,344) is 9,368 bytes
(0.05%) under the 19,482,712 in `docs/perf.md` for the same change; that page
does not record its exact commit or toolchain.

The "unexplained" gap in #987 between the local 9.8 MB tar.gz and the 8.4 MB
published v0.11.0 asset is explained by the table: the local build was 70cffd7
(2026-11-19), the published v0.11.0 is the 2026-11-14 code, before the regex
stack and six days of work. The same code built here gives 8,340,833.

## Attribution

### The three separable steps

| Step | Evidence | Bytes |
|---|---|---:|
| #533: reqwest, h2, hyper-util, rmcp 3.3 to 3.9 | build of b04ff42 and its parent | +3,326,832 |
| f525fbb: `uteke-privacy` and `regex` | build of f525fbb and its parent | +1,582,920 |
| #672: `web-builtin` | `develop` built with and without the feature | +1,312,536 |

- #533 symbol bytes, v0.10.1 to b04ff42: rmcp +705,665, `std` +443,098, h2
  +280,839, reqwest +153,217, hyper-util +127,980, tokio +92,752, hyper
  +80,323, `tole_core` +55,359. Crates in the tree: 158 to 204. The commit is
  one change (`crates/tole-core/Cargo.toml`: rmcp features
  `transport-streamable-http-client-reqwest` and `reqwest-tls-no-provider`,
  plus `mcp.rs`), and it is inside the `mcp` feature, so `mcp` cannot be split
  from it today.
- f525fbb symbol bytes: `regex_automata` +274,433, `regex_syntax` +178,060,
  `aho_corasick` +145,492, `uteke_privacy` +47,211, `memchr` +22,741;
  first-party `tole_core` +11,895 and `tole_cli` +2,764. By section the
  1,582,920 bytes are `.text` +864,576, `.rodata` +278,528 (Unicode tables),
  `.rela.dyn` +179,400, `.data.rel.ro` +146,232, `.eh_frame` +78,792 and about
  35,000 in smaller sections. The #991 comment put the regex stack at about
  0.6 MiB of `.text`, which matches the symbol bytes (0.64 MiB), but the file
  grew 1.51 MiB because the other sections grow with it.
- `regex` comes only through `uteke-privacy 0.24.1` (`cargo tree -i regex`).
  `tole-cli` already uses `regex-lite` for its own filters.

### The rest is first-party code

Symbol bytes of the first-party crates (`tole_cli`, `tole_core`, `tole`,
`tole_tui`): 1,929,772 at v0.10.1, 3,966,933 at 268d75d, 5,851,140 at 70cffd7,
6,130,379 at 146ee4c. The Rust source of those crates (files without
`*_tests.rs`, inline test modules included) went from 46,882 lines at v0.10.1
to 84,254, 117,329 and 125,059 lines.

Between 268d75d and ac726bc (156 commits, 2.15 MB) no dependency was added
(`Cargo.lock` differs only by version bumps of `toml`, `toml_datetime`,
`toml_writer`, `serde_spanned`, `uuid` and `yoke-derive`); the commit at which
half of the lines of that range were added (a434107) already carries
+1,273,720 bytes, so the growth is steady, not a step. Between 70cffd7 and
`develop` 64 commits touched `tole-cli` or `tole-core` for +626,240 bytes. A
per-module table from `nm` is not given: module paths change as code is moved
or inlined (`tole_cli::session` has no symbols at 268d75d and 237,969 bytes at
70cffd7), so it would not be reliable.

### Symbol bytes by crate group

| Group | v0.10.1 | 268d75d | 70cffd7 | 146ee4c |
|---|---:|---:|---:|---:|
| first-party | 1,929,772 | 3,966,933 | 5,851,140 | 6,130,379 |
| `std`, `core`, `alloc` | 1,977,639 | 3,222,196 | 3,751,896 | 3,815,900 |
| rmcp | 1,700,312 | 2,404,852 | 2,418,948 | 2,444,323 |
| HTTP stack (h2, hyper, hyper-util, reqwest, http, tower) | 233,183 | 960,649 | 963,640 | 956,945 |
| tokio | 390,802 | 483,554 | 487,482 | 487,562 |
| TLS (rustls, webpki, ring) | 594,662 | 668,316 | 670,789 | 670,755 |
| regex stack (`memchr` was already present, 5,503) | 5,503 | 5,503 | 672,795 | 672,795 |
| web-builtin HTML stack (html5ever, selectors, scraper, ...) | 0 | 401,550 | 401,550 | 401,550 |
| serde, serde_json, toml | 709,554 | 1,025,925 | 1,033,320 | 1,039,906 |
| clap | 244,565 | 288,224 | 295,509 | 300,343 |
| all other crates | 1,315,607 | 2,097,713 | 2,145,694 | 2,257,715 |
| total | 9,101,599 | 15,525,415 | 18,692,763 | 19,178,173 |

The largest third-party crate is rmcp (2.44 MB of symbols); `std` is large
because generic code is instantiated there for rmcp, tokio and the first-party
crates.

## What ships by default

`tole-cli` defaults: `shell-tools`, `mcp`, `mcp-http`, `tui`, `web-builtin`.
The release workflow builds exactly that, so every published binary carries
all of it. A user switches parts off with `cargo install tole-cli
--no-default-features --features <list>`. Sizes at 146ee4c:

| Features | Stripped bytes | vs default | tar.gz | Crates |
|---|---:|---:|---:|---:|
| default (`shell-tools,mcp,mcp-http,tui,web-builtin`) | 24,402,272 | | 10,057,395 | 243 |
| without `web-builtin` | 23,089,736 | -1,312,536 (-5.4%) | 9,634,133 | 213 |
| without `tui` | 23,768,560 | -633,712 (-2.6%) | | 211 |
| without `mcp-http` (keeps `mcp`) | 22,420,552 | -1,981,720 (-8.1%) | | 240 |
| without `mcp` (and so `mcp-http`) | 15,250,160 | -9,152,112 (-37.5%) | 6,433,296 | 182 |
| `shell-tools,tui` only | 13,707,072 | -10,695,200 (-43.8%) | | |
| `--no-default-features` | 10,344,608 | -14,057,664 (-57.6%) | 4,461,063 | 85 |

`mcp` is the big one: rmcp, tokio, the whole HTTP and TLS client stack and the
MCP client code. Switching it off removes the MCP client (and the auto-preset
that needs it) and `tole serve --transport mcp`; that is a capability
decision, not a free trim. The `regex` stack, the `std` code and the
first-party core are in every row.

## Options

Saving is against the 24,402,272 bytes of `develop`. "Measured" means a build
of that variant at 146ee4c; "estimated" is stated with its basis.

| Option | Saving | Basis | Cost |
|---|---:|---|---|
| `opt-level = "s"` for everything | -5,545,904 (-22.7%) | measured | slower hot paths; harness per-turn 4.84 to 5.38 ms (below) |
| `opt-level = "s"` for first-party crates only | -3,532,992 (-14.5%) | measured | first-party hot paths only |
| `opt-level = "s"` for dependencies only (`profile.release.package."*"`) | -1,416,136 (-5.8%) | measured | dependency hot paths (rustls, serde_json) |
| `lto = "fat"` | -2,204,360 (-9.0%) | measured | build time; `docs/perf.md` recorded 3m52s to 8m32s (not re-measured here) |
| `lto = "fat"` and `opt-level = "s"` | -8,775,040 (-36.0%) | measured | both of the above |
| `panic = "abort"` | -2,776,768 (-11.4%) | measured | changes failure semantics, see below |
| `lto = "fat"`, `opt-level = "s"`, `panic = "abort"` | -10,758,768 (-44.1%) | measured | all of the above |
| `codegen-units` | 0 | already 1 | |
| `strip` | 0 | already on (unstripped 35,175,056) | |
| `uteke-privacy`: `regex` without unused Unicode tables | -265,600 (-1.1%) | measured, patched copy | upstream change in `uteke-privacy` |
| `uteke-privacy`: `regex` features `std,unicode-perl,unicode-case` only | -782,896 (-3.2%) | measured, patched copy | slower matching (no `perf-*`); upstream |
| `uteke-privacy` on `regex-lite` | -1,459,912 (-6.0%) | measured, patched copy | `\b`, `\s`, `\w` and `(?i)` become ASCII-only; upstream |
| gate the HTTP MCP client behind its own feature | up to 3.3 MB | estimated: the whole #533 step, which also holds first-party `mcp.rs` changes and the rmcp 3.3 to 3.9 bump; what the server side keeps of hyper, h2 and tokio decides the real figure | a new feature and a decision on its default |
| ship a second, lean artifact (`shell-tools,tui`) | -10,695,200 (-43.8%) | measured | a second release matrix entry and support surface |
| a size ceiling in the #921 gate | none | | see the recommendation |

Notes on the measured rows:

- The `uteke-privacy` experiments are on a copy of the crates.io 0.24.1 source
  in a scratch directory, wired in with `[patch.crates-io]`; the copies change
  only the `regex` dependency line (and, for `regex-lite`, the one `use`
  line). Tole cannot do this from its own manifest: Cargo features are
  additive and `uteke-privacy` declares `regex = "1"` with default features;
  its public API also names `regex::Regex`, so the `regex-lite` switch is a
  change to `uteke-privacy` itself. With the `regex-lite` copy, 22 of the
  crate's 23 unit tests pass (the 23rd reads a shared-cases file that is not
  in the crates.io package and fails the same way on the unmodified crate),
  and the 21 tests of `shape_redaction`, `shape_marker`, `shape_allowlist` and
  `secret_encodings` in `tole-core` pass.
- The #991 comment reports `opt-level = "s"` for only the regex crates at
  -153,776 bytes (not re-run here). The dependency-wide row above is the same
  idea at full scale and is also small: most of what `"s"` shrinks is in the
  first-party crates and the `std` code instantiated for them (first-party
  only: -3.5 MB; everything: -5.5 MB).
- `panic = "abort"` is incompatible with the current design: `Cargo.toml`
  documents reliance on unwinding (mutex poison recovery, turns on threads and
  `spawn_blocking`), and `catch_unwind` is used in nine source files of
  `tole-cli`, `tole-core` and `tole-tui` (`approvals.rs`, `cron/webhook.rs`,
  `discuss/owner.rs`, `serve.rs`, `serve/missions.rs`,
  `webhook_out/approval_sender.rs`, `mcp.rs`, `update_check.rs` and the TUI
  `lib.rs` at 146ee4c). With abort each of those would end the whole process,
  including a `serve`, ACP or MCP daemon with other live sessions.

Runtime cost on the #921 harness (mock provider, `bench/run.py measure`,
median of 5 runs per variant, taken interleaved, same machine, nothing else
running). It covers start-up, memory and the per-turn overhead of the harness,
not CPU-heavy paths such as secret redaction or the ledger fold:

| Variant | Cold start ms | Idle RSS MB | Active RSS MB | Per-turn ms |
|---|---:|---:|---:|---:|
| default | 4.04 | 12.45 | 17.73 | 4.84 |
| `lto = "fat"` | 3.98 | 10.80 | 16.16 | 4.65 |
| `opt-level = "s"` | 4.02 | 12.50 | 17.11 | 5.38 |
| `opt-level = "s"`, first-party only | 4.02 | 12.78 | 18.36 | 5.05 |
| `opt-level = "s"`, dependencies only | 4.04 | 12.70 | 17.88 | 5.07 |
| `panic = "abort"` | 4.01 | 12.00 | 17.04 | 4.76 |
| fat and "s" | 4.01 | 10.81 | 15.03 | 5.14 |
| fat, "s", abort | 4.06 | 9.90 | 14.29 | 5.24 |
| without `mcp` | 3.92 | 10.52 | 14.98 | 4.80 |

Across the five runs of one variant the per-turn figure spans 0.2 to 0.5 ms
(default 4.7 to 5.1), so a difference of 0.2 ms is noise and the +0.5 ms of
`opt-level = "s"` is the only clear cost. RSS is less stable than it looks: an
earlier batch of three runs of the default binary, taken before the others,
read 17.4 MB idle and 23.1 MB active against 12.4 and 17.7 here, so treat RSS
differences below about 2 MB as unreliable; the cause of that shift was not
investigated.

## Recommendation

For the owner; nothing here is applied.

1. Treat the growth as three decisions already taken (HTTP MCP client, web
   tools, secret detector) plus first-party code, not as a regression to
   revert. In the #991 window the lockfile gains one dependency family
   (`uteke-privacy`, `regex`, `regex-automata`, `regex-syntax`,
   `aho-corasick`) and nothing else.
2. Ask `uteke-privacy` to depend on `regex-lite` (measured -1.46 MB, and the
   `tole-core` redaction tests pass) or at least to trim its `regex` features
   (measured -0.78 MB). It is the only separable third-party cost left in the
   window and tole cannot do it from its own manifest. `regex-lite` has no
   Unicode classes, so that choice belongs to the `uteke-privacy` owner.
3. If a decision on the profile is wanted, `opt-level = "s"` is the largest
   single lever (-5.5 MB) and costs about 0.5 ms on a 4.8 ms per-turn overhead
   on the mock harness; an agent turn is bound by the provider call, so the
   cost looks small, but the CPU-heavy paths were not measured and should be
   (`cargo test -p tole-core --release --lib secrets::tests::throughput --
   --ignored --nocapture`, before and after). `lto = "fat"` adds -2.2 MB for a
   slower release build and is the weaker trade alone; with `"s"` the pair
   gives -8.8 MB (-36%) with no behaviour change (per-turn 5.14 ms). If the
   hot-path worry dominates, `"s"` on the three first-party crates only gives
   -3.5 MB (-14.5%) with a per-turn figure inside the noise (5.05 ms); the
   dependencies alone give -1.4 MB.
4. Keep `panic = "abort"` off. Its -2.8 MB is real but it turns every guarded
   panic into a daemon exit; do not take it without redesigning those guards.
5. `codegen-units = 1` and `strip = true` are already at their smallest; no
   change.
6. If the distribution size matters more than one binary for everything, a
   second artifact built with `--no-default-features --features
   shell-tools,tui` is 13.7 MB (-44%) and the MCP-capable default stays as is.
   That is a product decision (what the lean build omits), not a build
   setting.
7. #921: add `binary_size_mb` to the gate as a ratcheted ceiling. The harness
   already reports the binary size next to its metrics (`binary 23.3 MB`, in
   MiB, for the 24.4 MB build); the growth here came from many steps of 0.5 to
   3 MB, which a ceiling just above the current 24.4 MB would catch one step
   at a time. The exact ceiling is the owner's choice and is not a
   measurement.
8. Follow-up documentation fixes once a decision is made: `docs/perf.md` ("the
   cause is not bisected", "unexplained" tar.gz gap) and the size comment in
   the root `Cargo.toml` still describe #991 as open; the tar.gz gap is
   explained above.

## Not measured

- Other targets: macOS, Windows and aarch64 Linux were not built. Published
  v0.12.0 assets (compressed): aarch64-apple-darwin 8,898,229,
  aarch64-unknown-linux-gnu 9,186,517, x86_64-pc-windows-msvc 10,072,316,
  x86_64-unknown-linux-gnu 9,886,436 bytes.
- Build time of each variant (compile cache in use).
- Hot-path CPU cost of `opt-level = "s"` beyond the mock harness.
- The part of the first-party growth that belongs to each feature: `nm` module
  paths are not stable across the interval, and 268d75d to ac726bc was
  bisected once (a434107), not per commit.
- How much of the #533 step a gate on the HTTP MCP client alone would remove:
  3.3 MB is an upper bound until it is built.
- `rmcp` itself (2.44 MB of symbols, the largest dependency): its feature set
  was not narrowed.
- A non-PIE link (`.rela.dyn` is 1,017,096 bytes at 146ee4c) was not tried; it
  trades away address-space randomisation.

## Reproduce the per-crate table

```python
#!/usr/bin/env python3
# usage: crates.py <unstripped tole> [topN]
import subprocess, sys, re, collections
out = subprocess.run(["nm", "-S", "-C", "--defined-only", sys.argv[1]],
                     capture_output=True, text=True).stdout
tot = collections.Counter()
for line in out.splitlines():
    p = line.split(None, 3)
    if len(p) < 4 or p[2] not in "tTrRdDbB":
        continue
    try:
        size = int(p[1], 16)
    except ValueError:
        continue
    m = re.match(r"^<*(?:&|\*const |\*mut |dyn |\[)*([A-Za-z_][A-Za-z0-9_]*)::", p[3])
    crate = m.group(1) if m else "[other]"
    tot[crate if crate not in ("core", "alloc", "std") else "std"] += size
for crate, size in tot.most_common(int(sys.argv[2]) if len(sys.argv) > 2 else 25):
    print(f"{crate:28s} {size:10d}")
```
