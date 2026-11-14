# Migrating from 0.10.1 to 0.11.0

0.11.0 closes the secret-handling and owner-approval gaps of the agentic
automation work (epics #583 and #594). Most of it is a tightening: something a
session could do silently before now needs the owner's decision, or no longer
receives a secret. This page lists every BREAKING change since `v0.10.1`, who is
affected, and the one step that restores the old behaviour where there is one.
The full entries, with the reasoning, are in `CHANGELOG.md` under `0.11.0`.

Each item was checked against the code of `develop` at the time of writing; the
identifiers named here (env vars, config keys, tools, subcommands) exist in it.

Quick check after upgrading: run `tole doctor`. It reports the skills waiting for
approval, the state of each MCP server in the plugin lock, which secret names are
set and which tools may receive each, and the process guard.

## Summary

| Change | Affects you if | One-line step |
|---|---|---|
| Skills need approval | you use skills from `skills/` or `~/.codecora/tole/skills/` | `tole skills approve <name> --hash <hash>` |
| MCP servers need approval | you attach any MCP server, the auto-attached `cora` included | accept the prompt once, or `tole plugin approve <name>` |
| Commands get no secrets | a command, job or cron script reads `GITHUB_TOKEN` or another secret | grant it in `[secrets]` |
| More names and values are secrets | a command reads `*_KEY`, `*_PASS`, `DATABASE_URL`, ... | grant it in `[secrets]` |
| `verify` and `turnend` get no secrets | a mission `--verify` command or `on_turnend` gate needs a token | grant the `verify` / `turnend` consumers |
| Sensitive files read redacted | the model must see an `.env`, key or credential file verbatim | `read_file_raw` (approved) |
| The session log is redacted | you read secrets back out of logs, exports or handoffs | none for new logs |
| Sessions cannot run `tole` | a mission runs `tole ...` through `run_command` | run it yourself in a terminal |
| No mutating file tools in the data dir | you start tole in `~` | start in a project directory |
| Child agents get a scrubbed environment | a child relied on a secret only the parent held | grant it in `[secrets]` |
| Rust API changes | you embed `tole-core`, `tole-tui` or `tole-cli` as libraries | see "Library API" |

## Skill approval is required (#591)

Without `TOLE_SKILL_APPROVAL` set, a session loads a discovered skill only when
its whole directory (SKILL.md and every other file) hashes to the owner's
approval in `$CODECORA_HOME/tole/skill-approvals.json`. Any other skill is listed
by name as unavailable and `load_skill` refuses it. Cron jobs naming an
unapproved skill are `blocked` with the exact approve command in the detail.
`--skill <file>` and a profile's `skills` stay ungated.

Migrate:

```sh
tole skills list                              # shows each skill and its hash
tole skills approve <name> --hash <sha256:...>
```

Without a terminal, `approve` refuses unless `--hash` is given, so scripts must
pass it. To restore the old behaviour, set `TOLE_SKILL_APPROVAL=off` in the
environment; only the environment can opt out, no config file can. Changing any
file inside an approved skill revokes its approval until you approve it again.

## MCP servers need approval, pinned in a plugin lock (#599)

Every connect, tole hashes the server's live tool manifest and its version and
registers the tools only when both match the owner's approval in
`$CODECORA_HOME/tole/plugins.lock`. An unapproved, changed or revoked server
registers no tools, and since #696 is not even started. The approval also binds
the server's command line or URL, the SHA-256 of its program file (when one was
recorded) and the secrets it held at approval. This applies to the auto-attached
`cora` server too.

Migrate, once per server:

- in a terminal session (`tole`, `tole chat`, `tole run`, `tole resume`), read the
  full tool list tole shows and answer yes; or
- `tole plugin list` to see each server's state, `tole plugin diff <name>` to see
  what changed, `tole plugin approve <name>` to approve, `tole plugin revoke
  <name>` to take it back.

