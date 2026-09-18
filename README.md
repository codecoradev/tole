# tole

Durable Rust agent harness: a conversational agent with risk-tiered approval
gates, a write-once JSONL session log, and a register state machine — resumable
after crashes, replayable forever.

**Status:** v0.3.0 — chat-first harness complete (chat / resume / sessions /
jobs / MCP client). Phase 3 hardening (E9) in progress. See
[CHANGELOG.md](CHANGELOG.md) and [docs/epics.md](docs/epics.md).

## Ecosystem position

tole is the agent harness of the CodeCora ecosystem — the hands. Two sibling
tools plug in as first-class tools (auto-detected at startup; a missing binary
degrades to a warning, never a phantom tool):

- **cora-code** (`cora`) — code intelligence. `cora_search` recalls indexed
  symbols via hybrid search (FTS5 + vector + graph). The full surface
  (callers, impact, dead-code, review) is available by attaching the built-in
  server: `tole run --mcp-server cora="cora mcp" ...`
- **uteke** (`uteke`) — memory. `uteke_recall` (semantic recall, room-scoped)
  and `uteke_document` (markdown → durable knowledge) register automatically
  when the `uteke` binary is on PATH.
- Anything else that speaks **MCP over stdio** joins the same registry via
  repeatable `--mcp-server name=command [args...]` flags.

## Workspace

- `crates/tole-core` — platform-agnostic library: state machine, JSONL
  storage, `Tool`/`Approver`/`Storage`/`Provider` traits, tools, OpenAI-compatible
  provider (published to crates.io).
- `crates/tole-cli` — headless CLI host (the `tole` binary; not on crates.io —
  build from source).

## Quickstart

```bash
cargo build --release -p tole-cli

# Any OpenAI-compatible endpoint works.
export TOLE_BASE_URL=https://api.openai.com/v1
export TOLE_MODEL=gpt-4o-mini
export TOLE_API_KEY=...

./target/release/tole run "summarize the last commit"
./target/release/tole chat                 # multi-turn REPL on one durable session
./target/release/tole chat --resume <id>   # pick the thread back up (also: --last)
./target/release/tole resume <id> "next instruction"
./target/release/tole sessions             # list durable sessions
./target/release/tole status <id>          # pc / seq / turns / token usage
```

Useful flags (all subcommands): `--system` (persona, pinned in the session
header), `--workspace <dir>` (file-tools jail root), `--allow <glob>`
(repeatable pre-authorization for Write tools, e.g. `--allow 'write_*'`),
`--yes` (auto-allow every Write; Destructive still prompts), `--mcp-server`.

## Tools

| Tool | Risk | Notes |
|------|------|-------|
| `read_file`, `write_file`, `edit_file` | RO / Write | jailed to `--workspace` (TOCTOU-safe, symlink-refusing) |
| `delete_file` | Destructive | always prompts; never allowlistable, even with `--yes` |
| `git` | Write | `status` / `diff` / `add` / `commit` only — **push stays human** |
| `gh` | Write | read-only ops, argv-validated per op; `--repo` is fixed at registration (currently `codecoradev/tole`) — per-repo wiring is a known gap |
| `run_command` | Write | argv-split (no shell), cwd-jailed, timeout + output cap |
| `job_start`, `job_poll` | Write / RO | detached long-running jobs with log tailing |
| `cora_search` | RO | hybrid codebase search via `cora brain` |
| `uteke_recall`, `uteke_document` | RO / Write | semantic memory recall / markdown → room |
| `mcp_*` (from `--mcp-server`) | Write | server metadata is **never** trusted for risk; approval gate always applies |

## Approval gates

Every non-ReadOnly call goes through an `Approver`. `Destructive` tools are
denied structurally: the registry refuses them without an interactive
approver, allowlists deny them on sight, and `--yes` never covers them. There
is no bypass path. Secret redaction is wire-only (the durable local log keeps
the original text by design).

## Design sources

Research notes and design sources: adapted from pi's harness spec (MIT).
Architecture and threat model: [docs/architecture.md](docs/architecture.md),
[docs/threat-model.md](docs/threat-model.md).

## License

MIT.
