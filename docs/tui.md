# Terminal UI (`tole-tui`)

Status: **MVP** (#460, in `develop`, not yet released). Bare `tole` on an
interactive terminal opens an inline UI over one ordinary tole session. Next:
#461 (slash commands, pickers), #464 (`/thinking`, detach key).

- Bare `tole` (no arguments, stdin and stdout are TTYs) runs the hidden
  `tole tui` subcommand, so it goes through the same config gate, `--allow` /
  `--yes` handling and settings as `tole chat`. On a non-TTY it prints help.
- Rendering is a ratatui **inline viewport**, not the alternate screen:
  finished lines are committed to the terminal scrollback, the viewport only
  holds the live stream line, the editor and the footer.
- Sessions are ordinary tole sessions (same JSONL as `chat`): `tole resume`,
  `tole sessions --all` and `tole status` see them. `tole tui --resume <id>`
  reopens one.

## Screen

- **Transcript:** answer text streams in as it arrives; reasoning is dimmed;
  tool calls and results are one compact line each (`▸ write_file out.txt`,
  `  ✓ …`).
- **Footer:** `folder · session · model · effort · context used / window (%) ·
  tokens`. Context and tokens come from the `tole status` snapshot, refreshed
  after every turn.
- **Approvals:** a Write or Destructive call that is not pre-authorized shows
  the tool and target in the viewport and waits for `y` / `n`. The prompt is
  read through the event loop, so rendering stays live. Allowlist, `--yes` and
  the Destructive-always-asks rule are unchanged (they live in the approver).

## Keys

| Key | Action |
|---|---|
| `Enter` | send the message |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | newline in the editor |
| `Up` / `Down` | input history (the draft is restored after the newest entry) |
| `Backspace` | delete the last character |
| `y` / `n` | answer an approval prompt (other keys are ignored) |
| `Esc` | cancel the running turn; at an approval prompt: deny and cancel |
| `Ctrl+C` | cancel the running turn; when idle, twice to quit |
| `/quit` + `Enter` | quit |

**Shift+Enter limitation:** most terminals send the same byte for `Enter` and
`Shift+Enter`. tole asks for the kitty keyboard protocol (kitty, WezTerm,
foot, Ghostty and recent iTerm2/Alacritty report it); on terminals that do
not, `Shift+Enter` submits like `Enter`. `Ctrl+J` and `Alt+Enter` work
everywhere.

## Not in the MVP

The editor has no in-buffer cursor movement (Left/Right/Home/End, `Up`/`Down`
always browse history); no slash commands beyond `/quit`; no memory loop, MCP
prompt commands or sibling notices; no soft-interrupt steering (#377/#383).
Wide characters wrap by character count, not cell width.

## Architecture

- `tole-tui` depends on `tole-core` only (no crate cycle). The host
  (`tole-cli`, behind the default `tui` feature) implements `TuiHost`: it
  owns storage, registry and provider config, runs one turn on a worker
  thread (`run_turn` + `TurnObserver` streams text, reasoning and tool events
  to the UI), and supplies the footer.
- **Footer decision:** the host composes the footer (`TuiHost::footer`) from
  `status::session_status`. The alternative, moving the snapshot into
  `tole-core`, was rejected because `SessionStatus` is built from
  `CompactionSettings` / `WindowSource` / `ModelsCatalog`, which are
  provider- and network-adjacent host types.
- Approvals: `tole_tui::approval_channel()` gives the host a `Prompter`; the
  CLI wraps it as a `PromptFn` inside the regular `InteractiveApprover`.
- Terminal state (raw mode, keyboard enhancement) is restored by a drop
  guard, so it is also restored on panic. SIGINT is just a `Ctrl+C` key event
  in raw mode.
- `cargo build -p tole-cli --no-default-features` builds without the TUI.
- Tests: unit tests for the editor, transcript, footer and approval state
  machines; `crates/tole-cli/tests/tui_e2e.rs` drives the real binary through
  a pty against a scripted mock provider.
