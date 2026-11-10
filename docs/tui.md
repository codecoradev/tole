# Terminal UI (`tole-tui`)

Status: **MVP + pickers + steering** (#460, #461, #462, in `develop`, not yet released). Bare
`tole` on an interactive terminal opens an inline UI over one ordinary tole
session. Next: #464 (detach key).

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
  `  ✓ …`). A multi-line tool result shows its first line plus a
  `[+n lines · Ctrl+O]` hint. Answers get minimal markdown styling: `#`
  headings, `**bold**`, `` `inline code` ``, and fenced code as indented dim
  lines (the fence lines themselves are dropped). `+` / `-` / `@@` rows of an
  expanded tool result are green / red / dim (the tool's own diff text is
  styled; tole does not diff files itself).
- **Queue rows:** follow-ups waiting behind the running turn show as
  `queued: …` rows above the editor (three, then `+n more queued`).
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
| `Enter` | send the message; **while a turn runs: steer it** (see below) |
| `Alt+Enter` | **while a turn runs: queue the draft as a follow-up**; idle: newline |
| `Alt+Up` | pop the last queued follow-up back into the editor |
| `Shift+Enter`, `Ctrl+J` | newline in the editor (`Alt+Enter` too when idle) |
| `Ctrl+O` | expand / collapse tool output (last block now, new blocks from then on) |
| `Ctrl+T` | show / hide reasoning for output that has not been printed yet |
| `@` | file panel: type to filter, `Up`/`Down` select, `Tab` / `Enter` insert the path |
| `Up` / `Down` | input history (the draft is restored after the newest entry) |
| `Backspace` | delete the last character |
| `/` | open the command menu; `Tab` completes, `Up`/`Down` select |
| `Ctrl+L` | model picker (`/models`); type to filter, `Enter` picks, `Esc` closes |
| `Ctrl+P` | cycle the models used in this run |
| `Shift+Tab` | cycle thinking: `minimal → low → medium → high → unset` |
| `y` / `n` | answer an approval prompt (other keys are ignored) |
| `Esc` | cancel the running turn **and return the queue to the editor**; at an approval prompt: deny and cancel |
| `Ctrl+C` | cancel the running turn (queue returned); when idle, twice to quit |

Model/thinking hotkeys and slash commands wait while a turn runs (they take
the session lock). `Ctrl+O`, `Ctrl+T`, `@`, steering and queueing do not.

## Steering and follow-ups

