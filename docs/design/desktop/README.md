# Desktop UI draft (Simple edition)

Images of the desktop design draft, the "Simple edition" page of the Design canvas (sixteen boards). They are a reference for the desktop issues of #527 (epic #523), not a specification: wording, density and sample data are drafts. The editable canvas is not in this repository.

Drawn for people who are not technical: plain words, one window on the local machine, technical detail behind "Show technical details". The look uses the dark tokens of the design canvas (background `#0F1113`, accent `#5CC8B0`, Inter and JetBrains Mono; defined in `crates/tole-desktop-model/src/tokens.rs`). Rows and buttons are about 34 to 38 px; boards S1, S11 and S12 were drawn first with 44 px controls and not redrawn.

Rendered from the board HTML with Chromium at 1x. They were not rendered by GPUI, and nothing here was built; check each part in a small spike (the GPUI Kit names on S13 were read from the source of gpui-component 0.9.1, not compiled).

| Board | Screen | Issue |
|---|---|---|
| [S1](s1-setup.png) | First setup, two screens | #1037 |
| [S2](s2-home.png) | Home with the sidebar | #1030 |
| [S3](s3-task.png) | A task in progress | #1031 |
| [S4](s4-allow.png) | A task waits for an OK | #1032 |
| [S5](s5-approvals.png) | Approvals page | #1032 |
| [S6](s6-automations.png) | Automations page | #1033 |
| [S7](s7-skills.png) | Skills, review before use | #1034 |
| [S8](s8-tools.png) | Connected tools, trust | #1034 |
| [S9](s9-settings.png) | Settings: Safety | #1035 |
| [S10](s10-general.png) | Settings: General | #1035 |
| [S11](s11-kinds.png) | Three kinds of requests | #1037 |
| [S12](s12-moments.png) | Question, problem, done, welcome back | #1037 |
| [S13](s13-map.png) | Plain words and GPUI Kit parts | all |
| [S14](s14-discuss.png) | Discussion report (Get opinions) | #1036 |
| [S15](s15-discussions.png) | Discussions page and new discussion | #1036 |
| [S16](s16-helpers.png) | Settings: Helpers (profiles) | #1035 |

## Intended differences from the technical clients

