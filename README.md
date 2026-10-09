# tole

[![CI](https://github.com/codecoradev/tole/actions/workflows/ci.yml/badge.svg)](https://github.com/codecoradev/tole/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tole-core.svg)](https://crates.io/crates/tole-core)
[![crates.io](https://img.shields.io/crates/v/tole-cli.svg)](https://crates.io/crates/tole-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Durable Rust agent harness: a conversational agent with risk-tiered approval
gates, a write-once JSONL session log, and a register state machine — resumable
after crashes, replayable forever.

**Status:** v0.8.0 released — it adds the per-project config `.tole/config.toml`
(trusted before it applies; see the Configuration section) on top of v0.7.x
(rescan-2 hardening, one tool-call authorization gate; see the CHANGELOG for
the behavior changes). Four faces on one durable core: the CLI (run / chat /
resume / sessions / jobs / **mission**), `tole mcp` (tool server),
`tole acp` (editor agent), and `tole serve` (REST + multi-session
MCP-over-HTTP daemon). Identity (owner-approved): a chat-first
personal assistant WITH a mission mode for autonomous work —
budgeted turn-chaining (`tole mission --max-steps/--max-minutes/
--max-tokens/--verify`), durable task-list tools (`todo_write`/
`todo_read`), cost reports, and remote approvals via the serve face
(the 0.9.0 mobile track consumes them from a phone). Shipped in
0.7.0: ACP intra-turn visibility + true text streaming, ACP
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
tole status <id>                            # pc / seq / turns / token + cache usage / request-size split
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

## Configuration (project file)

`<cwd>/.tole/config.toml` (no parent-directory walk) holds per-project defaults.
It is loaded by every command except `config`, `upgrade` and `approvals`; use
`--config <path>` to name another file (a path given on the command line is
trusted without a trust record) or `--no-config` to ignore any file entirely
(no discovery, no trust check, no output). When a config is loaded, one line
`tole: using config <path>` goes to stderr. **Precedence: flag > env > config >
default**, and a project without a config file behaves exactly as before.

**Keys.** Every key is optional; the schema is strict (an unknown key, a wrong
type or a secret-like key is an error with `file:line`; the file is capped at
64 KiB).

| Key | Mirrors | Effective value |
|-----|---------|-----------------|
| `model`, `base_url`, `system_prompt`, `memory` | the provider env, `--system`, `--memory` | flag > env > config |
| `sessions_dir`, `workspace` | `--sessions-dir`, `--workspace` | flag > config > default |
| `[mission]` `max_steps`, `max_minutes`, `max_tokens` | `mission --max-*` | flag > config > budget tier |
| `[mission]` `verify`, `verify_timeout` | `mission --verify`, `--verify-timeout` | flag > config > default (`300`) |
| `trust` (list of presets) | `--trust` | flag > `TOLE_TRUST` env > config |
| `allow` (list of globs) | the subcommand's own `--allow` | flag > config |
| `mcp_server` (list) | `--mcp-server` | flag > config |
| `on_pretool`, `on_posttool`, `on_turnend` (lists) | `--on-pretool` / `--on-posttool` / `--on-turnend` | flag > config |
| `skill` (list of paths) | `--skill` | flag > config (paths resolve against the cwd) |
| `plan_mode`, `no_auto_mcp`, `no_skills` (booleans) | `--plan-mode`, `--no-auto-mcp`, `--no-skills` | `flag \|\| config` |

The API key only ever comes from the environment (a secret-like key in the file
is an error). An empty env variable counts as unset.

- **Lists are replaced wholesale.** A higher layer replaces the whole list for
  that key; lists are never merged. A non-empty `--allow` list replaces the
  config `allow` list entirely (an empty flag list means "not given"), exactly
  like `--trust` over `TOLE_TRUST`. Every subcommand has its own `--allow`; the
  config `allow` is the fallback when THAT subcommand's `--allow` is empty.
- **Booleans can only be turned on.** `effective = flag || config`: a flag can
  turn a setting on but there is no flag that forces it off, so a config `true`
  is dropped only with `--no-config` (or by editing / untrusting the file).
  `tole config check` says so next to each boolean.
- **Same effect as the flag.** `plan_mode = true` removes the write/delete/run
  tools from the wire like `--plan-mode`; `allow`/`trust` feed the same
  allowlist as the flags, so `Destructive` tools still always prompt (they can
  never be allowed through the file, and on the non-interactive faces they stay
  unregistered).
- **Faces that refuse a flag refuse it from the config too.** `serve`, `acp`,
  `mcp` and `mission` refuse `--on-pretool`/`--on-posttool` (and `--on-turnend`
  where the flag is refused), `--skill`, `--no-skills`, `--mcp-server` (and
  `--plan-mode` on `mission`). A value that arrives from the config is refused
  with the same message plus `use --no-config to ignore the project config`;
  tole never silently drops a project's safety hook.
- **Feature-less builds.** In a build without the `mcp` feature, `mcp_server` /
  `no_auto_mcp` in the config is an error naming the key and the feature; without
  `shell-tools` the same holds for the hook keys and `memory`.

`tole config check` prints every key that is set with its effective value and
where it came from (`flag`, `env VAR`, `config`, `default`), using the same
resolution code as startup (lists show the effective list, booleans
`effective: true (config)` or `(flag)`).

The file is untrusted input (it lives in a cloned repo), so it needs a
content-bound approval before it takes effect:

- On an interactive command (`run`, `chat`, `resume`, `mission`) with a terminal
  on both stdin and stderr, an untrusted or changed config is shown in full and
  you are asked `trust this config? [y/N]`. Everywhere else (`sessions`,
  `status`, `serve`, `acp`, `mcp`, no terminal, or `--prompt-file -`) tole
  FAILS CLOSED with the exact instruction (`tole config trust`) and runs
  nothing; `serve`/`acp`/`mcp` never ask because their stdin/stdout are
  protocol channels.

- `tole config trust [--config <path>] [--yes]` validates the file, prints its
  path and FULL content (plus a line diff against the previously trusted
  version) with control and bidi characters escaped, then asks `[y/N]`.
  Without a terminal it refuses unless `--yes` is given (the content is still
  printed).
- The approval is stored outside the repo in
  `$CODECORA_HOME/tole/trusted-configs.json` (default `~/.codecora`, mode 0600)
  as a snapshot of the file keyed by canonical project directory. A config is
  trusted only while its bytes are identical to that snapshot; any edit makes it
  untrusted again.
- `tole config untrust [--config <path>]` removes the record (no error if
  absent). `tole config check` prints `trust: trusted`, `trust: NOT trusted` or
  `trust: CHANGED since it was trusted`; its exit code stays 0 for a valid file
  whatever the trust state.
- A corrupt or unsupported-version store is an error naming the file; tole never
  overwrites it.

The schema is strict (unknown keys, wrong types, malformed TOML, unknown `trust`
presets and files over 64 KiB are errors, reported as `path:line:col: message`).
Flat snake_case keys mirror the global flags (`model`, `base_url`, `sessions_dir`,
`workspace`, `plan_mode`, `memory`, `trust`, `allow`, `mcp_server`, `no_auto_mcp`,
`on_pretool`, `on_posttool`, `on_turnend`, `skill`, `no_skills`, `system_prompt`)
plus one `[mission]` table (`max_steps`, `max_minutes`, `max_tokens`, `verify`,
`verify_timeout`). Secrets never belong in a project file: any key whose name
contains `secret`, `password`, `token` or `api_key` (any depth, any case) is a
hard error.

## Tools

| Tool | Risk | Notes |
|------|------|-------|
| `read_file`, `write_file`, `edit_file` | RO / Write | jailed to `--workspace` (TOCTOU-safe, symlink-refusing); `read_file` returns at most 20,000 chars per call (`offset`/`limit` in chars, `next_offset` to continue) |
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
per-risk-tier tool calls, cached tokens) lands on the session and `tole status` renders it.
`tole status` (and `GET /sessions/{id}/status`, as `usage_report`) also shows
the usage derived from the ledger for any session: prompt / completion /
reasoning / cached tokens, the cache-hit rate, and the request size in
characters split into system+tools vs history at the first and last step
(#211; recorded per step under `tole_wire`; sessions recorded before it read
`n/a`). Reported only — `--max-tokens` accounting is unchanged.

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