Missions, cron, `tole serve` and child agents never approve, so approve first
interactively. Re-approve after you upgrade an MCP server's binary (`cargo
install cora-code`, a new `gh` extension) or edit its command line; a server that
should receive a secret it did not hold at approval needs a re-approval too. A
project lock (`.tole/plugins.lock`) can only narrow the user lock. Details and CI
guidance: `docs/third-party-plugins-design.md` section 8.

## Commands get no secrets by default; scoped release (#598, #682)

`run_command`, `job_start` and cron scripts no longer inherit `GITHUB_TOKEN` or
any other secret. A `[secrets]` table in the **user** config binds each secret
name to the tools whose child processes may receive it:

```toml
[secrets]
GITHUB_TOKEN = { tools = ["gh", "git", "hook", "mcp", "run_command"] }
DATABASE_URL = { tools = ["run_command"] }
```

The default keeps `GITHUB_TOKEN` for the `gh` and `git` tools, your own hooks and
MCP servers only. A project config can only narrow a binding; a widening entry is
ignored with a warning. Redaction is unchanged and covers every secret, granted
or not.

What counts as a secret is wider (#682): names ending in `PASS`, `_KEY`, `_DSN`,
`_SK`, `_COOKIE`, `_AUTH`, `_BEARER` or `DATABASE_URL`, names starting with
`AUTH_` or `BEARER_`, and `PASSPHRASE` anywhere; `SSH_AUTH_SOCK`, `GPG_KEY`,
public keys and `*BYPASS` stay ordinary. A variable is also a secret by value: a
URL with a password under any name (its password is redacted; `*_PROXY` variables
are not scrubbed), and any value tole holds by role (webhook route keys, the
serve token, `${NAME}` web-header expansions). `[secrets]` keys may be `*_URL` /
`*_URI` names so a scrubbed connection string can be granted back.

Migrate: run `tole doctor` to see which names are set and who may receive each,
then add the `[secrets]` entries your commands need.

`tole serve --token <value>` now prints a warning because the value is readable
by every local process; use the new `--token-file <path>` (a regular file, at
most 4 KiB) or `TOLE_SERVE_TOKEN`.

## `mission --verify` and `on_turnend` gates get no secrets (#676)

Both run unattended and usually execute code the agent just wrote. Each now has
its own `[secrets]` consumer, `verify` and `turnend`, bound to nothing by default
(pre- and post-tool hooks keep the `hook` consumer). If a verify command or stop
gate needs a token, list every consumer that should keep it:

```toml
[secrets]
GITHUB_TOKEN = { tools = ["gh", "git", "hook", "mcp", "verify", "turnend"] }
```

## More files are sensitive; `read_file_raw` (#598, #683)

`read_file` returns sensitive files redacted (`KEY=«redacted»` lines, a single
`«redacted»` for a key file, no content hash); the git diff filter and
`cora_search` hits hide them too. Beyond `.env*`, `*.pem`, `id_*`, `.npmrc`,
`.netrc` and `*.key`, the name rule now covers `*.env`, `*.env.*`,
`credentials`, `.git-credentials`, `.pgpass`, `.htpasswd`, `.pypirc`,
`.dockercfg`, `config.json` directly under `.docker`, `*.p12`, `*.pfx`, `*.jks`,
`*.keystore`, `*.tfvars`, `kubeconfig` and `secrets.y*ml`. Add your own with
`sensitive_files = ["*.secret", "vault-*.json"]` (name globs, no directory part;
a project list adds to the user's and cannot remove a name).

Migrate: a tool that needs the verbatim content uses `read_file_raw`, which is
`Risk::Write` (approve the call) or start with `--allow read_file_raw`. Known
gap: a hard link to a sensitive file under another name is not detected.

## The session log stores the redacted form (#596)

Every entry payload and the pinned system prompt are redacted with the process
secret set before the line is written, so the log holds exactly what the provider
saw. Logs written before 0.11 are never rewritten; their old lines stay raw.
`/export`, the memory-loop summary, handoffs and forks of a new session therefore
carry no known secret. A tool still runs with the arguments the model sent; a
crash-resumed intent re-runs with the logged (redacted) arguments. Nothing to do,
unless something of yours read a secret value back out of a log.

## A session cannot run `tole`, and owner-only subcommands refuse (#684)

In every session kind, `run_command` and `job_start` refuse an argv whose program
is the tole binary (by name, by resolving to the running executable, behind
`env`, `nohup` or `sh -c`) and any argument naming `TOLE_UNDER_SESSION`. Every
process they start carries `TOLE_UNDER_SESSION=1`, and `tole cron
create|resume|edit`, `plugin approve`, `skills approve`, `skills drafts promote`
and `config trust` refuse to write when that marker or `TOLE_AGENT_DEPTH >= 1` is
set. A human at a terminal is unaffected. A mission that had the model run `tole
...` must be run by the owner instead. This is defense in depth, not a boundary:
a copied binary with a cleared environment, or an interpreter, can still write
the files (`docs/threat-model.md`).

## No file-mutating tools when the workspace holds the tole data dir (#684)

Started in `~`, the file-tool jail contained `~/.codecora/tole` (cron ledger,
`plugins.lock`, `skill-approvals.json`, `trusted-configs.json`, the user config).
Every session kind now registers no `write_file`, `edit_file` or `delete_file`
when the workspace contains, or sits inside, the data dir, and tole warns at
startup. Start tole in a project directory. Cron agent runs already refused such
a workdir.

## Child agents get a scrubbed environment (#595, #598)

`agent_start` children (and delegated children) no longer inherit the parent's
whole environment. They get the scrubbed environment plus their provider keys and
base URLs, and only the secrets their parent's release grants, narrowed by a
profile's `grants`. A child can no longer use an integration whose secret lives
only in the parent's environment (`TOLE_MCP_<NAME>_TOKEN`, `SYSTEMONE_API_KEY`,
web backend header keys). Bind the secret in `[secrets]` if a child needs it.

## Library API (embedders)

Only relevant if you depend on the crates directly.

- `tole_core::mcp::register_server_tools` takes a `&SecretSet` and an `Admit`
  (the plugin-lock decision; see `tole_cli::plugin_gate` for the host side).
- `tole_core::memory::remember_session` takes a `&SecretSet`.
- `tole_core::web::WebFetchTool` is no longer a unit struct; use
  `WebFetchTool::default()` or `with_secrets`. `McpTool`, `WebSearchTool`,
  `WebBackendFetchTool`, `SystemOneTool` and `UtekeDocumentTool` gained
  `with_secrets`.
- `tole_tui::TuiHost` is `footer`, `run_turn` and `apply(HostCmd) ->
  Result<HostReply, String>` (plus defaulted `theme`, `keymap`,
  `startup_note`) in place of `models`, `set_model`, `set_effort`,
  `request_compaction`, `new_session`, `sessions`, `switch_session`,
  `status_lines` and `set_name`. Input is the UI-neutral `Input`
  (`App::on_input`); `App::on_key` is the crossterm entry point. A pending
  approval is an `ApprovalRequest` and `Prompter` gained `post`.
- `tole_cli::session_host::{UpdatePlanTool, PlanEmitter, register_gitea,
  detect_github_repo, github_repo_from_remote_url}` moved to
  `tole_cli::tool_roster`, with no re-export.
- The redaction marker changed from `<redacted>` to `«NAME»`, and a provider key
  shorter than 8 characters is no longer redacted.

## New in 0.11, nothing to migrate from 0.10.1

These were never in a release, so a 0.10.1 install has nothing to convert. They
matter only if you ran builds from `develop`.

- Native webhook signatures are scheme v2 (`X-Tole-Signature: v2=<hex>`, HMAC
  over `v2`, timestamp, delivery id, event and body joined by newlines); the v1
  `sha256=` form is refused with 401. GitHub deliveries are deduplicated on the
  signed body. Senders: `tole hook test --fire` already signs v2.
- The plugin lock is version 2 and records source, binary hash and secrets; a
  lock written by an earlier `develop` build keeps working while the command line
  and binary are the ones approved.
- `tole cron`, `tole hook`, `tole plugin`, `tole audit`, `tole discuss`,
  `tole curator`, profiles, delegation and routing are new, along with the
  owner-only rule above: cron jobs not created with `tole cron create` stay
  `blocked` until `tole cron resume`.

## Other behaviour you may notice

- tole is non-dumpable on Linux (#675): same-user children cannot read tole's
  `/proc/<pid>/environ` or memory, no core dump is written, and attaching a
  debugger to a running tole needs root. `tole doctor` shows the state.
- A `git commit` through the agent is refused until the staged diff has a
  validated cora review, while both `mcp_cora_cora_review_*` tools are attached
  (#534). The default is `block`; set `--review-gate off|warn`,
  `TOLE_REVIEW_GATE` or `review_gate` in the config to change it.
- `agent_start` and `agent_poll` are now aliases of `delegate` and
  `delegate_poll` in the CLI faces and will be removed in the next release.
- Built-in `web_search` and `web_fetch` (default `web-builtin` feature) work
  without a backend; a configured `TOLE_WEB_*` backend still wins, then nginjen
  (`NGINJEN_BASE_URL` and `NGINJEN_API_KEY`).
