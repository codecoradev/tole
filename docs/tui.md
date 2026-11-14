# Terminal UI (`tole-tui`)

Status: **MVP + pickers + steering + paste/images + export/copy + fork/tree + themes + keybinding config** (#460, #461, #462, #560, #463 parts 1-4, in `develop`, not yet released). Bare
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
- **Image row:** images attached to the next message (#560) show as one
  `image: shot.png (image/png, 12 KiB) · next message` row above the editor
  (`images (n): …` for several); it shares the three rows with the queue.
- **Footer:** `folder · session · model · effort · context used / window (%) ·
  tokens`. Context and tokens come from the `tole status` snapshot, refreshed
  after every turn.
- **Approvals:** a Write or Destructive call that is not pre-authorized shows
  the tool and target in the viewport and waits for `y` / `n`. The prompt is
  read through the event loop, so rendering stays live. Allowlist, `--yes` and
  the Destructive-always-asks rule are unchanged (they live in the approver).

## Keys

Keys marked *(action)* can be rebound with `[tui.keys]` (see
[Keybindings](#keybindings-463)); the others are fixed.

| Key | Action |
|---|---|
| `Enter` | send the message; **while a turn runs: steer it** (see below) |
| `Alt+Enter` | *(`queue_follow_up`)* **while a turn runs: queue the draft as a follow-up**; idle: newline |
| `Alt+Up` | *(`pop_queue`)* pop the last queued follow-up back into the editor |
| `Shift+Enter`, `Ctrl+J` | *(`newline`)* newline in the editor (`Alt+Enter` too when idle) |
| `Ctrl+O` | *(`toggle_tools`)* expand / collapse tool output (last block now, new blocks from then on) |
| `Ctrl+T` | *(`toggle_reasoning`)* show / hide reasoning for output that has not been printed yet |
| `@` | file panel: type to filter, `Up`/`Down` select, `Tab` / `Enter` insert the path |
| `Up` / `Down` | *(`history_prev` / `history_next`)* input history (the draft is restored after the newest entry) |
| `Backspace` | delete the last character |
| paste | the whole block is inserted as one edit, never sent: newlines stay newlines, `Enter` sends (see Paste) |
| `/` | open the command menu; `Tab` completes, `Up`/`Down` select |
| `Ctrl+L` | *(`model_picker`)* model picker (`/models`); type to filter, `Enter` picks, `Esc` closes |
| `Ctrl+P` | *(`cycle_model`)* cycle the models used in this run |
| `Shift+Tab` | *(`cycle_thinking`)* cycle thinking: `minimal → low → medium → high → unset` |
| `y` / `n` | answer an approval prompt: the plain keys only (`Alt+Y`, `Ctrl+N` and every other key are ignored) |
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

## Paste and images (#560)

- **Bracketed paste.** The TUI turns the terminal's bracketed-paste mode on
  at start and off again on exit (also when a panic unwinds: same drop guard
  as raw mode). A pasted block then arrives as one event and is inserted into
  the editor as one edit: CRLF and lone CR become LF, control characters other
  than newline and tab are dropped, and nothing is submitted until `Enter`
  (while a turn runs, `Enter` steers with it as usual). A paste never answers
  an approval prompt (it is ignored there) and only filters an open picker.
  Before #560 a multi-line paste arrived as keys: its first line was sent at
  the first line break and the next line steered that turn. Terminals without
  bracketed paste still behave that way.
- **`/image <path|data-url>`** queues an image for the next message, like
  `chat`'s `/image` (#559): same workspace jail, 10 MiB cap and type sniffing
  as `--image`. `/image` lists the queue, `/image clear` empties it. A refused
  image shows the host's error (`(/image: image: … outside the workspace jail
  — refused)`). Like other commands, `/image` is ignored while a turn runs (the
  host holds the session for the turn).
- **Pasting an image.** A paste that is exactly ONE image reference is
  attached instead of inserted, when no turn is running: a `data:image/…` URL,
  or a single path ending in `.png .jpg .jpeg .gif .webp` (any case), possibly
  wrapped in quotes or with `\ `-escaped spaces (what terminals send for a
  dropped file). The host decides: a file that does not exist, lies outside
  the jail or is not an image is inserted as text after all, with a note
  saying why. Anything else (several words or lines) is text.
- **Non-vision models:** the image rides on the message and the provider
  refuses it like `--image` does: with `TOLE_VISION=0` (or a server that
  rejects image input) the turn stops with the `model-capability error: …`
  note. The message is recorded with its image, so later messages in that
  session hit the same error (as in `chat`); switch to a vision model.
- **No clipboard images (decision).** The TUI does not read images from the OS
  clipboard: a terminal only delivers text, and reading the clipboard would
  need a native, platform-specific dependency. Graphical clients (#527–#529)
  own clipboard images. In a terminal, paste or drop the file's path, or use
  `/image`.
- No `@`-panel entry for images: the `@` panel inserts a path into the draft
  as text, and turning a selection into an attach would give one key two
  meanings. Use `/image <path>`.

## `@` file search

Typing a word that starts with `@` opens a panel of files under the working
directory (up to 50 shown, filtered as you type, case-insensitive substring).
The listing is made once per run: depth ≤ 3, at most 2000 files, hidden
entries, `target/`, `node_modules/` and names with control characters
skipped. `Tab` / `Enter` replace the `@word` with the path; with no match,
`Enter` sends the text as typed.

## `/copy` and `/export` (#463)

- **`/copy`** sends the text of the session's LAST ASSISTANT ANSWER (the newest
  assistant message in the log, read by the host) to the terminal clipboard
  with an OSC 52 sequence, `ESC ] 52 ; c ; <base64> BEL`, written by the TUI
  process itself. There is no OS clipboard dependency (decision of #560). The
  payload is only base64 of the answer, so answer text can never put an escape
  byte on the terminal; the sequence is built and written in one place
  (`clipboard.rs`) and provider text still reaches the screen only through the
  sanitizer. A terminal never confirms OSC 52, so the note says what was
  attempted (`sent N characters to the terminal clipboard; needs OSC 52
  support, tmux needs set -g set-clipboard on`) and never that it worked.
  Terminals differ: some need OSC 52 write enabled in their settings, and over
  ssh or in tmux it works only if each hop passes it. An answer over **64 KiB**
  is refused (terminals and multiplexers cap the payload, and a truncated copy
  is worse than none) with a pointer to `/export`; with no answer yet, `/copy`
  says so and sends nothing.
- **`/export [path]`** writes the transcript of the open session. The format
  follows the extension: `.html` is a self-contained static page (every text
  escaped, no script, no external resource, a
  `default-src 'none'; style-src 'unsafe-inline'` CSP meta, light and dark);
  `.jsonl` is the session's entries exactly as stored, one JSON object per
  line. Any other extension is refused with the supported list. Reasoning is
  not part of the log, so it is not exported. With no path the file is
  `<workspace>/.tole/exports/<session id>.html` (only that directory is
  created). The path, absolute or relative to the workspace, must resolve
  inside the workspace jail (`..`, outside paths and symlinks that lead out
  are refused, like `/image` and the file tools); its directory must exist. An
  existing file, or a symlink at the target, is **never overwritten or
  followed**: pick another path. The session file is only read, never changed.
- **Secrets:** a transcript contains whatever tools printed (file contents,
  command output, environment dumps), including secrets. The export is
  created owner-only (`0600`) on Unix, but treat it like the session log:
  review it before sharing, and keep `.tole/exports/` out of version control.
- **While a turn runs:** like `/image` and the other commands, both are
  ignored (the UI only runs commands when idle), which keeps one rule for
  every command; `/export` could read the log safely during a turn, but it
  would be the only command that did.

## Themes (#463)

Colors come from a theme: a table from semantic roles to a foreground color
plus bold / italic / underline / dim. Three are built in.

- `default` is the look tole always had (cyan tool lines, yellow notes and
  approval prompt, green / red diff rows, dim hints and footer).
- `light` avoids colors that vanish on a light background (no yellow, white or
  cyan text; tools and code in blue, notes and the approval prompt in a dark
  amber).
- `mono` uses no color at all, only bold, dim, italic and underline.

**Choosing one.** `TOLE_THEME=<name>` (env) beats `[tui] theme = "<name>"` in
the **user** config (`~/.codecora/tole/config.toml`), which beats `NO_COLOR`
(a set, non-empty `NO_COLOR` selects `mono`, see <https://no-color.org>), which
beats `default`. `tole config check` shows the effective theme and which of
these supplied it. Note that the terminal backend (crossterm) itself drops
color escapes whenever `NO_COLOR` is set, so an explicitly chosen colored
theme still prints without color until `NO_COLOR` is unset (bold, dim and
underline stay).

**Custom colors.** The user config may override single roles; a value is an
optional color and any of `bold italic underline dim`, in any order:

```toml
[tui]
theme = "light"          # optional; default | light | mono

[tui.colors]
tool = "blue"            # a color name
heading = "#5f00af bold" # #rrggbb
reasoning = "244 italic" # 0-255 indexed
```

Colors are `black red green yellow blue magenta cyan gray darkgray lightred
lightgreen lightyellow lightblue lightmagenta lightcyan white`, a number
`0`-`255`, or `#rrggbb`. Roles: `user text reasoning tool tool_output heading
code note diff_add diff_remove diff_hunk inline_code inline_bold queue menu
picker hint border footer approval`. An override replaces the role's whole
style. There is no theme file format; the table above is the whole surface.

**What a theme can never do.** It has no background colors, and `hidden`,
`reversed`, `blink` and the like do not exist in the vocabulary, so text cannot
be made invisible by a theme. The approval prompt (`approval`) and notes
(`note`, which also carry errors) are *critical*: never dim, and any custom
style for them is forced bold. A color is rejected only when it can never be
valid (unknown name, index above 255, malformed hex); choosing a color close to
your own background is your call.

**Bad values never stop the UI.** An unknown theme name or an invalid color
makes the TUI use `default` and print one note at start naming the key and the
problem (`theme: tui.colors: role `tool`: invalid color `x` ...; using the
default theme`). `tole config check` flags the same value as `INVALID`.

**Project config is not a source.** A `[tui]` table in `.tole/config.toml` is
ignored: a cloned repository must not control how the approval prompt looks.
tole prints one `warning: ignoring `tui` in project config ...` at startup and
`tole config check` marks the project's keys `IGNORED`.

**No `/theme` command.** A live switch could not restyle the append-only
scrollback (lines already printed keep their colors), so the theme is read once
at start; change the setting and restart.

## Keybindings (#463)

The ten convenience actions marked above can be rebound in the **user** config
(`~/.codecora/tole/config.toml`), next to `[tui] theme`:

```toml
[tui.keys]
toggle_tools = "alt+o"                 # one key
newline = ["shift+enter", "alt+n"]     # or several; a list replaces the defaults
history_prev = ["up", "ctrl+k"]        # keep the default Up and add Ctrl+K
```

Actions: `toggle_tools`, `toggle_reasoning`, `model_picker`, `cycle_model`,
`cycle_thinking`, `newline`, `queue_follow_up`, `pop_queue`, `history_prev`,
`history_next`. An action that is not listed keeps its default; listing it
replaces ALL of its default keys. Defaults: `ctrl+o`, `ctrl+t`, `ctrl+l`,
`ctrl+p`, `shift+tab`, `shift+enter` and `ctrl+j`, `alt+enter`, `alt+up`, `up`,
`down` (`queue_follow_up` is a newline when no turn is running, as `Alt+Enter`
always was).

**Key syntax**, case-insensitive: any of `ctrl+` `alt+` `shift+` in any order,
then a letter or digit, `enter`, `up`, `down`, `left`, `right`, `f1` to `f12`,
or `shift+tab` (the only form of Tab). `Shift+` goes with Enter and the
arrows; `shift+tab` is its own key; letters take `ctrl+` or `alt+`, never
`shift+` (a terminal reports Shift+O as a capital O).

**Never rebindable.** Typing (every bare character, so `/` and `@` too),
`y` / `n` at an approval prompt, `Esc`, `Ctrl+C`, `Enter` (send / steer),
`Backspace` and `Tab` (completion). A binding must use `ctrl+` or `alt+`, or be
a function key; bare `Enter`, bare arrows (except `up` for `history_prev` and
`down` for `history_next`, the defaults) and every protected key are refused.
Also refused, because a terminal cannot tell them apart from another key or
reserves them: `Ctrl+I` (is Tab), `Ctrl+M` (Enter), `Ctrl+H` (Backspace; some
terminals send it for Backspace), `Ctrl+[` (Esc), `Ctrl+S` / `Ctrl+Q` (flow
control), `Ctrl+Z` (suspend), `Ctrl+\`, and `Ctrl+digit` (a terminal sends
`Ctrl+4` to `Ctrl+7` as the codes of `Ctrl+\ ] ^ _`, the other digits as
nothing distinct). In raw mode crossterm 0.29 does deliver Ctrl+S, Ctrl+Q,
Ctrl+Z and Ctrl+\ as ordinary keys (flow control and signals are off), so
they would work, but a terminal, multiplexer or SSH client in between may eat
them, so they are refused anyway. `Ctrl+J` is allowed (it is the newline
fallback).

**Shielded at the approval prompt.** At an approval prompt the keymap is not
consulted at all: nothing but `y`, `n`, `Esc` and `Ctrl+C` does anything, so a
rebinding can neither intercept nor swallow them. The same holds in a picker.
As a second layer, the dispatcher itself never maps a bare character, Esc,
Tab, Backspace, bare Enter or Ctrl+C to an action, whatever the table says.

**Bad values never stop the UI.** An unknown action, a bad key, a protected or
ambiguous key, or one key on two actions makes the TUI use the whole default
keymap and print one note at start naming the key and the problem
(`keys: tui.keys.cycle_model: "y": a bare character ...; using the default
keys`; a clash names both actions). `tole config check` shows `tui.keys` with
the effective result (`effective: N action(s) rebound ... (user config)`) or
`INVALID: ...`, and in a build without the TUI `not validated: this build has
no TUI ...`.

**Project config is not a source**, and there is no environment variable: a
`[tui.keys]` table in `.tole/config.toml` is ignored (the same single
`warning: ignoring `tui` in project config ...` at startup, `IGNORED` in
`tole config check`), so a cloned repository cannot change what a key does.

**Hints follow the binding.** The footer hint (`Ctrl+L model`,
`Shift+Tab thinking`, `Shift+Enter newline`, `Alt+Enter queues`, `Alt+Up
unqueues`) and the `[+n lines · Ctrl+O]` marker show the first key of the
effective binding. The slash-menu descriptions name no key. Terminal limits
still apply: `Alt+` combinations arrive as `Esc` + key, some terminals and
window managers keep `F11` or `Alt+` shortcuts for themselves, and
`Shift+Enter` needs the kitty keyboard protocol (see below). An unbound
`Shift+Enter` or `Alt+Enter` never sends the message.

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
| `/fork [entry-id]` | fork this session at a settled entry (default: the last) into a new session and switch to it (see Forking) |
| `/tree` | picker over this session's family (parents and forks, indented, current marked); picking switches (see Forking) |
| `/session` | the `tole status` lines |
| `/name <text>` | rename (`fact/session_name`; shown by `sessions --all`, `resume <name>`) |
| `/image [path\|data-url\|clear]` | queue an image for the next message; no argument lists the queue, `clear` empties it (#560) |
| `/copy` | copy the last assistant answer to the terminal clipboard via OSC 52 (#463) |
| `/export [path]` | write the transcript to a file: `.html` or `.jsonl`; no path = `.tole/exports/<session id>.html` (#463) |
| `/quit` | quit |

An unknown command is refused with a prefix hint and nothing is sent. Model,
effort and name are session registers: the next provider call uses them and
they survive `tole resume`. `unset` clears the override, so the footer then
shows the process default (`--effort` / env / config) if one is set, and
`Shift+Tab` continues from that value. `/new`, `/resume`, `/fork` and picking
from `/tree` swap the session in
place (one open handle per session) but do not replay the old transcript on
screen; the todo list tool keeps the launch session's state.

## Forking (`/fork`, `/tree`, #463)

A UI over the session branching of #210 (`tole branch`, `tole sessions
--tree`); nothing new is stored beyond what `branch` writes.

- **`/fork [entry-id]`** cuts the open session at a *settled* entry (a final
  assistant message with no tool call hanging off it; default: the last one,
  as `tole branch` does) into a new session file in the sessions directory,
  and switches the TUI to it in place. The fork carries the history up to the
  cut, so the next message continues from there; it does not touch the
  original, which stays resumable (`tole resume <id>`). The new session is
  recorded in the session index (face `branch`), so `/resume` and `sessions
  --all` list it, and its header names the source and the entry
  (`forkedFrom`). An unknown entry, an unsettled one (a user message, a tool
  call) or a session with nothing settled yet is refused with the core's
  error (`(fork: forking s-1: entry … is not a settled point …)`), and the
  session stays as it was. Forking while the TUI holds the session's writer
  lock is safe: the source is replayed read-only, the new file takes its own
  lock.
- **`/tree`** opens a picker like `/resume` over the family of the open
  session: its parent chain up to the root and every fork below that root
  (siblings and grandchildren included), nested with `└─ ` by depth, each fork
  showing the entry it was cut at (`(@entry)`), the open session marked
  `(current)` and preselected. `Enter` on another row switches to it (as
  `/resume` does); on the current row it only says so. A session with no
  parent and no forks shows just itself, with a note. The data is the same as
  `sessions --tree` (the `forkedFrom` headers of the files in the sessions
  directory); sessions whose source file is gone are roots.
- While a turn runs both are ignored, like `/new` and `/resume`. There is no
  transcript replay after the switch (the new session's earlier turns are not
  redrawn), and in-file branching (several branches inside one file) is a
  separate, still-deferred follow-up.

**Shift+Enter limitation:** most terminals send the same byte for `Enter` and
`Shift+Enter`. tole asks for the kitty keyboard protocol (kitty, WezTerm,
foot, Ghostty and recent iTerm2/Alacritty report it); on terminals that do
not, `Shift+Enter` submits like `Enter`. `Ctrl+J` and `Alt+Enter` work
everywhere.

## Not implemented

`/export` writes no Markdown or PDF and does not redact secrets; `/copy` cannot confirm the terminal accepted the clipboard write.
The editor has no in-buffer cursor movement (Left/Right/Home/End, `Up`/`Down`
always browse history); no memory loop, MCP prompt commands or sibling
notices; no `/resume` paging and no transcript replay after `/new` /
`/resume` / `/fork` / `/tree`; no in-file branching. Markdown is inline styling only: no tables, nested lists, links,
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
- **Theme (#463):** `tole-tui` owns the `Theme` type (role -> `Style`, built-ins, color parsing and the safety rules) and reads no file or env; the host hands it over through the defaulted `TuiHost::theme` / `startup_note`. `tole-cli` resolves the name and overrides (`TOLE_THEME` > user config > `NO_COLOR` > `default`; the project layer is dropped in `config::layered`).
- **Keymap (#463):** `tole-tui` owns the `Keymap` type (closed `KeyAction` set, key-spec parsing, the binding rules and the clash check) and reads no file or env; the host hands it over through the defaulted `TuiHost::keymap` (note via `startup_note`, joined with the theme note). `tole-cli` reads `[tui.keys]` from the user layer only (`config::layered` drops the project table) and calls `Keymap::from_overrides`; an error is the default keymap plus one note. `App::on_key_press` consults the keymap only after Ctrl+C, the approval prompt and the picker.
- **Host seam (#526):** `TuiHost` has three methods: `footer`, `run_turn` and
  `apply(HostCmd) -> Result<HostReply, String>`, which covers every slash command
  and hotkey (`/model`, `/effort`, `/compact`, `/new`, `/resume`, `/session`,
  `/name`, the pickers, and #560's `AttachImage(spec)` / `ClearImages` /
  `Images`, and #463's `LastAnswer` / `Export(path)`). The reply is data only (`Done`, `Session(id)`,
  `Models`, `Sessions`, `Lines`, `Images(Vec<ImageItem>)`: the queue after the
  command, `Answer(Option<String>)`, `Path(String)`); the wording shown to the user and the pickers
  are the UI's, so a remote adapter writes three methods and no UI text. The default
  `apply` supports `Status` (no lines) and `Images` (none queued) and refuses the
  rest with "not supported by this host"; a reply of the wrong kind is reported, never a panic.
- **Input seam (#526):** the app logic does not see terminal key types.
  `App::on_input(Input)` takes a UI-neutral `Input`: `Input::Key(KeyPress)`
  (`Key` plus ctrl/alt/shift) or `Input::Paste(String)` (#560, one pasted
  block). The crossterm adapter (`term_input`: `input_from_event`,
  `input_from`, and `App::on_key` as the key entry point) maps key events and
  `Event::Paste` onto it and drops key releases. A GUI front end feeds its
  clipboard text as `Input::Paste`. Every
  key the logic does not distinguish is `Key::Other`: it still counts as a key
  press (it disarms a pending `Ctrl+C`) but triggers nothing. The key table above
  is the behavior contract for both entry points, so a second front end (a GPUI
  desktop, a remote client) feeds the same state machine with `Input`s.
- **Render split (#526):** drawing (`App::draw` and the markdown/diff styling helpers) lives in the private `render` module, which only reads `App`. State and input handling stay in `lib.rs`, so another front end can reuse the state machine without the ratatui drawing.
- **Image queue (#560):** the queue lives in the host (`TuiSession`'s #559
  pending images, taken by the next `run_turn`). The UI keeps the list the host
  last reported for the image row; it clears it when a message starts a turn
  and, after a turn that took images, asks the host again (`HostCmd::Images`),
  because a message that was never recorded hands its images back. Whether a
  paste "looks like" an image is decided by the UI (`paste::image_spec`); the
  file checks (exists, jail, size, type) are the host's.
- Terminal state (raw mode, keyboard enhancement, bracketed paste) is restored
  by a drop guard, so it is also restored on panic. SIGINT is just a `Ctrl+C` key event
  in raw mode.
- `cargo build -p tole-cli --no-default-features` builds without the TUI.
- Tests: unit tests for the editor, transcript, footer and approval state
  machines; `crates/tole-cli/tests/tui_e2e.rs` drives the real binary through
  a pty against a scripted mock provider.