- Approvals are a page and a card, answered "Allow once" or "Don't allow"; no "always allow" for a single request (the serve decision route is one-shot, and Destructive tools are never auto-allowed by the core).
- Discussions have their own menu: the owner chooses the helpers, or Tole proposes them and the owner confirms before anything runs (a model-started discussion is an approval).
- Skills and connected tools: trust approval from the app is an open owner decision (#1034); nothing a model says can trust a tool.
- Automations run while the local serve runs, if it was started with the cron scheduler (`--cron`); closing the window does not stop them.

See [../../desktop-design.md](../../desktop-design.md) section 0 for the architecture decisions.

## The desktop crates and their guardrails (#1028)

The desktop is two workspace members, so the compiler, not a text search, keeps the toolkit out of the logic:

- `crates/tole-desktop-model`: view state, actions and reducers (tested without a window), `tokens` (colours, sizes and fonts: the dark palette above) and `strings` (every word the window shows). It has no GUI toolkit and no tole dependency, directly or through another crate; today it has no dependency at all.
- `crates/tole-desktop`: the `tole-desktop` binary and the `ui` module that renders the model. Only `src/ui/adapter.rs` names the toolkit (the text check enforces it). It is a thin client of `tole serve`: its one tole dependency is the typed client crate `tole-client` (#1040, allowed by `ALLOWED_FOR_DESKTOP` in `scripts/desktop-lint.py`). It never lists `tole-core`, `tole-tui` or `tole-cli` (the session host lives there) among its own dependencies, in any kind or target. `tole-core` is in its closure only because `tole-client` uses its wire types (`default-features = false`); `tole-tui` and `tole-cli` are in it nowhere. The `supervisor` module (macOS and Linux, no window) attaches to or starts the local serve and is tested without one; the real `tole` binary is exercised by `crates/tole-cli/tests/desktop_supervisor.rs`, because that crate is where cargo builds it (a dev-dependency in that direction only).

Neither crate is published. `cargo test --workspace` stays light because the GUI toolkit is behind the `gpui` cargo feature, which is off by default. `default-members` was not used: `--workspace` ignores it, so it would not have helped, and it would change what a bare `cargo build` does.

`scripts/desktop-check.sh [deps|size|literals|all]` (self-test: `scripts/test-desktop-check.sh`; run `cargo fetch` once first, `deps` reads the dependency graph with `cargo metadata` and `cargo tree`, both `--offline`). "Script exits 1" is all the gate means: CI runs it in the `Desktop` workflow, which is not a required check unless the owner makes it one.

| Rule | Status | What fails |
|---|---|---|
| deps | script exits 1 | Over the resolved graph (`cargo metadata` for declarations and sources, `cargo tree -p` for what a build of the crate reaches, with that crate's own features only; table form, renames, target tables, underscore and case spellings and `workspace = true` are already normalised): `tole-desktop-model` reaches a `gpui*` or `zed*` package or any of `tole-cli`, `tole-core`, `tole-tui`, `tole-client`; `tole-desktop` lists `tole-cli`, `tole-core` or `tole-tui` among its own dependencies, reaches `tole-cli` or `tole-tui` at all, or reaches `tole-core` other than through `tole-client` (checked by pruning `tole-client`'s subtree from the closure), or its `tole-client` is not the workspace crate; a toolkit dependency of `tole-desktop` that is not optional, not `=X.Y.Z`, not from crates.io or not a normal dependency; a toolkit package resolved from anywhere but crates.io (a `[patch]`, a path); a default build that reaches a toolkit package. Secondary, by text, in the `tole-desktop` crate: `gpui*` named outside `src/ui/adapter.rs`, a `path` attribute (also inside `cfg_attr`) and `include!` |
| size | script exits 1 | a `.rs` file over 500 lines or a desktop crate over 4,000 (owner rule D1, 2026-11-20: a crate's ideal is 1,500 to 2,500 lines, the hard ceiling 4,000; the other crates are listed as warnings; they are far above both caps). All checks walk a crate with one function: paths relative to the crate, only the crate's own top-level `target/` skipped, symlinked `.rs` files followed, a dangling link or symlinked directory refused, and a crate with no `src/` or no `.rs` file refused (never a pass over zero files) |
| literals | advisory | warns, never fails: in `src/ui`, numeric colours and sizes (`rgb(0x..)`, `0xRRGGBB`, `px(..)`, `len(8.0)`) and string literals handed to text (`.child("..")`, `.label("..")`, `String::from("..")`, `"..".into()`, `.font_family("..")`) |

The script fails closed: if python3 or cargo is missing, `cargo metadata` fails, a file is unreadable or not UTF-8, there is a dangling symlink, or the helper crashes, it exits 2 with a message and never reports `ok`. The literals check is a pattern match over comment-stripped source and cannot see a literal first bound to a `let`, `const` or `static` and used later, a literal made by a macro or a helper, a number reaching a size through arithmetic on a non-token constant, or text assembled at run time; review still reads the diff. Both scripts belong in the pre-commit list of AGENTS.md section 3 for a change to the crates. The `Desktop` workflow (`.github/workflows/desktop.yml`) runs them, but it is separate from `ci.yml` and a required check only if the owner adds it to the branch protection of `develop`; if so, require only `Desktop model and guardrails`. Its guardrail job always runs; the Linux window build runs when a desktop crate, the root manifest, the lockfile, the desktop scripts or the workflow change; the macOS and Windows window builds run on every push to `develop` or `main`, on manual runs, and on changes to a desktop crate or the workflow (a PR that changes only the lockfile runs the Linux build, and the other two after the merge).

### Building the window

The window needs the `gpui` feature: `cargo run -p tole-desktop --features gpui`. Without it the binary exits with a message and `cargo test --workspace` builds no GUI toolkit. The window is a preview: nothing connects to a `tole serve` yet, so the status reads "Not connected (preview)" (no green dot) and the folders are labelled as sample data until the typed client (#1040) feeds the model.

Pins (all from crates.io, exact, optional, in `crates/tole-desktop/Cargo.toml`; only `src/ui/adapter.rs` names them): `gpui-component =0.9.1`, which itself depends on `gpui-pre =0.3.12` (a crates.io snapshot of Zed's GPUI whose library is named `gpui`), and `gpui-pre-platform =0.3.12` (`gpui_platform`, the OS backends). The plain `gpui` crate on crates.io (0.2.2, October 2025) is an older snapshot that `gpui-component` 0.9.1 does not use, so it is not the pin. An upgrade is a change of that block and of the adapter. The first build compiles about 450 more crates (the lockfile grows accordingly).

Linux build packages (Debian and Ubuntu names): `libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev libx11-xcb-dev libxcb-xkb-dev libfontconfig-dev libvulkan-dev libdrm-dev libgbm-dev libasound2-dev libssl-dev libfreetype-dev`. macOS needs the Xcode command line tools; Windows needs the MSVC build tools.

Checked: the window opens and renders the shell under X11 (Xvfb, software GL) on Linux. Not checked: Wayland, macOS, Windows (the acceptance of #1028 for those needs a person on a real machine).

### Reviewing the UI offscreen

Every desktop page PR (#1030 to #1037) renders the real window offscreen, compares each image with the matching board and checks the keyboard and focus order; the images and findings go in the PR. No `screencapture` (it needs Screen Recording permission). The harness is how:

```bash
scripts/desktop-render.sh /tmp/shots                  # every screen and state, the window's own size
scripts/desktop-render.sh /tmp/shots --size 1440x900  # at the size of the boards
scripts/desktop-render.sh --test                      # the harness checks (below)
```

Run `cargo fetch` and install the Linux build packages above first. The first run builds about 500 crates (about 20 minutes on 4 cores; later runs rebuild only the desktop crates and reuse `target/desktop-render`, or your `CARGO_TARGET_DIR`).

**What it writes.** One PNG per capture in the directory you name, and `index.tsv` (file, board, keys pressed, width, height, bytes). The captures are `tole_desktop_model::shell_captures()`: the preview with the sample folders (`shell-preview`, the "Not connected (preview)" state), a selected folder, a pending-approvals badge, each section, each connection status, no folders, a real (non-sample) folder, and two keyboard-focus images (`shell-focus-first-section`, `shell-focus-first-folder`). A page PR adds the captures of its screen there, with the board each is compared with. The images are 2x (GPUI's test platform has a fixed scale factor of 2): 2400x1560 for the default 1200x780 window.

**How it works, and why it is a script.** `Window::render_to_image` and GPUI's headless test platform need the `test-support` feature of `gpui-pre` and `gpui-pre-platform`. That feature must not reach the committed graph, so the script never edits the repository: it copies the two desktop crates, `tole-client` and `tole-core` (the closure of the desktop's one tole dependency) and a trimmed workspace manifest to a temporary directory, adds the two packages as dev-dependencies there (same exact versions, read from the committed pins, plus `test-support`), builds the `render` example with the `render-harness` feature and deletes the copy. `Cargo.toml`, `Cargo.lock`, the pins and `scripts/desktop-check.sh deps` are untouched, and `git status` is the same after a run. `render-harness` is declared in the committed manifest (`render-harness = ["gpui"]`) so the harness code compiles under a known feature, but it needs the dev-dependencies, and no committed build or CI job enables it (its example and test have `required-features`). The code is `src/ui/harness.rs` (no toolkit type) over the `offscreen` module in `src/ui/adapter.rs` (the only file that names the toolkit). It opens the same `Root`-wrapped view as `tole-desktop` does, after the same `adapter::init`, so the keys and the component library are set up alike. No window is shown and no display server is needed: the text system comes from GPUI's headless platform (system fonts: install Inter and JetBrains Mono, as for the real window) and wgpu draws the frame into a texture. On Linux that needs a Vulkan adapter (Mesa's software lavapipe is enough, `mesa-vulkan-drivers`); without one, try `xvfb-run -a scripts/desktop-render.sh ...`.

**Comparing an image with a board.** Boards are 1440x900 at 1x (`docs/design/desktop/s*.png`, from Chromium); the window opens at 1200x780, so render with `--size 1440x900` to compare like with like, and halve the 2x image (`convert shell-preview.png -resize 50% half.png`, ImageMagick) or view both at the same display size. `index.tsv` names the board of each image. The shell's main area is still a placeholder, so for now compare the top bar, the sidebar (section rows, caption, folders and the sample note) and the connection footer; judge, per state: the colours (dark palette of `tokens.rs`), the row height (34 px, 68 px in the image), the words (`strings.rs` against the board), the selected and the focused row, and the status dot. The boards are drafts, not a specification: a difference that the design should absorb is a finding for the owner, not a bug. `compare -metric AE a.png b.png null:` counts differing pixels if a number helps; it will not be zero.

**Keyboard and focus check.** Nothing in GPUI binds Tab, so the app does (`adapter::init`: `tab` and `shift-tab` move to the next or previous tab stop of the active window), and each row of the shell is a tab stop with a visible accent ring (`focus_visible`). The model owns the order (`ShellState::tab_order()`: the sections in sidebar order, then the folders in list order). The harness drives the real window: `harness::focus_walk(state, &["tab", "tab", ...], size)` presses each key through `Window::dispatch_keystroke` (the path a keyboard event takes in the test platform), repaints, and returns which row has focus after each press; `crates/tole-desktop/tests/render.rs` asserts that Tab visits the rows in the model's order and wraps to the first, that Shift+Tab walks it backwards from the last, and that the ring is on screen (the accent colour appears in `shell-focus-first-section` and not in `shell-preview`). A page PR extends `tab_order()` with its controls, adds a capture with the keys that reach each one, and adds the order to that test.

**`--test`** runs those checks (and the unit tests of the harness): every capture is written as a PNG, non-empty, of the window size times the scale factor, the same size in its header as in memory, not a blank frame, with the sidebar in the panel colour and the main area in the background colour of the tokens, and `index.tsv` lists each image.

**Not covered.** The test platform is not a platform window: window creation, the compositor, real key events from the OS, IME and screen readers stay hand tests (checklist of #1028). The frame is drawn by whichever wgpu adapter the machine has; a software rasteriser antialiases a little differently from a GPU. Only Linux is checked: the code for macOS (Metal) and Windows is GPUI's own and is not run here. Enter and Space do not activate a row yet.

The `Desktop` workflow has an optional manual job for the same run: start it with the `render` input; it attaches the images to the run. It is not part of any PR run and not a required check.

### Supply chain

Adding the `gpui` feature changes `Cargo.lock` for every build, even one without the feature, because the lockfile lists the optional dependencies too. Facts, from the lockfile and the crates.io metadata at the time of writing:

- `gpui-pre` and `gpui-pre-platform` (and their `gpui-pre-*` siblings) are crates.io snapshots of `zed@d2a4fb6` (repository `zed-industries/zed`). `gpui-component` (and `gpui-base`, `gpui-kit-assets`) come from `longbridge/gpui-kit`. Who owns these crates.io names was not verified.
- The lockfile grows from 324 to 775 package names. New crates include pre-releases and forks: `ropey 2.0.0-beta.1`, `zed-font-kit`, `zed-scap`, `zed-xim`, `core-graphics-helmer-fork` and `gpui-pre-reqwest` (a fork of `reqwest`).
- Existing crates that the default `tole-cli` build also uses moved to newer versions: `icu_*` 2.3.x to 2.4.0, and `tinystr`, `yoke`, `zerovec`, `zerovec-derive`.
- `cargo audit` and the Trivy scan of `security.yml` read the whole lockfile, so an advisory in the new tree would fail them for every PR. `cargo audit` was not available where this was authored; the `Cargo Audit` job of CI is the first run on this lockfile.

The owner accepted this supply chain on 2026-11-20 (the pins above, optional behind the `gpui` feature). No dependency was changed to reduce it.
