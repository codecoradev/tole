# tole

[![CI](https://github.com/codecoradev/tole/actions/workflows/ci.yml/badge.svg)](https://github.com/codecoradev/tole/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tole-core.svg)](https://crates.io/crates/tole-core)
[![crates.io](https://img.shields.io/crates/v/tole-cli.svg)](https://crates.io/crates/tole-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Durable Rust agent harness: a conversational agent with risk-tiered approval
gates, a write-once JSONL session log, and a register state machine — resumable
after crashes, replayable forever.

**Status:** v0.9.0 is the latest release: `tole doctor`, soft interrupts, per-turn token stats, sibling change notices and the typed child-agent handoff, a provider failover chain, the user config layer, `--trust workspace`, command-level allow/deny patterns, a global session index (`sessions --all`) and context compaction, plus the 0.8.2 `tole serve` hardening (see the CHANGELOG). `develop` is working toward 0.10.0, a pi-style terminal UI: the `tole-tui` is in and daily-usable — bare `tole` on a TTY opens an inline UI with a streaming transcript, a multi-line editor, a footer, inline approvals and Esc-cancel; slash commands (`/compact` `/new` `/resume` `/session` `/name`, and `/fork` `/tree` over session forks, #463), the model picker and thinking-level cycling (#461); steering, a follow-up queue, `@file`, collapsible tool output and markdown/diff styling (#462), `/export` and `/copy` (#463), color themes (`default`, `light`, `mono`, `NO_COLOR`; #463), a keybinding config (`[tui.keys]` in the user config; #463), `--effort` reasoning effort and the `tole status [id]` snapshot (model, effort, context used/window, tokens, compactions) shipped, and the multi-client daemon (jcode-style socket server) stays design-only ([docs/daemon-design.md](docs/daemon-design.md)), while the UI-client seam of `tole serve` has started (epic #523): `GET /sessions/{id}/entries` reads a transcript page by page `POST /sessions {"resume": id}` reopens a session and `GET /sessions/{id}/events` streams it live over SSE with a lossless replay from a `seq` (#524); images are uploaded with `POST /sessions/{id}/images`, attached with `"images"` on a prompt and read back from `GET /sessions/{id}/images/{fnv1a64}` (#558). The automation epic (#577) has started: `tole-core` carries the scheduler's ledger core — an in-tree 5-field cron / `@every` parser and an append-only claim/fold/recovery log where a run is claimed before it starts, a terminal record is never overwritten and every skipped occurrence is recorded (#578); `tole cron create|edit|list|status|pause|resume|remove|run|cancel|runs|tick` stores jobs in an append-only `jobs.jsonl` and runs script jobs (argv, no shell, secret-shaped env scrubbed, zero tokens) from one `tick` pass that cron(8) or a systemd timer can host, and agent jobs (one durable session per run, per-job `--model`, `--max-steps`/`--max-minutes`/`--max-tokens` budgets and the user config's `[cron] daily_token_cap`); results go to `--deliver` targets (`log`, `session`, `file:PATH` inside the workdir, `command:ARGV` for CLI-saved jobs only) and `GET /cron/runs` on `tole serve`; `tole cron status` and `tole doctor` report a stale ticker; unattended Writes run only from the job's `--allow` list — under `tole serve --cron` (the in-process ticker) anything else is parked in `/approvals`, a standalone `tole cron tick` refuses it, and the run settles `awaiting_approval`; nothing is auto-approved (#579). An allow in `/approvals` continues the parked run in its own session; an agent creates jobs only through the `cron_create` tool, which saves them paused and within its own allow list, and only the owner's `tole cron resume` enables them (#581). A job can run on a zone's wall clock (`--timezone Asia/Jakarta`, `+07:00` or `UTC`; `cron.timezone` sets the default), with DST gaps and repeats run once, zone rules read from the host's zoneinfo at run time and `tole cron list` showing local time and UTC (#580). The profiles epic (#583) has started: dormant `[profiles.<name>]` personas in the user config (persona file, allow list, skills, memory room, model tiers), selected with `--profile <name>` (#584), and the core of delegation (#585): a budget carved from the parent's (never added), depth 2 by default (3 at most), parent ∩ profile allowlists, one write lease per path and a mandatory completion report, recorded as an auditable tree; the `delegate` tool runs each child as a budgeted `tole mission`, optionally as a profile, within the user config's `[delegation]` limits (`agent_start` is now its deprecated alias); a child hands in one capped completion report with `delegate_report` and is `done` only with a complete one; `tole audit [goal] [--json]` prints the tree read-only (budget carve and use, allow lists with dropped patterns, leases, report summaries; capped and redacted). Model routing by task weight (#586) has its rule baseline: with `[router] enabled = true` in the user config, a new run, mission, delegated child or cron run without a pinned model gets the tier its first matching rule picks (cheapest wins, no match keeps the configured model), recorded redacted in the session; `tole route` is the dry run. The advisor tier (#587) is in: a profile's `models.advisor` is consulted through `consult_advisor` in missions, delegated children and cron runs, at most `advisor_max_uses` times per run and out of the run's token budget, and only its fenced verdict (plan, correction or stop) comes back. v0.8.1 itself brought token-usage reporting and a capped, resumable `read_file` (#211), on top of v0.8.0, which added the per-project config `.tole/config.toml`
(trusted before it applies; see the Configuration section) on top of v0.7.x
(rescan-2 hardening, one tool-call authorization gate; see the CHANGELOG for
the behavior changes). Five faces on one durable core: the CLI (run / chat /
resume / sessions / jobs / **mission**), `tole mcp` (tool server),
`tole acp` (editor agent), `tole serve` (REST + multi-session
MCP-over-HTTP daemon), and the inline terminal UI (`tole-tui`, bare
`tole` on a TTY; MVP stage, [docs/tui.md](docs/tui.md)). Identity (owner-approved): a chat-first
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
  **Code review** (cora ≥ 0.19): with the review tools attached, the system
  prompt names cora's agent-as-reviewer flow — the model passes a diff to
  `mcp_cora_cora_review_prepare`, judges it itself (no LLM key in cora), then
  calls `mcp_cora_cora_review_finalize` with the same diff and its findings.
  **Review gate before commit** (#534, cora ≥ 0.20.0): while both review tools
  are attached, a `git commit` is refused until the STAGED diff has a
  validated review. tole records and hashes nothing itself: cora's own commit
  gate does (`mcp_cora_cora_review_finalize` leaves a marker bound to the
  exact change set; `cora hook verify` answers 0 validated / 10 blocking / 11
  not validated), and tole asks it before the commit runs. It covers the `git`
  tool and `run_command` / `job_start` running `git commit` (a `git -C <dir>
  commit` is checked in `<dir>`; one that redirects git's git dir or work tree,
  such as `--git-dir`, `GIT_DIR=` or `-c core.worktree=`, cannot be checked and
  is refused under `block`); `TOLE_REVIEW_GATE=warn`
  lets the commit through with a note, `off` (or `--review-gate off`) never
  gates, and with no review tools attached (`--no-auto-mcp`, no cora) there is
  no gate. A cora too old for `hook verify` fails open with one warning;
  `tole doctor` reports the state. It is a guardrail for a cooperating agent,
  not a security boundary: see the threat model for what it does not see.
  `--trust internal` allows the `mcp_cora_*` tools without prompting. The
  guidance is in the pinned prompt of `run`, `chat`, the TUI and `mission`
  (#536). `serve`, `acp` and `mcp` sessions serve their own registry and attach
  no external MCP server (the flag is refused there), so they have no review
  tools and no review section.
- **[uteke](https://github.com/codecoradev/uteke)** (`uteke`) — semantic
  memory. `uteke_recall` (recall) and `uteke_document` (markdown → durable
  knowledge) register automatically when the `uteke` binary is on PATH, and
  the optional [memory loop](#memory-loop) closes the circle.
- **Supported versions:** cora ≥ 0.19.0 and uteke ≥ 0.21.1 (both optional).
  `tole doctor` reports each one as supported, too old (with the upgrade
  command) or not installed. `tole setup cora` (#535) installs or upgrades
  cora for you: it runs `cargo install cora-code --locked` after a `[y/N]`
  at a terminal (or `--yes`; with neither it refuses and exits 2), does
  nothing when cora is already supported, and `--check` only reports.
  Cargo verifies the registry checksum; tole downloads nothing itself.
- Anything else that speaks **MCP over stdio** joins the same registry via
  repeatable `--mcp-server name=command [args...]` flags. **Every MCP server,
  cora included, needs your approval** (#599): on a terminal, `tole` shows
  its full tool list once and asks, and the approval pins a hash of its tool
  names, descriptions and schemas in `$CODECORA_HOME/tole/plugins.lock`. A
  server whose tools change later, or that you never approved, is not
  registered (missions, cron and `serve` never ask; approve once interactively,
  or give cron and CI a user lock ahead of time, see the migration note in
  [docs/third-party-plugins-design.md §8](docs/third-party-plugins-design.md));
  `tole doctor` shows the state; `tole plugin list|diff|approve|revoke`
  manages it (`approve --hash <sha256:…>` for scripts). Servers that
  offer prompts show up in `tole chat` as `/mcp:<server>:<prompt> [k=v ...]`
  (rendered server-side, sent as your next message); servers that offer
  resources add a ReadOnly `mcp_read` tool. Servers with neither add nothing.
- **Remote MCP servers over Streamable HTTP** (#530) use the same flag with a
  bare url: `--mcp-server research=https://mcp.example.com/mcp`. The bearer
  token comes from the environment only, `TOLE_MCP_<NAME>_TOKEN` (name
  upper-cased, `-` becomes `_`, e.g. `TOLE_MCP_RESEARCH_TOKEN`), is re-read on
  every (re)connect and is sent only to that endpoint; it is never accepted in
  the spec or a config file, and it and the url are scrubbed from errors. The
  url must be `https` (plain `http` only for `localhost` / `127.0.0.0/8` /
  `::1`), may not contain credentials, and a redirect to another origin is not
  followed. The tools register as `mcp_<name>_<tool>` and are **always
  `Write`** like every MCP tool; pre-authorize a trusted read-only server with
  `--allow 'mcp_research_*'`. Needs the `mcp` feature (on by default in
  `tole-cli`); embedders without it link no HTTP client.
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
  `POST /sessions/{id}/interrupt` (#377) steers the running turn instead,
  without losing its work: the `{"text", "urgent"}` message is injected at
  the turn's next safe point as an attributed entry (`409` when no turn
  is running).
  `POST /sessions/{id}/detach` (#217) stops waiting without aborting: the
  running tool settles, the turn ends `stopReason: "detached"` (resumable),
  and background jobs keep running and nudge the session when they finish
  (`409` when no turn is running).
  `--transport mcp` (issue #137) serves **multi-session MCP over
  Streamable HTTP** instead: one authenticated MCP connection addresses N
  durable tole sessions (`tole_session_new/prompt/cancel/interrupt/detach/status/list` +
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
tole                                        # terminal UI: bare `tole` on a TTY opens it (streaming transcript, approvals, slash commands)
tole chat                                   # multi-turn REPL on one durable session
tole chat --memory uteke                    # …with cross-session memory
tole chat --resume <id>                     # pick the thread back up (also: --last)
tole resume <id> "next instruction"         # continue a settled session
tole sessions                               # list durable sessions
tole sessions --all [--grep <text>]         # every project's sessions + where they live
tole sessions --tree                        # sessions with their forks nested
tole branch <id> [--at <entry>]             # fork a session at a settled entry, then resume the fork
tole import-claude <transcript.jsonl>       # import a Claude Code session as cc-<id>, then resume it
tole cron create "@daily" --script "backup.sh"  # scheduled job; `tole cron tick` (every minute from cron/systemd) runs what is due; list|pause|resume|remove|runs
tole cron create "0 9 * * 1-5" "Summarize open PRs" --allow write_file --model M  # agent job; `tole serve --cron` hosts the ticker
tole cron create "@daily" "Triage new issues" --skill triage --profile ops  # agent job with a skill (name or SKILL.md, content pinned on save: a changed skill blocks the run until `cron edit --skill` re-pins) and a user-config profile; Writes need --allow AND the profile's allow
tole cron edit NAME --deliver file:reports/prs.md   # also: run NAME (now), cancel NAME, status (ticker alive?)
tole hook add gh --key-env GH_HOOK_KEY --job triage --event push --template "commit: {head_commit.message}"  # webhook route in the USER config (never a project one); the key is named, never given or shown; allow_writes/filters are hand edits
tole route "Summarize open PRs" --kind cron --risk read_only  # dry run of [router] (#586): which tier and model a run would get, the configured model and its source (e.g. `from env TOLE_MODEL`), and why; no provider call
tole hook test gh --event push --payload push.json  # local dry run: sign, filter, print the fenced context; fires nothing (--fire --url http://127.0.0.1:PORT posts it to a loopback serve); also: list, remove ROUTE
tole status [id]                            # pc / seq / turns / token + cache usage / request-size split; model, effort, context used/window, tokens, compactions (default: latest session)
tole audit [goal] [--json]                  # this workspace's delegation tree, read-only, capped and redacted (default: list the goals)
```

Sessions live in `.tole/sessions/<id>.jsonl` (override with
`--sessions-dir`): append-only, crash-safe, replayable — kill the process
mid-turn and resume exactly where it stopped.

Useful flags (all subcommands): `--system` (persona, pinned in the session
header), `--workspace <dir>` (file-tools jail root), `--allow <glob>`
(repeatable pre-authorization for Write tools, e.g. `--allow 'write_*'`;
`run_command:<argv>` matches commands by argument, e.g.
`--allow 'run_command:git status *'` — a lone `*` is "any further
arguments", the program is matched literally; a leading `!` denies and wins
over every allow and `--yes`, e.g. `--allow '!run_command:rm *'`; with a
command deny set, only commands an explicit `run_command:` allow names run
unattended — everything else asks),
`--yes` (auto-allow every Write; Destructive still prompts), `--effort <minimal|low|medium|high>`
(reasoning effort sent as `reasoning_effort`; session register beats it across resumes),
`--mcp-server`,
`--no-auto-mcp` (skip the cora auto-preset), `--trust <preset>` (one-word
trust: `internal` / `read_only` / `workspace` — `read_only` plus
`write_file`/`edit_file` auto-allowed only inside an explicit
`--workspace` jail, and not for files another tool may execute — dot-paths
(`.git/hooks`, `.envrc`, `.vscode/`), build and task files (`build.rs`,
`Makefile`, `package.json`, ...) and scripts keep prompting, as do
`delete_file` and `run_command`; a home or filesystem-root workspace is
refused, and the guard is also available as `--allow 'write_file@workspace'`.
Not a sandbox: review what it wrote before you run it), `--skill <path>` (load a SKILL.md),
`--plan-mode` (read-only wire), `--on-turnend <cmd>` (final-message gate).

## Configuration (environment)

| Variable | Purpose |
|----------|---------|
| `TOLE_BASE_URL`, `TOLE_MODEL`, `TOLE_API_KEY` | LLM provider — any OpenAI-compatible endpoint (`OPENAI_*` equivalents read as fallback) |
| `TOLE_FALLBACK_BASE_URL`, `TOLE_FALLBACK_API_KEY`, `TOLE_FALLBACK_MODEL` | optional reserve provider (#209): a step that fails because the primary is unavailable (connection failure, timeout, 5xx, 429) is retried on the reserve, and the switch is recorded in the session log (`tole_route`). The reserve needs its own key (the primary's is never sent to another host); the model defaults to the primary's. Request errors (401/403/404/400) never fail over |
| `TOLE_SYSTEM_PROMPT` | default persona when `--system` is absent |
| `TOLE_CONTEXT_WINDOW` | the model's context window in tokens, for context compaction (#440). Without it, tole reads the window from the provider's `/models` (`context_length`, `max_model_len`, ...) once a prompt passes 8k tokens; with neither, compaction stays off. `chat`, `resume` and `mission` compact between turns once a prompt reaches `--compact-at` (default 0.8) of the window: the settled turns before the last one become one durable `compaction` entry (a fenced summary), the session file keeps everything, and `tole status` lists each compaction. `--no-compaction` (or config `no_compaction = true`) turns it off; config keys `context_window`, `compact_at`. **Gateways whose `/models` carries no window** (the fleet gateway lists only `id`, `created`, `owned_by`) never compact on their own: set `TOLE_CONTEXT_WINDOW` (e.g. `200000`); `tole doctor --tier catalog` warns when no window is known. **Overflow recovery:** if the provider rejects a request as larger than its context window (HTTP 400/413/422 `context_length_exceeded`, "maximum context length", ...) the turn ends with the provider's message and `(compaction will run on the next resume)`; the next `resume <id> "<prompt>"` (or the next `chat` message) first forces one compaction pass — whatever the window or threshold — then runs the prompt. Compaction stays between turns, never inside one; `--no-compaction` also disables the recovery pass. The turn that overflowed is kept verbatim, so if it alone exceeds the window, ask for less output per turn |
| `TOLE_REASONING_EFFORT` | reasoning effort sent as `reasoning_effort` on chat completions (#457): `minimal`, `low`, `medium` or `high`; unset (default) sends nothing, so the request body is unchanged. Also `--effort <level>` (flag > env > config `reasoning_effort` > user config). A session's durable `fact/reasoning_effort` register overrides it, so a session keeps its level on resume. A provider that answers HTTP 400 naming `reasoning_effort` gets ONE retry without it (remembered for the rest of the turn and recorded under `tole_degraded` in the step's usage row); an invalid level is a startup error. `tole config check` shows the effective value and its source |
| `TOLE_VISION` | `0` declares the model non-vision: image input (`--image`) fails with a model-capability error before any request; `1` declares it vision-capable. Unset = the server decides (a 400/415/422 that names images becomes the same capability error) |
| `TOLE_SKILL` | skill files to preload (`SKILL.md` paths, separated like `PATH`); same as repeated `--skill`, which replaces it; refused by the server faces like `--skill` |
| `TOLE_SKILL_APPROVAL` | `required` (default since 2026-11-11, #591) or `off` (every discovered skill loads, the old behaviour; environment only, no config file can set it): a discovered skill loads only when its whole directory hashes to the owner's approval; a session with unapproved skills prints one startup note with the count and the fix, and `tole doctor` shows the same. `--skill <file>` and a profile's `skills` are not gated (you named the file). Approve with `tole skills approve <name>` (`--hash <sha256:…>` to pin the hash you reviewed), check with `tole skills list` (status, hash, approval time), undo with `tole skills revoke <name>`. Approvals live in `$CODECORA_HOME/tole/skill-approvals.json`; any change to any file of a skill needs a new approval. An unapproved skill is listed by name only, as unavailable, and `load_skill` refuses it. Anything else fails startup, and so does a malformed approvals file |
| `TOLE_MEMORY` | memory loop backend (`uteke`) — same as `--memory uteke` |
| `TOLE_NO_UPDATE_CHECK` | `1` disables the startup update-check banner (issue #220) |
| `TOLE_SKILL_INDEX` | which skills a session is shown (#590): `local` (default; every discovered skill is listed, as before) or `uteke` (the pinned core plus the top K recalled by meaning from the `uteke doc` index for the `run` prompt, and the `skill_search` tool; an interactive session gets the pinned core only). With `--profile <name>` only skills scoped to that profile (or unscoped) are recalled. Every query is redacted against the process secrets before it reaches uteke. Index the skills first with `tole skills index`. `uteke` without the binary falls back to `local` with a note; anything else fails startup |
| `TOLE_SKILLS_PINNED` | comma-separated skill names always listed with `TOLE_SKILL_INDEX=uteke` (at most 5; a name with no local skill is skipped with a note) |
| `TOLE_SKILL_RECALL_K` | skills recalled by meaning per task, `0` to `10` (default `5`); anything else fails startup |
| `TOLE_SKILL_FLOOR` | cosine similarity floor for that recall, `0.0` to `1.0` (default `0.35`: a documented starting value, not a calibrated one; check it with `tole skills search` on your own store); fewer or zero skills is fine |
| `TOLE_REVIEW_GATE` | `off` / `warn` / `block` (default): what a `git commit` does while the cora review tools are attached and its staged diff has no validated review (#534); `--review-gate` sets it and wins; anything else fails startup. Also a config key `review_gate` (flag > env > config > user config); a project's `.tole/config.toml` can keep or tighten the gate but never loosen what your user config sets (the stricter of the two layers applies), and the key is covered by the project-config trust gate |
| `TOLE_THEME` | TUI color theme (#463): `default`, `light` or `mono`; beats the user config's `[tui] theme`. A set, non-empty `NO_COLOR` selects `mono` when neither is set. An unknown name falls back to `default` with a note. A project config cannot set it |
| `TOLE_VERBOSE` | `1` prints the startup probe misses (uteke / cora binary not found, no GitHub origin), which are silent by default; `tole doctor` reports the sibling binaries either way |
| `TOLE_MEMORY_NAMESPACE` | override the loop's uteke room (default: `repo-<directory name>`; the name predates rooms) |

### Image input

`tole run|chat|mission --image <path|data-url>` (repeatable) attaches images to the first message
(#212). Any later message can carry images too (#559): in `tole chat`, type `/image <path|data-url>`
to queue one for your next message (`/image` lists the queue, `/image clear` empties it; a message
that could not be recorded keeps its images queued). The TUI has the same `/image`, and pasting a
single image path or `data:image/…` URL attaches it (#560; no OS clipboard images in a terminal). `tole acp` accepts ACP `image` content blocks
(base64 `data`), advertises `promptCapabilities.image`, and runs an image-only prompt; the blocks
take the same spill-and-reference path as a `data:` URL, the declared `mimeType` is not trusted
(the bytes are sniffed), the optional `uri` is never read, and audio or embedded-resource blocks are
still ignored. A path must resolve **inside the workspace jail** (symlinks and `..` escapes are refused);
an inline `data:image/…;base64,…` URL is spilled to `~/.codecora/tole/images/` first. PNG, JPEG,
GIF and WebP (sniffed from the bytes), 10 MiB each. The wire carries OpenAI-compatible
`image_url` content parts; the session log stores only the reference (`path`, `mime`, `bytes`,
`fnv1a64`), and every later step — including `resume` — re-reads the file, re-checking the jail and
the hash (a changed or missing file reaches the model as an `[image unavailable: …]` note, never
silently). Images are input, not tools: no approval is involved. They cost tokens on every step
(the data URL counts in the request's `tole_wire.history_chars`). Needs a vision model; a
non-vision model is a loud `model-capability` error, not a dropped image. Image content is
untrusted input (text inside an image can carry instructions, like a fetched web page). There is
**no model-callable image tool**: images are always supplied by the user (see the threat model).

`tole serve` takes images too (#558): `POST /sessions/{id}/images` (raw body) stores an upload
under `~/.codecora/tole/images/serve/`, a prompt references it with `"images": [<id>]`, and
`GET /sessions/{id}/images/{fnv1a64}` reads back an image the session's log records. Quotas:
`TOLE_SERVE_IMAGE_SESSION_QUOTA` (bytes, default 64 MiB per session) and
`TOLE_SERVE_IMAGE_TOTAL_QUOTA` (bytes, default 512 MiB). See
[docs/mobile-control.md](docs/mobile-control.md#images-558).

### Provider retries

A transient provider failure is retried inside the turn, up to 3 times
(4 attempts), before the turn fails: HTTP 429, any 5xx, connection or
timeout errors, a stream read failure and an empty completion (no content and
no tool calls, streaming or not). Waits are 1 s, 2 s, 4 s plus up to 25%
jitter; a `Retry-After` header in seconds replaces the wait, capped at 30 s.
Other 4xx errors (auth, model, invalid request) fail immediately. Each retry
is written to the session log as a `provider transient failure, retrying`
error entry before the wait, the wait ends early on cancel (Esc, ACP
`session/cancel`), and the budget is per turn, not per step. With a reserve
provider (#209) a failed step still fails over first; retries run when every
provider in the chain has failed. An empty completion that outlasts its
retries fails the turn without trying the reserve.

## Provider doctor

`tole doctor` answers "is this provider/model actually usable?" in tiers:

| `--tier` | Checks |
|----------|--------|
| `offline` | config resolves, base URL shape, key present (no network) |
| `catalog` (default) | live `GET /models`; the configured model must be served. Also warns (`!`, exit code unchanged) when no context window is known for it — compaction then stays off until `TOLE_CONTEXT_WINDOW` / `context_window` is set |
| `full` | a chat call, a streamed call and a real tool-call loop (spends a few tokens) |

Tiers are cumulative and stop at the first blocker, which is printed with
the command that fixes it. The exit code is non-zero when the requested
tier is not reached, so `tole doctor --tier full` is the gate for "ready to
release" claims. API keys and URL credentials never appear in the output.

After the provider tiers, an `ecosystem:` section reports the sibling
binaries against the supported floor (cora ≥ 0.19.0, uteke ≥ 0.21.1). It is
informational: an old or missing sibling never fails the run.

Every run is recorded in a coverage ledger (`$CODECORA_HOME/tole/doctor-ledger.json`,
owner-readable only: level reached, first blocker, fix, timestamp per provider
+ model; `--no-ledger` skips it). `tole doctor --summary` prints `READY n-of-m`
plus the first blocker and its fix command without running anything; the latest
run wins per provider + model, so re-run `--tier full` before a release claim.

## Configuration (project and user files)

`<cwd>/.tole/config.toml` (no parent-directory walk) holds per-project defaults.
It is loaded by every command except `config`, `upgrade` and `approvals`; use
`--config <path>` to name another file (a path given on the command line is
trusted without a trust record) or `--no-config` to ignore any file entirely
(no discovery, no trust check, no output). When a config is loaded, one line
`tole: using config <path>` goes to stderr. **Precedence: flag > env > config >
default**, and a project without a config file behaves exactly as before.

**User config (#433).** `$CODECORA_HOME/tole/config.toml` (default
`~/.codecora/tole/config.toml`) holds a user's global defaults with the same
keys and validation, e.g. a skill to preload everywhere. It sits below the
project file: **flag > env > config > user config > default**, key by key (a
project key replaces the user key, lists included). It needs no trust record
(it lives outside any repository, like the trust store), prints
`tole: using user config <path>`, and `--no-config` ignores it too.
`tole config check` lists both files and shows which layer wins each key.

**Keys.** Every key is optional; the schema is strict (an unknown key, a wrong
type or a secret-like key is an error with `file:line`; the file is capped at
64 KiB).

| Key | Mirrors | Effective value |
|-----|---------|-----------------|
| `model`, `base_url`, `system_prompt`, `memory` | the provider env, `--system`, `--memory` | flag > env > config |
| `sessions_dir`, `workspace` | `--sessions-dir`, `--workspace` | flag > config > default |
| `[mission]` `max_steps`, `max_minutes`, `max_tokens` | `mission --max-*` | flag > config > budget tier |
| `[tui]` `theme`, `[tui.colors]` | `TOLE_THEME`, `NO_COLOR` | **user config only** (a project's `[tui]` is ignored with a warning): env > user config > `NO_COLOR` (`mono`) > `default`; see docs/tui.md |
| `[mission]` `verify`, `verify_timeout` | `mission --verify`, `--verify-timeout` | flag > config > default (`300`) |
| `[cron]` `daily_token_cap` | (none) | **user config only** (a project's `[cron]` is ignored): tokens all cron agent runs of one UTC day may spend; past it they settle `budget` |
| `[delegation]` `max_depth`, `max_live_processes`, `goal_budget`, `goal_steps`, `goal_minutes` | (none) | **user config only** (a project's `[delegation]` is ignored): depth below the orchestrator (1–3, default 2), delegated processes live at once before spawns queue (default 8), and a goal's default budget (1,000,000 tokens, 400 steps, 120 minutes; used by the `delegate` tool, next PR) |
| `[delegation]` `min_free_memory_mb`, `child_memory_mb`, `max_load_per_cpu`, `min_free_disk_mb`, `min_free_pids_percent`, `spawn_interval_secs`, `machine_max_live`, `child_nice` | `1024`, `512`, `1.5`, `2048`, `10`, `2`, max(2, CPUs), `10` | **user config only**: the machine resource guard `delegate` and `delegate_poll` run before every delegated launch. A child starts only when the machine-wide live count is under `machine_max_live`, the last start was at least `spawn_interval_secs` ago, free memory covers `min_free_memory_mb` plus `child_memory_mb` for it and each child still ramping up (cgroup v2 limits count), the one-minute load per CPU is at most `max_load_per_cpu`, `min_free_pids_percent` of the process-id limit is free, and `min_free_disk_mb` is free on the data dir, workspace and worktree filesystems; otherwise the spawn waits in the queue, and the replies carry its `queued_reason` and `waited_secs` (`tole doctor` shows the headroom). Children run at niceness `child_nice`. Zero, negative or absurd values are config errors |
| `[cron]` `max_concurrent` | `4` | **user config only**: runs one tick (`tole cron tick`, the `tole serve --cron` ticker) executes at once, 1 to 64; `tole cron tick --max-concurrent N` overrides it for that tick |
| `[cron]` `timezone` | (none: plain UTC) | **user config only**: the time zone of a job saved without `tole cron create --timezone` (`UTC`, `+07:00` or an IANA name like `Asia/Jakarta`). Jobs saved earlier keep no zone |
| `[cron]` `tz_source` | `system` on unix, else `offset-only` | **user config only**: where IANA zone names resolve. `system` reads the host's zoneinfo at run time (`$TZDIR`, else `/usr/share/zoneinfo`; no zone data in the binary); `offset-only` takes only `UTC` and fixed offsets |
| `[hooks.<route>]` `hmac_key_env` / `hmac_key_file`, `insecure_loopback`, `rate_per_minute`, `max_body_bytes` | (none), `false`, `30`, `262144` | **user config only**: a `POST /hooks/<route>` on `tole serve`. Each route has an HMAC key, named by an environment variable or held in a file (a regular file, at most 4 KiB, at least 16 bytes); the key is never shown. A route without a key stops `tole serve` at startup, and `insecure_loopback` loads on a loopback bind only. Limits: 1–600 deliveries per minute and at most 1 MiB per body |
| `[router]` `enabled`, `allow_tiers`, `[router.tiers]`, `[[router.rules]]` | off | **user config only** (a project config can only set `enabled = false` or narrow `allow_tiers`): model routing by task weight (#586). Each rule picks a tier (`light`, `standard`, `heavy`) when all its conditions hold (`kinds`, `min_chars`/`max_chars`, `min_risk`/`max_risk`, `profiles`, `jobs`, `budget_below`, `keywords`). The cheapest matching tier wins, and no match keeps the configured model. A matching rule replaces the configured model whether it comes from `TOLE_MODEL`, the config's `model` or a profile's default; only a session's own model (`/model`) and a cron job's `--model` pin are never routed. A rule without `profiles` applies only to runs without a profile. A tier resolves through `[router.tiers]`, or through `profiles.<name>.models` for rules naming `profiles`; an undeclared tier is an error, never another model. `tole route` is the dry run. A new `run`, `mission`, delegated child or cron run is routed once, before its first provider call: an explicit model (a session's `/model`, a job's `--model`) wins, the decision goes to the session's `fact/route` register (redacted), and a resumed session keeps its model |
| `[hooks.<route>]` `cron_job`, `events`, `filter`, `template`, `allow_writes` | (none), all, (none), (none), `false` | **user config only**: what a verified delivery does.<br>• A delivery whose event (`X-GitHub-Event` / `X-Tole-Event`) is not in `events`, or whose JSON body fails `filter`, gets `200 ignored` and fires nothing.<br>• A match fires the existing job `cron_job` (`202` once the run is claimed). It goes through the checks a scheduled run does: a paused, running, untrusted or out-of-budget job gets `409`.<br>• The fired run sees `template` (`{dot.path}` placeholders, scalars only, redacted and capped) as fenced untrusted data appended to its prompt for that run only.<br>• It has no Write tools unless `allow_writes = true`.<br>• `[hooks.<route>.filter]` takes one of `equals`, `contains`, `in`, `regex`, `exists` on a `path`, or `all` / `any` / `not` over nested filters. `contains` is a substring match on a string, but a whole-element match on an array: `labels` contains `"bug"` matches `["bug"]`, not `["bugfix"]`. Regexes are `regex-lite` (at most 256 bytes; strings over 16 KiB never match) |
| `trust` (list of presets) | `--trust` | flag > `TOLE_TRUST` env > config |
| `allow` (list of globs) | the subcommand's own `--allow` | flag > config |
| `mcp_server` (list) | `--mcp-server` | flag > config |
| `on_pretool`, `on_posttool`, `on_turnend` (lists) | `--on-pretool` / `--on-posttool` / `--on-turnend` | flag > config |
| `skill` (list of paths) | `--skill` | flag > config (paths resolve against the cwd) |
| `plan_mode`, `no_auto_mcp`, `no_skills` (booleans) | `--plan-mode`, `--no-auto-mcp`, `--no-skills` | `flag \|\| config` |
| `[profiles.<name>]` | `--profile <name>` | **user config only** (a project's `[profiles]` is ignored with a warning); see Profiles below |

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

**Profiles (#584).** A profile is a dormant persona in the user config: a
persona file, an allow list, skills, a memory room and model tiers. Nothing
runs for it and nothing is read for it until a command selects it with the
global `--profile <name>`; then only its persona file is read.

```toml
[profiles.coder]
system_prompt_file = "~/.codecora/tole/profiles/coder.md"  # or relative to the user config's directory
allow = ["write_*"]
skills = ["~/skills/rust/SKILL.md"]
memory_room = "profile-coder"          # default: profile-<name>
grants = ["GITHUB_TOKEN"]              # secret NAMES only, never values (used by #585/#598)
advisor_max_uses = 3                   # consult_advisor calls per run, 1-20 (#587; needs models.advisor; missions, delegated children and cron runs only, out of the run's token budget)
[profiles.coder.models]
light = "small-model"
standard = "mid-model"
heavy = "big-model"
advisor = "big-model"                  # consulted, never executes (#587)
default = "standard"                   # light | standard | heavy
```

`[secrets]` (#598) binds each secret NAME to the tools whose child processes
may receive it; every other secret-shaped variable is scrubbed from them.
**`run_command`, `job_start` and cron scripts get no secret by default**;
`GITHUB_TOKEN` defaults to the `gh` and `git` tools and your hooks and MCP
servers. A project config can only narrow a binding. Sensitive files
(`.env*`, `*.pem`, `id_*`, `.npmrc`, `.netrc`, `*.key`) read as
`KEY=«redacted»`; to read one verbatim, approve a `read_file_raw` call (or
`--allow read_file_raw`). `tole doctor` lists which secrets are set and which
tools may receive each. Migration note:
[docs/secrets-integrations-design.md §9](docs/secrets-integrations-design.md).

```toml
[secrets]
GITHUB_TOKEN = { tools = ["gh", "git", "hook", "mcp", "run_command"] }  # restore it for commands
NPM_TOKEN = { tools = ["run_command"] }
```

A selected profile is one layer of the usual precedence: **per-key flag
(`--system`, `--allow`, `--skill`) > profile > env > config > user config**.
The `default` tier's model replaces the model of the provider triple (it beats
`TOLE_MODEL`); the persona replaces the system prompt (for `mission`, the
default base prompt); the memory loop uses the profile's room, except in a
child agent, whose mailbox room always wins. A non-empty profile `allow`
list **replaces** the config `allow` list rather than merging with it, exactly
as a non-empty `--allow` does: while the profile is selected, the config's
patterns do not apply. Names are `[a-z0-9_-]`; an unknown
name is a startup error that lists the defined ones.
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
| `git` | Write | `status` / `diff` / `add` / `commit` only — **push stays human**; while the cora review tools are attached, `commit` needs a reviewed staged diff (see the review gate under [Ecosystem position](#ecosystem-position), #534) |
| `gh` | Write | read-only ops, argv-validated per op; `repo` defaults to the checkout's GitHub remote, optional per-call override (validated) |
| `run_command` | Write | argv-split (no shell), cwd-jailed, timeout + output cap |
| `verify_package` | RO | crates.io / npm registry check before any install — hallucinated names get NOT FOUND + candidates, edit-distance-1 candidates get a typo-squat warning |
| `load_skill` | RO | loads a discovered SKILL.md on demand (`--skill` pins one upfront; `--no-skills` disables) |
| `skill_search` | RO | with `TOLE_SKILL_INDEX=uteke` (#590): finds skills by meaning in the uteke index; returns names and descriptions from the local skill files, bodies still come through `load_skill` |
| `gitea` | Write | Gitea counterpart of `gh` over the instance REST API — registers when `origin` is a Gitea remote AND `TOLE_GITEA_TOKEN`/`GITEA_TOKEN` is set; same six ops |
| `agent_start`, `agent_poll` | Write / Write | depth-1 child agents: spawn durable child sessions (structurally no grandchildren), results via per-child uteke mailboxes (ephemeral by default); `agent_poll` is Write because a settled poll consumes (cleans) the mailbox, and `--trust internal` allowlists it by name; `--agents-worktree` gives each child its own git worktree; siblings are told (#380) when another child changes a file they read or wrote, at their next safe point; `role` (`explore`/`implement`/`verify`/`fix`) and `consumes` (artifacts by reference) give children a typed handoff, and an implement/fix child without passing verification is not closed (#383) |
| `systemone_decide` | RO | typed decisions (choice/score/noul + confidence) from a System One backend — active when `SYSTEMONE_API_KEY` is set; `SYSTEMONE_BASE_URL` picks the backend (hosted Jev default, self-hosted compatible) |
| `job_start`, `job_poll` | Write / RO | detached long-running jobs with log tailing |
| `cora_search` | RO | hybrid codebase search via `cora brain`; native fallback — skipped when the cora MCP surface is attached |
| `uteke_recall`, `uteke_document` | RO / Write | semantic memory recall / markdown → room |
| `mcp_*` (from `--mcp-server`, stdio or `https://` url) | Write | server metadata is **never** trusted for risk; approval gate always applies |
| `web_fetch`, `web_search` | RO | text-only internet (no JS/browser): fetch is direct HTTPS, size-capped, content-type allowlisted, HTML→text — or, with `TOLE_WEB_FETCH_URL` set, goes through your fetch backend for JS-heavy or bot-protected pages (default contract `GET ?url=<target>` → `{"content":"…"}`; `TOLE_WEB_FETCH_METHOD=POST` sends `{"url"}`, `TOLE_WEB_FETCH_HEADERS` like the search ones, `TOLE_WEB_FETCH_FIELDS='content=markdown'` maps the field; https targets only, vetted locally first; backend only, no direct fallback; the target URL is disclosed to the backend operator; a malformed value fails startup); search needs `TOLE_WEB_SEARCH_URL` (default contract `GET ?q=` → `{"results":[{title,url,snippet}]}`; `TOLE_WEB_SEARCH_METHOD=POST` sends `{"query","limit"}`, `TOLE_WEB_SEARCH_HEADERS='Authorization=Bearer ${KEY}'` adds headers sent only to the backend, `TOLE_WEB_SEARCH_FIELDS='snippet=description,results=items'` maps fields; a malformed value fails startup) — no backend, no tool; results are model content, never executed |
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
`n/a`). It also lists per-user-turn stats (#379: input / output / cache-read /
cache-write / reasoning tokens, `turns` in the status JSON), summed from the same
ledger so they survive restarts; a value no step reported is `n/a` / `null`,
never 0. Reported only — `--max-tokens` accounting is unchanged.

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

Memory lives in the uteke room `repo-<dir>` (uteke ≥ 0.21.1; rooms replace
the deprecated namespaces; `TOLE_MEMORY_NAMESPACE` overrides the room). Memory
stored by earlier tole versions in the namespace of the same name is still
recalled while the room is empty. The injected batch is hard-capped
at 24 items / 64 KiB (a larger `limit` is clamped; a hit is never cut
mid-way) and deduplicated. A batch is all-or-nothing: an unparseable
recall payload fails the whole batch with a stderr note rather than
injecting fewer items, and the turn proceeds without memory.

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
  [Performance record](docs/perf.md) ·
  [Child-agent handoff](docs/child-agents-handoff.md)
- [CONTRIBUTING.md](CONTRIBUTING.md) — workflow, issue/PR rules, merge gate,
  cora review gate, live-mission binary hygiene
- [AGENTS.md](AGENTS.md) — the same rules for AI coding agents
- [CHANGELOG.md](CHANGELOG.md) · [Security policy](SECURITY.md)

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
