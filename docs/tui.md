# Terminal UI (`tole-tui`)

Status: **skeleton** (#459, in `develop`, not yet released). It renders a 3-row placeholder (header with `tole <version>`, hint, input line) and only handles quitting; typed text is echoed but never sent. Next: #460 (transcript / editor / footer MVP), then #461.

- Bare `tole` (no arguments) on an interactive terminal (stdin and stdout
  are TTYs) opens the TUI. On a non-TTY it prints help as before.
- Exit with `Ctrl+C` twice, or type `/quit` and press Enter.
- Rendering is a ratatui **inline viewport**, not the alternate screen: the
  terminal scrollback keeps the conversation after exit.
- Terminal state (raw mode) is restored by a drop guard, so it is also
  restored on panic. SIGINT is just a `Ctrl+C` key event in raw mode; no
  custom signal handler.
- `tole-tui` depends on `tole-core` only; `tole-cli` supplies the host
  (`TuiHost`) behind the default `tui` feature. `cargo build -p tole-cli
  --no-default-features` builds without it.
