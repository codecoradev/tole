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
- `crates/tole-desktop`: the `tole-desktop` binary and the `ui` module that renders the model. Once the window lands, only `src/ui/adapter.rs` may name the toolkit (the text check enforces it). It does not depend on `tole-core`, `tole-tui` or `tole-cli` (the session host lives there): it is a thin client of `tole serve`, and will depend on the typed client crate (#1040) once that is on `develop` (an allowlist entry in `scripts/desktop-lint.py`, off now).

Neither crate is published. `cargo test --workspace` stays light because the GUI toolkit will be behind a `gpui` cargo feature that is off by default (the window is a later change; today `tole-desktop` has no toolkit dependency). `default-members` was not used: `--workspace` ignores it, so it would not have helped, and it would change what a bare `cargo build` does.

`scripts/desktop-check.sh [deps|size|literals|all]` (self-test: `scripts/test-desktop-check.sh`; run `cargo fetch` once first, `deps` reads the dependency graph with `cargo metadata` and `cargo tree`, both `--offline`). "Script exits 1" is all the gate means: CI runs it in the `Desktop` workflow, which is not a required check unless the owner makes it one.

| Rule | Status | What fails |
|---|---|---|
| deps | script exits 1 | Over the resolved graph (`cargo metadata` for declarations and sources, `cargo tree -p` for what a build of the crate reaches, with that crate's own features only; table form, renames, target tables, underscore and case spellings and `workspace = true` are already normalised): `tole-desktop-model` reaches a `gpui*` or `zed*` package or any of `tole-cli`, `tole-core`, `tole-tui`, `tole-client`; `tole-desktop` reaches `tole-cli`, `tole-core` or `tole-tui` (or `tole-client` while the allowlist is off); a toolkit dependency of `tole-desktop` that is not optional, not `=X.Y.Z`, not from crates.io or not a normal dependency; a toolkit package resolved from anywhere but crates.io (a `[patch]`, a path); a default build that reaches a toolkit package. Secondary, by text, in the `tole-desktop` crate: `gpui*` named outside `src/ui/adapter.rs`, a `path` attribute (also inside `cfg_attr`) and `include!` |
| size | script exits 1 | a `.rs` file over 500 lines or a desktop crate over 2,000 (the other crates are listed as warnings; they are far above both caps). All checks walk a crate with one function: paths relative to the crate, only the crate's own top-level `target/` skipped, symlinked `.rs` files followed, a dangling link or symlinked directory refused, and a crate with no `src/` or no `.rs` file refused (never a pass over zero files) |
| literals | advisory | warns, never fails: in `src/ui`, numeric colours and sizes (`rgb(0x..)`, `0xRRGGBB`, `px(..)`, `len(8.0)`) and string literals handed to text (`.child("..")`, `.label("..")`, `String::from("..")`, `"..".into()`, `.font_family("..")`) |

The script fails closed: if python3 or cargo is missing, `cargo metadata` fails, a file is unreadable or not UTF-8, there is a dangling symlink, or the helper crashes, it exits 2 with a message and never reports `ok`. The literals check is a pattern match over comment-stripped source and cannot see a literal first bound to a `let`, `const` or `static` and used later, a literal made by a macro or a helper, a number reaching a size through arithmetic on a non-token constant, or text assembled at run time; review still reads the diff. Both scripts belong in the pre-commit list of AGENTS.md section 3 for a change to the crates. The `Desktop` workflow (`.github/workflows/desktop.yml`) runs them, but it is separate from `ci.yml` and a required check only if the owner adds it to the branch protection of `develop`; if so, require only `Desktop model and guardrails`. Its guardrail job always runs; the Linux window build runs when a desktop crate, the root manifest, the lockfile, the desktop scripts or the workflow change; the macOS and Windows window builds run on every push to `develop` or `main`, on manual runs, and on changes to a desktop crate or the workflow (a PR that changes only the lockfile runs the Linux build, and the other two after the merge).