- **Enter while a turn runs = soft interrupt (#377).** The draft is pushed on
  the turn's `CancelToken::interrupts()` queue (non-urgent). The turn commits
  it as an attributed user message `[owner interrupt …]` at its next safe
  point (after the current tool settles, before the next provider call); a
  final answer that was in flight is withdrawn and the model continues with
  the steering text. Nothing is cancelled and no work is lost. The text
  shows in the transcript as `> text  [steer]`. A steer the turn never
  reached (it settled first) runs next as a follow-up, or returns to the
  editor if the turn was cancelled. Blank drafts and `/` commands are
  ignored while a turn runs. Steering is not urgent: a tool call the model
  already asked for still runs.
- **Alt+Enter = follow-up queue.** Queued drafts wait (max 16); when the turn
  ends with an answer, the head becomes the next prompt (one at a time, the
  rest stay queued). If a turn stops without an answer (provider error,
  denial) or is cancelled, the queue goes back into the editor (joined by
  newlines) instead of running.
- Terminals that cannot report `Alt+Enter` / `Alt+Up` distinctly send them as
  `Esc` + key; on those the sequence is read as Alt only when it arrives as one
  burst.

## `@` file search

Typing a word that starts with `@` opens a panel of files under the working
directory (up to 50 shown, filtered as you type, case-insensitive substring).
The listing is made once per run: depth ≤ 3, at most 2000 files, hidden
entries, `target/`, `node_modules/` and names with control characters
skipped. `Tab` / `Enter` replace the `@word` with the path; with no match,
`Enter` sends the text as typed.

## Scrollback is append-only

Finished lines live in the terminal's own scrollback, so `Ctrl+O` / `Ctrl+T`
cannot rewrite what is already printed: `Ctrl+O` prints the rows the last tool
block left out and changes how later blocks are shown; `Ctrl+T` off drops
reasoning that streams afterwards (it is not buffered). Both are UI state only;
nothing is stored. The tool preview itself is capped at 200 characters by
tole-core, so a long diff is cut there.

## Commands

| Command | Effect |
|---|---|
| `/model [id]` | picker over `/models`, or pin `id` (`fact/model`) |
| `/thinking [level]` | set `minimal\|low\|medium\|high\|unset`; no argument cycles (`fact/reasoning_effort`) |
| `/compact [instructions]` | the next prompt compacts first (the forced pass of #481); **not** a mid-turn compaction; instructions are not supported yet and reported as ignored |
| `/new` | fresh session in place (the old handle is closed) |
| `/resume` | picker over the global session index (newest 50, current excluded) |
| `/session` | the `tole status` lines |
| `/name <text>` | rename (`fact/session_name`; shown by `sessions --all`, `resume <name>`) |
| `/quit` | quit |

An unknown command is refused with a prefix hint and nothing is sent. Model,
effort and name are session registers: the next provider call uses them and
they survive `tole resume`. `unset` clears the override, so the footer then
shows the process default (`--effort` / env / config) if one is set, and
`Shift+Tab` continues from that value. `/new` and `/resume` swap the session in
place (one open handle per session) but do not replay the old transcript on
screen; the todo list tool keeps the launch session's state.

**Shift+Enter limitation:** most terminals send the same byte for `Enter` and
`Shift+Enter`. tole asks for the kitty keyboard protocol (kitty, WezTerm,
foot, Ghostty and recent iTerm2/Alacritty report it); on terminals that do
not, `Shift+Enter` submits like `Enter`. `Ctrl+J` and `Alt+Enter` work
everywhere.

## Not implemented

The editor has no in-buffer cursor movement (Left/Right/Home/End, `Up`/`Down`
always browse history); no memory loop, MCP prompt commands or sibling
notices; no `/resume` paging and no transcript replay after `/new` /
`/resume`. Markdown is inline styling only: no tables, nested lists, links,
italics; a `**` or `` ` `` pair split by line wrapping is not styled.
`@` inserts a path at the end of the draft (there is no in-buffer cursor).
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
  A pending approval is source-agnostic UI state (#526): an `ApprovalRequest`
  (one sanitized line plus the `Risk` tier) and a one-shot reply. An in-process
  turn blocks on `Prompter::ask(request)`; a remote source (a server's
  `/approvals` queue) calls `Prompter::post(request, reply)` without blocking and
  `reply` is where the verdict goes (for example the decision `POST`). The verdict
  is delivered exactly once, and a request that is dropped unanswered (turn
  ended, a newer request, the UI gone) is **denied**. The risk tier is carried so
  the rule that a `Destructive` call is never "allowed always"
  (`ApprovalRequest::can_allow_always`) lives in one place for every UI.
- **Host seam (#526):** `TuiHost` has three methods: `footer`, `run_turn` and
  `apply(HostCmd) -> Result<HostReply, String>`, which covers every slash command
  and hotkey (`/model`, `/effort`, `/compact`, `/new`, `/resume`, `/session`,
  `/name`, the pickers). The reply is data only (`Done`, `Session(id)`,
  `Models`, `Sessions`, `Lines`); the wording shown to the user and the pickers
  are the UI's, so a remote adapter writes three methods and no UI text. The default
  `apply` supports `Status` (no lines) and refuses the rest with "not supported by
  this host"; a reply of the wrong kind is reported, never a panic.
- **Input seam (#526):** the app logic does not see terminal key types.
  `App::on_input(Input)` takes a UI-neutral `Input` (`Key` plus ctrl/alt/shift);
  the crossterm adapter (`term_input`: `input_from`, and `App::on_key` as the
  terminal entry point) maps key events onto it and drops key releases. Every
  key the logic does not distinguish is `Key::Other`: it still counts as a key
  press (it disarms a pending `Ctrl+C`) but triggers nothing. The key table above
  is the behavior contract for both entry points, so a second front end (a GPUI
  desktop, a remote client) feeds the same state machine with `Input`s.
- Terminal state (raw mode, keyboard enhancement) is restored by a drop
  guard, so it is also restored on panic. SIGINT is just a `Ctrl+C` key event
  in raw mode.
- `cargo build -p tole-cli --no-default-features` builds without the TUI.
- Tests: unit tests for the editor, transcript, footer and approval state
  machines; `crates/tole-cli/tests/tui_e2e.rs` drives the real binary through
  a pty against a scripted mock provider.
