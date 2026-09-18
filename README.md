# tole

[![CI](https://github.com/codecoradev/tole/actions/workflows/ci.yml/badge.svg)](https://github.com/codecoradev/tole/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tole-core.svg)](https://crates.io/crates/tole-core)
[![crates.io](https://img.shields.io/crates/v/tole-cli.svg)](https://crates.io/crates/tole-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Durable Rust agent harness: a conversational agent with risk-tiered approval
gates, a write-once JSONL session log, and a register state machine — resumable
after crashes, replayable forever.

**Status:** v0.3.0 released — chat-first harness (chat / resume / sessions /
jobs / MCP client). Post-0.3.0 work landing on `develop`: the uteke memory
loop and the cora MCP auto-preset (see [CHANGELOG.md](CHANGELOG.md) →
Unreleased). Phase 3 hardening (E9) in progress in
[docs/epics.md](docs/epics.md).

## Ecosystem position

tole is the agent harness of the CodeCora ecosystem — the hands:

- **[cora-code](https://github.com/codecoradev/cora-code)** (`cora`) — code
  intelligence. Attached automatically when the `cora` binary is on PATH:
  tole detects the built-in `cora mcp` server and registers its 18-tool
  surface (brain search, callers, impact, affected tests, dead-code, review
  — registry names `mcp_cora_*`). Opt out with `--no-auto-mcp`.
- **[uteke](https://github.com/codecoradev/uteke)** (`uteke`) — semantic
  memory. `uteke_recall` (recall) and `uteke_document` (markdown → durable
  knowledge) register automatically when the `uteke` binary is on PATH, and
  the optional [memory loop](#memory-loop) closes the circle.
- Anything else that speaks **MCP over stdio** joins the same registry via
  repeatable `--mcp-server name=command [args...]` flags.
- **tole as an MCP server**: `tole mcp` serves the registry's hardened tools
  (jailed file ops, git, jobs, memory) to any MCP client. ReadOnly tools are
  always callable; Write tools require `--allow` patterns; Destructive tools
  are structurally absent.
- **tole as an ACP agent**: `tole acp` speaks the Agent Client Protocol —
  editors (Zed et al.) drive durable tole sessions, and tool approvals
  surface as permission requests in the editor.

Every ecosystem integration is probe-first: a missing binary degrades to a
one-line warning, never a phantom tool.

## Install

```bash
cargo install tole-cli        # the `tole` binary from crates.io
# or build from source:
git clone https://github.com/codecoradev/tole && cd tole
cargo build --release -p tole-cli && ./target/release/tole --help
```

## Quickstart

```bash
# Any OpenAI-compatible endpoint works; TOLE_* is canonical,
# OPENAI_* is read as a fallback.
export TOLE_BASE_URL=https://api.openai.com/v1
export TOLE_MODEL=gpt-4o-mini
export TOLE_API_KEY=...

tole run "summarize the last commit"        # one durable turn
tole chat                                   # multi-turn REPL on one durable session
tole chat --memory uteke                    # …with cross-session memory
tole chat --resume <id>                     # pick the thread back up (also: --last)
tole resume <id> "next instruction"         # continue a settled session
tole sessions                               # list durable sessions
tole status <id>                            # pc / seq / turns / token usage
```

Sessions live in `.tole/sessions/<id>.jsonl` (override with
`--sessions-dir`): append-only, crash-safe, replayable — kill the process
mid-turn and resume exactly where it stopped.

Useful flags (all subcommands): `--system` (persona, pinned in the session
header), `--workspace <dir>` (file-tools jail root), `--allow <glob>`
(repeatable pre-authorization for Write tools, e.g. `--allow 'write_*'`),
`--yes` (auto-allow every Write; Destructive still prompts), `--mcp-server`,
`--no-auto-mcp` (skip the cora auto-preset).

## Configuration (environment)

| Variable | Purpose |
|----------|---------|
| `TOLE_BASE_URL`, `TOLE_MODEL`, `TOLE_API_KEY` | LLM provider — any OpenAI-compatible endpoint (`OPENAI_*` equivalents read as fallback) |
| `TOLE_SYSTEM_PROMPT` | default persona when `--system` is absent |
| `TOLE_MEMORY` | memory loop backend (`uteke`) — same as `--memory uteke` |
| `TOLE_MEMORY_NAMESPACE` | override the loop's namespace (default: `repo-<directory name>`) |

## Tools

| Tool | Risk | Notes |
|------|------|-------|
| `read_file`, `write_file`, `edit_file` | RO / Write | jailed to `--workspace` (TOCTOU-safe, symlink-refusing) |
| `delete_file` | Destructive | always prompts; never allowlistable, even with `--yes` |
| `git` | Write | `status` / `diff` / `add` / `commit` only — **push stays human** |
| `gh` | Write | read-only ops, argv-validated per op; `--repo` is fixed at registration (currently `codecoradev/tole`) — per-repo wiring is a known gap |
| `run_command` | Write | argv-split (no shell), cwd-jailed, timeout + output cap |
| `job_start`, `job_poll` | Write / RO | detached long-running jobs with log tailing |
| `cora_search` | RO | hybrid codebase search via `cora brain`; native fallback — skipped when the cora MCP surface is attached |
| `uteke_recall`, `uteke_document` | RO / Write | semantic memory recall / markdown → room |
| `mcp_*` (from `--mcp-server`) | Write | server metadata is **never** trusted for risk; approval gate always applies |

## Approval gates

Every non-ReadOnly call goes through an `Approver`. `Destructive` tools are
denied structurally: the registry refuses them without an interactive
approver, allowlists deny them on sight, and `--yes` never covers them. There
is no bypass path. Secret redaction is wire-only (the durable local log keeps
the original text by design).

## Memory loop

`--memory uteke` (or `TOLE_MEMORY=uteke`) turns the harness into a
cross-session memory loop — host-initiated on both ends, the model can
neither trigger nor suppress it:

- **Before the first turn** of a fresh session, memories relevant to the
  prompt are recalled from the owner's uteke store and injected into the
  message inside a clearly marked fenced block; the durable log records
  exactly what the provider saw.
- **When the session settles** with a final answer, a compact summary is
  stored back — the next session's recall can find it.

The namespace follows the ecosystem `repo-<dir>` convention
(`TOLE_MEMORY_NAMESPACE` overrides). Every memory failure degrades to a
stderr note; the turn proceeds without memory.

## Workspace layout

- `crates/tole-core` — platform-agnostic library: state machine, JSONL
  storage, `Tool`/`Approver`/`Storage`/`Provider` traits, tools, and the
  OpenAI-compatible provider (crates.io; embedders build with
  `--no-default-features` — CI keeps the mobile targets compile-checked).
- `crates/tole-cli` — headless CLI host (the `tole` binary).
- `evals/` — agentic eval harness: Tier 1 CI contracts, Tier 2 live
  missions, Tier 3 release baselines (see [`evals/README.md`](evals/README.md)).

## Documentation

- [Architecture](docs/architecture.md) · [Threat model](docs/threat-model.md)
  · [PRD](docs/prd.md) · [Epics & roadmap](docs/epics.md)
- [CONTRIBUTING.md](CONTRIBUTING.md) — workflow, issue/PR rules, merge gate,
  cora review gate, live-mission binary hygiene
- [AGENTS.md](AGENTS.md) — the same rules for AI coding agents
- [CHANGELOG.md](CHANGELOG.md) · [Security policy](SECURITY.md)
- Contributing requires the CLA:
  [CLA_INDIVIDUAL.md](CLA_INDIVIDUAL.md) /
  [CLA_CORPORATE.md](CLA_CORPORATE.md) (enforced per PR by `cla-check`;
  PR and issue templates live in [.github/](.github/)).

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cora review --staged      # ecosystem-standard review gate before every commit
```

CI enforces the same set across Linux, plus cross-compile checks for
`aarch64-apple-ios` and `aarch64-linux-android`.

## Design sources

Research notes and design sources: adapted from pi's harness spec (MIT).

## License

MIT.
