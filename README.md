# tole

[![CI](https://github.com/codecoradev/tole/actions/workflows/ci.yml/badge.svg)](https://github.com/codecoradev/tole/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tole-core.svg)](https://crates.io/crates/tole-core)
[![crates.io](https://img.shields.io/crates/v/tole-cli.svg)](https://crates.io/crates/tole-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Durable Rust agent harness: a conversational agent with risk-tiered approval
gates, a write-once JSONL session log, and a register state machine — resumable
after crashes, replayable forever.

**Status:** v0.6.0 released; the 0.7.0 train is release-candidate on
`develop`. Four faces on one durable core: the CLI (run / chat /
resume / sessions / jobs / **mission**), `tole mcp` (tool server),
`tole acp` (editor agent), and `tole serve` (REST + multi-session
MCP-over-HTTP daemon). Identity (owner-approved): a chat-first
personal assistant WITH a mission mode for autonomous work —
budgeted turn-chaining (`tole mission --max-steps/--max-minutes/
--max-tokens/--verify`), durable task-list tools (`todo_write`/
`todo_read`), cost reports, and remote approvals via the serve face
(the 0.9.0 mobile track consumes them from a phone). Also shipped on
this train: ACP intra-turn visibility + true text streaming, ACP
auto model picker, run ergonomics (`--prompt-file/--name/--timeout`),
read-only `web_fetch`/`web_search` (SSRF-guarded), and startup
update notification + `tole upgrade`. See
[CHANGELOG.md](CHANGELOG.md) and [docs/epics.md](docs/epics.md).

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
  surface as permission requests in the editor. Native pickers via ACP
  config options (#176): `TOLE_MODELS` (comma-separated ids) advertises a
  model switch that persists durably per session across resumes, and an
  approval selector (`ask` / `auto`, session-scoped) plus an
  `allow_always` option on Write permission requests let the human relax
  the gate from the editor without weakening the Destructive tier.
- **tole as an HTTP daemon**: `tole serve` (issue #96) exposes the session
  host over token-authenticated REST — remote clients create sessions, run
  turns, and poll status without SSH-ing into the box. Binds 127.0.0.1 by
  default. `POST /sessions/{id}/cancel` (#178, REST face) stops an
  in-flight turn — it settles `stopReason: "cancelled"`.
  `--transport mcp` (issue #137) serves **multi-session MCP over
  Streamable HTTP** instead: one authenticated MCP connection addresses N
  durable tole sessions (`tole_session_new/prompt/cancel/status/list` +
  the registry tools routed to each session's workspace jail).

Every ecosystem integration is probe-first: a missing binary degrades to a
one-line warning, never a phantom tool.

## Install

```bash
cargo install tole-cli        # the `tole` binary from crates.io
# or build from source:
git clone https://github.com/codecoradev/tole && cd tole
cargo build --release -p tole-cli && ./target/release/tole --help
# `mcp-http` (needed by `tole serve --transport mcp`) is a default
# feature since #168 — a plain build/install already includes it.
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
`--no-auto-mcp` (skip the cora auto-preset), `--trust <preset>` (one-word
trust: `internal` / `read_only`), `--skill <path>` (load a SKILL.md),
`--plan-mode` (read-only wire), `--on-turnend <cmd>` (final-message gate).

## Configuration (environment)

| Variable | Purpose |
|----------|---------|
| `TOLE_BASE_URL`, `TOLE_MODEL`, `TOLE_API_KEY` | LLM provider — any OpenAI-compatible endpoint (`OPENAI_*` equivalents read as fallback) |
| `TOLE_SYSTEM_PROMPT` | default persona when `--system` is absent |
| `TOLE_MEMORY` | memory loop backend (`uteke`) — same as `--memory uteke` |
| `TOLE_NO_UPDATE_CHECK` | `1` disables the startup update-check banner (issue #220) |
| `TOLE_MEMORY_NAMESPACE` | override the loop's namespace (default: `repo-<directory name>`) |

## Tools

| Tool | Risk | Notes |
|------|------|-------|
| `read_file`, `write_file`, `edit_file` | RO / Write | jailed to `--workspace` (TOCTOU-safe, symlink-refusing) |
| `delete_file` | Destructive | always prompts; never allowlistable, even with `--yes` |
| `git` | Write | `status` / `diff` / `add` / `commit` only — **push stays human** |
| `gh` | Write | read-only ops, argv-validated per op; `repo` defaults to the checkout's GitHub remote, optional per-call override (validated) |
| `run_command` | Write | argv-split (no shell), cwd-jailed, timeout + output cap |
| `verify_package` | RO | crates.io / npm registry check before any install — hallucinated names get NOT FOUND + candidates, edit-distance-1 candidates get a typo-squat warning |
| `load_skill` | RO | loads a discovered SKILL.md on demand (`--skill` pins one upfront; `--no-skills` disables) |
| `gitea` | Write | Gitea counterpart of `gh` over the instance REST API — registers when `origin` is a Gitea remote AND `TOLE_GITEA_TOKEN`/`GITEA_TOKEN` is set; same six ops |
| `agent_start`, `agent_poll` | Write / Write | depth-1 child agents: spawn durable child sessions (structurally no grandchildren), results via per-child uteke mailboxes (ephemeral by default); `agent_poll` is Write because a settled poll consumes (cleans) the mailbox, and `--trust internal` allowlists it by name; `--agents-worktree` gives each child its own git worktree |
| `systemone_decide` | RO | typed decisions (choice/score/noul + confidence) from a System One backend — active when `SYSTEMONE_API_KEY` is set; `SYSTEMONE_BASE_URL` picks the backend (hosted Jev default, self-hosted compatible) |
| `job_start`, `job_poll` | Write / RO | detached long-running jobs with log tailing |
| `cora_search` | RO | hybrid codebase search via `cora brain`; native fallback — skipped when the cora MCP surface is attached |
| `uteke_recall`, `uteke_document` | RO / Write | semantic memory recall / markdown → room |
| `mcp_*` (from `--mcp-server`) | Write | server metadata is **never** trusted for risk; approval gate always applies |
| `web_fetch`, `web_search` | RO | text-only internet (no JS/browser): fetch is direct HTTPS, size-capped, content-type allowlisted, HTML→text; search needs `TOLE_WEB_SEARCH_URL` (fleet backend, `{"results":[{title,url,snippet}]}`) — no backend, no tool; results are model content, never executed |
| `todo_write`, `todo_read` | Write / RO | durable mission task list (at most one `in_progress`); state lives in session entries — write results are the record, crash-resume restores the last settled list; covered by `--trust internal` |

## Remote approvals

```bash
tole serve --token $TOLE_SERVE_TOKEN          # on the box
tole approvals list --url http://box:7801 --token $TOKEN
tole approvals allow apr-... --url http://box:7801 --token $TOKEN
```

When a serve-face session hits a non-preauthorized Write (#200), the
pending decision becomes a queue entry and the turn settles resumably
(fail-closed). A remote operator lists and decides: **allow** stores a
one-shot approval and resumes the session (the replayed effect
re-consults the gate — exactly once), **deny** records the verdict.
Entries expire to denied (15 min) so nothing hangs silently; every
decision lands a durable audit register on the session. MCP/ACP parity
is a follow-up. This is also the surface uteke-mobile drives —
[docs/mobile-control.md](docs/mobile-control.md) is the mobile-control
guide (#202).

## Mission mode

```bash
tole mission "ship the feature" --max-steps 48 --max-minutes 15 \
  --verify "cargo test" --yes
```

Autonomous turn-chaining toward a goal (#199): the existing turn
machinery looped until the model declares `MISSION_COMPLETE` (and
`--verify` exits 0, when set) or a budget trips (`--max-steps`,
`--max-minutes`, `--max-tokens`). Every chained turn is a normal durable turn — crash
mid-mission resumes exactly where it stopped (`tole mission --resume
<id>`, or plain `tole resume`), cancel works unchanged, Destructive
tools stay un-auto-allowable, and a durable summary lands on the
session either way. Plans ride the `todo_write`/`todo_read` tools
(#198); budget exhaustion settles resumably, never a dead session.
Run ergonomics (#216): `run --prompt-file <path>` (`-` = stdin),
`--name <alias>` (stored in the session header — `tole sessions` shows
it and `resume` accepts the alias), and `--timeout <secs>` (wall-clock
cap; expiry cancels at a checkpoint — resumable, never dead).
Budget tiers (#201): conservative defaults (48 steps / 15 min / 200k
tokens) with headroom under `--trust internal` (96 / 30 / 500k) —
explicit flags always win; a durable cost report (turns, steps, tokens,
per-risk-tier tool calls) lands on the session and `tole status` renders it.

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
  · [PRD](docs/prd.md) · [Epics & roadmap](docs/epics.md) ·
  [Performance record](docs/perf.md)
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
