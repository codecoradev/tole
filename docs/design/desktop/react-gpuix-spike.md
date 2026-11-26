# React on GPUIX, M0 validation gates: spike report (DRAFT, 2026-11-25)

Status: **PARTIAL: the Linux half of M0 (#1240, epic #1241).** G6, G7 and G8 were run on a Linux x86_64 cloud container; G2, G3, G4, G5 and the macOS
half of G1 are NOT done (they need a Mac) and are marked as such. This report is merged as it stands so the numbers and the open items are in the
repository; it is completed when the Mac kit has been run, and M1 does not start before G2 and G7 pass and the owner has given the six confirmations
of `docs/desktop-design.md` section 0.1. The code behind it is throwaway and outside the repository (a container directory and local branches that
were never pushed). Nothing here is a decision; the owner confirms or changes the gates.

## Summary

| Gate | Verdict | Where | Evidence |
|---|---|---|---|
| G1 no fork, exact pin | PASS on linux-x64, macOS pending | Linux | stock `@gpuix/native` and `@gpuix/react` 0.11.0 exact, React 19.2.0, no patch; macOS addon SHA via the Mac kit |
| G2 IME | **NOT RUN (blocks M1)** | macOS | Mac kit, `g45-form.tsx`, checklist in its README |
| G3 frame cost vs Rust baseline | NOT RUN on a Mac (both instruments built and run on Linux) | macOS | Mac kit `g3-stream.tsx` (GPUIX) and `frame-bench.patch` (real-window Rust instrument, same overlay quantity) |
| G4 idle | NOT RUN (Linux numbers invalid, software GL) | macOS | Mac kit `g4-idle.mjs` |
| G5 accessibility | PARTIAL | macOS | aria/role reach the native batch on Linux (45 ops, 17 elements); VoiceOver and Tab order need macOS |
| G6 Tailwind | **PASS** for the subset used | Linux | 123 of 123 classes of S9 and the transcript compile, zero hand-written style objects |
| G7 pipe | **PASS** | Linux | p95 0.32 ms (900 px viewport), 0.59 ms (about 40 rows), 0.26 ms (snapshot); messages 9 to 62 KB, under 256 KB |
| G8 effort | PASS | Linux | S9 + shell + transcript 275 lines of TSX; compiler about 660 lines; about 16 minutes |

## Environment

Linux x86_64 container, no GPU. Bun 1.4.4. Stock addon `@gpuix/native-linux-x64-gnu` 0.11.0 needs `libxkbcommon-x11-0`;
software Vulkan from apt `mesa-vulkan-drivers` (lavapipe) under Xvfb. Frame, idle and CPU numbers from this setup are not
valid for G3 and G4 and are not reported as gate results.

New finding: the automation call `app.screenshot()` fails on Linux ("captureScreenshot needs a test-support build on macOS
or Windows"). Click, fill and `textContent` work. Pictures were taken from the Xvfb screen.

## G6 and G8: Tailwind to GPUIX, S9 and a transcript

Built: a build-time compiler that runs Tailwind's own `compile()` (tailwindcss 4.3.5, `@theme` from `tokens.rs`), resolves
`var()` and `calc()`, and maps declarations to GPUIX styles through a property table; a JSX transform (`className` becomes
`style`, no Tailwind at runtime) and a `tw('...')` function. Screens: board S9 "Safety" with its real controls plus an
extra switch, native input and Save card (labelled "not on the board"), and a transcript with a 2,000-row `<virtual-list>`,
a `<markdown>` block every 10th row and a `<diff>` block every 50th.

- Coverage: 123 classes used, 123 compile. A broad probe of 378 typical classes: 288 compile (76%), 89 cannot be expressed.
- Fidelity to the board: about 90% at a glance. Palette and structure match; layout is within about 10 px. Missing: sidebar
  and top-bar icons, a real select popup, some card padding. Pictures: `compare-s9-board-over-ours-1440.png`.
- GPUIX knows `hover`, `active` and `focusVisible` only. `focus:` and `disabled:` are resolved in a small JS wrapper.
- Cannot be expressed in GPUIX: z-index, transform, transition and animation, filters, letter-spacing, text-transform,
  italic, `mx-auto`, `vh` and `vw`, fixed and sticky positioning, `group`/`peer`/`first`/`last`/`placeholder`/`aria`/`data`
  variants, `space-*`, `divide-*`, responsive and `dark:` variants, `aria-checked`, `aria-disabled`, `aria-labelledby`.
- Not implemented by the spike although GPUIX can: gradients, coloured shadows. `ring-offset-*` compiles and has no effect.
- Classes are merged in class-string order, not in Tailwind's cascade order.
- `keyboardFocusDim: false` is needed to stop GPUIX dimming other focusable elements.
- Known defect of the spike scanner: `--strict` exits 1 because it treats words in text ("Safety", "you") as classes (4 false
  positives of 127 candidates). A real scanner must read `className` attributes through the AST.
- A comparison with Rust is not possible: there is no S9 page on the Rust side yet.

## G7: the pipe

Built: a Rust stub (`crates/tole-desktop/examples/pipe_stub.rs`, 1,105 lines, local branch only) speaking protocol v1 as JSON
lines over stdio, serialising the **real** model and driver types (not mirrors), and a Bun consumer (`bench.mjs`).
The result was reproduced independently in a second run (same p50, p95 within 10%).

| Measure | p50 | p95 | Size |
|---|---|---|---|
| `set_viewport`, 900 px viewport | 0.13 ms | 0.30 to 0.32 ms | 9 KB median |
| `set_viewport`, about 40 rows | 0.33 ms | 0.54 to 0.59 ms | 31 KB median, 62 KB max |
| `snapshot` | 0.14 to 0.17 ms | 0.26 ms | 14.4 KB |

Round trips include `JSON.parse`; n = 600. Hello message 10.8 KB. The whole 2,000-row task as one message: 2.37 MB (9.1 times
the 256 KB cap), so windowing is mandatory. 60 pushes per second for 10 s: 0 sequence gaps, core 1.5% CPU, Bun 2.8% CPU.
A stalled reader at 2,000 Hz: 8,001 notifications collapsed into 40 messages, memory flat at about 9 MB.
The 256 KB cap holds both ways (262,144 bytes accepted, one more refused, a 64 MB line refused without memory growth);
malformed JSON, invalid UTF-8, an unknown command and bad parameters answer with errors and keep the channel; stdout carries
JSON only, stderr logs only; the core exits with code 0 within 3 ms of stdin closing.

Type generation: `schemars` (draft-07) gives an 83 KB schema; `json-schema-to-typescript` 16.0.0 turns it into 27.5 KB of
types that compile under strict `tsc`, and `ajv` found 0 violations on real messages. The 2020-12 default loses tuple types.
serde's default enum tagging produces `{User:{...}}` shapes without a `kind` discriminant.

Serde cost: 67 types needed `Serialize` + `JsonSchema` behind a `schema` feature (59 in `tole-desktop-model`, 8 in
`tole-desktop`); 65 are reachable from `BackendView` and `TaskView`. `schemars` is already in `Cargo.lock`.

### Items M1 must solve (found by the spike)
1. A `BackendView` at the model's caps is 404 KB (1.54 times the cap: approvals 101 KB, skills 105 KB, automations 198 KB).
   It has to go out as per-section messages with windowed lists.
2. One row can be 200,000 characters (`MAX_ANSWER_CHARS`): a `row_text` chunk fetch is needed and was not built.
3. The string table has 137 constants but 77 functions with free arguments. The core must send resolved text, or the TS side
   re-implements them; the stub reads constants by parsing the source, which is a hack.

### Alternatives
- napi addon in the same process: not built. The pipe costs 0.1 to 0.3 ms against a 16.7 ms frame, so napi has no performance
  case and gives up crash isolation.
- TypeScript client straight to `tole serve`: the `tole-client` schema generates 934 lines of types easily, but the model,
  adapter, driver and backend (about 44k lines, about 25.6k outside dedicated test files, about 911 tests; with supervisor
  and app 990 tests) would be duplicated in TypeScript. The epic's 33k figure was not reproduced; this breakdown is measured.
- Conclusion: the sidecar stays the choice.

Not measured: macOS, a real `tole serve`, React in the loop (the frame budget is pipe + parse + render).


## G3: how the baseline is measured (found by the spike)

The assumption of #1240 ("the recorded Rust harness on the same Mac") does not hold: `scripts/desktop-render.sh --test` is
Linux-only (GPUI scene tests are ignored on macOS, #1172) and measures offscreen software rendering in a debug build.
Stock `gpui-pre =0.3.12` (the pin tole uses) already contains the same frame overlay as GPUIX (`Window::draw` duration, shown as
CUR / 1% / 10% / MAX), behind its `profiler` feature, with no public getter; its foreground journal (`FrameTiming`,
`draw_end - draw_start`) gives the same numbers through public API. A throwaway real-window instrument was built from that
(`crates/tole-desktop/examples/frame_bench.rs`, local branch `spike/g3-rust-baseline`, 6 files, +759, adds `hdrhistogram` 7.6.0 to
the lockfile, must not be merged) with a 2,000-row task and the same two phases as the GPUIX app (20 s of streaming at about 30
tokens/s, then 100 scrolls of 400 px up). It ran on Linux under Xvfb + lavapipe (frames at a fixed 48 ms there, so the numbers are
invalid for the gate; it proves the path and the JSON shape; on-screen overlay values equal the JSON's `overlay_as_read_on_screen`).
Caveat of fairness: the Rust rows are real Markdown with diff blocks in a 1200x780 window, the TSX rows are plain text in 1000x700,
so Rust is heavier by construction. The comparable quantity is overlay draw time; compare both with the frame budget.

## Still open before M0 can close
1. Run the Mac kit: G1 on macOS, **G2 (blocks M1)**, G3, G4, G5.
2. Run both G3 instruments on the same Mac in the same session (`frame-bench.patch` for Rust, `g3-stream.tsx` for GPUIX).
3. Owner decision on the six confirmations of `docs/desktop-design.md` section 0.1 and on the gate thresholds.
4. Decide how `focus:` and `disabled:` are handled in #1244 (JS wrapper versus a design rule), and what to do with the
   classes GPUIX cannot express (forbid them in a lint, or map them to the nearest equivalent).
