# Threat Model — tole

Status: initial version (2026-09-12), mapped against the OWASP Agentic/LLM
Top 10 checklist (issue #72). Every item resolves to **control** (where it
lives) / **gap** (tracked) / **N/A** (with rationale).

## Assets

| Asset | Where | Notes |
|---|---|---|
| Provider API key | `TOLE_/OPENAI_*` env | Never persisted; scrubbed from wire errors/messages |
| Session logs (JSONL) | sessions dir | Full transcript incl. tool results — local only |
| Job logs | `<workspace>/tole-jobs/` | Child stdout/stderr, attacker-influenceable content |
| Workspace files | `--workspace` dir (default cwd) | Readable AND writable by the model via tools |
| Host env of child processes | inherited by spawned commands | See ENV-1 |
| Subprocess execution authority | run_command / job_start / git / gh | The model chooses argv within argv-split + jail rules |

## Trust boundaries

1. **Provider responses → context.** The model's output becomes intent
   entries; tool args are argv-validated but otherwise untrusted.
2. **Tool results → context.** File contents, job logs, command stdout flow
   back into the transcript and are sent to the provider. A malicious
   workspace file or a compromised job subprocess can inject
   instructions here (OWASP: prompt injection).
3. **Workspace → filesystem.** File tools jail to `--workspace`
   (component-validated walk using `symlink_metadata`, final component
   opened with `O_NOFOLLOW` on unix, #68; a parent-directory swap between
   walk and open remains a residual race — see Deliberate limitations);
   `run_command`/`job_start`
   run with cwd jailed but FULL process authority — the jail bounds *cwd*,
   not capability.
4. **Child env.** Spawned commands inherit the host environment by default.

## OWASP Agentic/LLM Top-10 mapping

| OWASP item | Status | Where / rationale |
|---|---|---|
| Prompt injection (direct/indirect) | **control + documented residual** | Tool results on the wire are wrapped in `TOOL_RESULT_BEGIN/END` fences with inner fence markers neutralized (this fix); secrets scrubbed (E11). Residual: an injected instruction inside data can still steer the model — no framework fully solves this; delimiters + user-visible tool audit lines (`what: …`) limit blast radius. |
| Insecure output handling | **control** | Same fence treatment; result sizes capped (read_file 1 MiB file cap and 20,000-char head cap per call, run_command 4000/2000 chars, job_poll 8 KiB window). |
| Excessive agency / tool misuse | **control (defense-in-depth) + documented residual** | Primary control: the per-call approval gate (every non-ReadOnly spawn needs consent). Secondary: `check_destructive_argv` blocklist refuses headline catastrophic patterns (teardown tools, recursive rm escaping the workspace, wrapper-nested variants) in BOTH `run_command` and `job_start`. The blocklist is best-effort by nature — wrapper/interpreter bypasses (`sudo apt ...`, `timeout 5 dd ...`, `find / -delete`, `python -c rmtree`) are expected and accepted residual risk; a real argv sandbox is a different product and explicitly out of scope. |
| Sensitive data disclosure (env) | **gap → fixed here (ENV-1)** | Children now inherit a scrubbed env: any var whose name contains `API_KEY`, `_SECRET`, `_TOKEN`, or `PASSWORD` is removed (`GITHUB_TOKEN` kept for gh/git auth — documented trade-off). |
| Supply chain (deps) | **control** | Cargo Audit + Trivy FS Scan required by rulesets on every PR and push. |
| Resource exhaustion | **control + fixed here (JOB-2)** | Provider 120 s timeout, subprocess 30 s ceiling, step budget 32, loop guard, read caps; job logs are read via bounded windows on poll (never a whole-log load). Since the 2026-09-18 scan triage, a ReadOnly poll no longer rewrites the log file — runaway log disk growth is bounded by the job's lifetime and clearing a runaway log is an operator action. |
| Session/message tampering | **control** | Append-only JSONL, CAS state transitions, seq monotonicity, torn-line recovery with truncation (#68); compaction never drops entries. |
| Injection via config | **control (MCP shipped)** | `.cora.yaml`/session headers are owner-controlled. MCP servers (#74) add an untrusted surface: server-supplied tool metadata is never trusted for risk tier (everything is Write → approval gate), tool results flow through the same fence/scrub path as native results, and the connection env is scrubbed. Residual: a malicious MCP server controls its own tool descriptions (model-visible) — treat server config as operator trust. |
| Identity & authz of sub-agents | **N/A** | tole v0 is single-agent, single-tenant. MCP servers are external tools, not sub-agents. |
| Human oversight | **control** | Risk-tiered approval gate (every non-ReadOnly call; Destructive never auto-allowed), #68 closed the replay-without-consent hole. |

## Deliberate limitations (documented, not fixed)

- **`GITHUB_TOKEN` stays in child env** — gh/git push auth needs it; the
  token can therefore be read by any command the model runs. Mitigating
  context: the model already holds provider credentials for its own API
  calls by necessity; both are operator-scoped secrets on a
  single-operator host.
- **Workspace width is the operator's choice** — `--workspace /` is
  technically possible and a terrible idea; the flag validates existence,
  not sanity. Documented in CONTRIBUTING.
- **Symlink swap inside `tole-jobs/`** — requires a local attacker with
  write access to the operator's workspace, at which point host
  compromise is already achieved. Jobs dirs are created 0700 to reduce
  exposure; no further control planned.
- **`write_file` parent-component swap (residual TOCTOU)** — the walk
  checks each component with `symlink_metadata`, then the final `open()`
  uses `O_NOFOLLOW`, which protects only the FINAL component. A leaf
  symlink swap is refused; replacing an already-verified parent directory
  with a symlink between the walk and the `open()` is not caught and can
  escape the jail. Exploiting it requires a concurrent local process with
  write access to the workspace, and the model already has full process
  authority through `run_command`/`job_start` (the jail bounds cwd, not
  capability), so severity is low. A per-component `openat` descent
  (`O_DIRECTORY | O_NOFOLLOW`, e.g. via `cap-std`) could close it as a
  possible future hardening; none is planned or promised. Non-unix hosts
  have no `O_NOFOLLOW` equivalent wired (best-effort `symlink_metadata`
  check only).
- **`git status`/`diff` are repo-wide by design** — they take no
  pathspec and run with cwd = the tool's workdir, so they show the whole
  repository containing it; in a monorepo subdirectory that includes
  modified files of sibling directories. This is not a jail escape: the
  jail (#247) applies to `add` pathspecs (no absolute paths, `..`, or
  pathspec magic), and the whole git tool is `Risk::Write`, so every call
  — reads included — goes through the approval gate.
- **Injection residual** (above) — fences are a mitigation, not immunity;
  the model must still treat fenced content as data.

## Follow-ups (filed)

- Per-call Destructive classification heuristics (from RC-1) — needs a
  design discussion before code.
- MCP (#74, SHIPPED): stdio client live; trust-model extensions applied
  (metadata never trusted for risk, scrubbed env, fenced results). The
  former "remaining surface" items shipped: HTTP transport + server auth
  are `tole serve --transport mcp` (#96/#137, hardened in #136) — see the
  Server surfaces section below. Resource subscriptions remain unshipped;
  that would re-open this document.

## Server surfaces — `tole serve`, `tole mcp`, `tole acp` (#94–#96, #137)

All three faces share one rule set: non-interactive hosts never gain
interactive powers. Concretely: `Destructive` registration is refused
behind a non-interactive approver (structurally absent from `tole mcp`
and serve; ACP is the exception by design — its approvals are genuine
per-call human decisions routed to the editor, so `delete_file` may
register there). The daemon adds (both transports): mandatory bearer token (refuses to
start without one), per-IP auth-failure rate limiting, a connection
cap (32), IO timeouts (30s: header, auth-header, and request-body
phases on the MCP transport — SSE response streaming is never cut),
and the **jail-of-jails** — a client-supplied session
cwd must canonicalize inside the server workspace root (default the
server cwd, `--workspace` override), or a remote client could jail a
session to `/`. Multi-session MCP routing strips the `session_id` key
before the tool sees its arguments, and a no-id registry call with 2+
open sessions is refused (the #138-documented ambiguity refusal,
implemented 2026-10-05) rather than silently executing against the
server-level registry.

## Skills loading (#161/#162)

SKILL.md files are operator-supplied prompt content, loaded into the
system prompt (`--skill`) or served on demand via the ReadOnly
`load_skill` tool (discovery index only — name + description — until
loaded). The surface is the same as `--system`: whoever controls the
workspace `skills/` dir or `~/.codecora/tole/skills/` controls prompt
content; discovery from a compromised repo is prompt injection by
another name. Frontmatter is validated loudly (name/dir mismatch is a
hard error) and bodies are capped (16 KiB) — but content itself is
trusted by definition, same tier as the system prompt.

## verify_package (#144)

The ReadOnly registry check before any install answers the
slopsquatting class: hallucinated names surface NOT FOUND with registry
candidates, one-character candidates carry a typo-squat warning, and
registry 429/5xx report `rate_limited` honestly (never "not found").
It is advisory to the model — installs still go through the normal
approval gate (`run_command` Write tier).

## Trust presets (#160, env fixed in #166)

`--trust` / `TOLE_TRUST` presets are pure sugar over `--allow` globs —
they widen nothing structurally: Destructive is never auto-allowed,
write-capable native tools keep prompting under `internal`, and unknown
preset names (flag or env) fail loudly. A typo'd env value breaks every
invocation with a clear error — deliberately, per the
typo-silently-narrowing-trust rule.

## Memory loop — decision typing (#143)

The session summary stored by the memory loop keeps its content contract
(first prompt + final answer only — never raw tool output), so the
prompt-injection-into-memory surface is unchanged. What #143 adds is a
TYPE: sessions that executed a Write/Destructive tool are stored with
`--type decision` and a `wrote` tag. The `wrote` bit is derived from the
harness's own tool-risk accounting (durable, session-scoped
`fact.wrote_this_turn` register — set on fresh execution AND
crash-replay, never reset by turn machinery), not from model-asserted
content — the model cannot claim or suppress the decision typing.

## Turn-end stop gates (#145)

`--on-turnend` gates run owner-controlled scripts at the final-message
boundary. Two surfaces are deliberate and bounded: (1) the deny reason is
owner-authored input (the gate script is trusted the same way
`--on-pretool` scripts are — argv-executed, env-scrubbed, timed out), and
it enters the transcript as a user-role entry the model will read; (2) the
final-text preview handed to the gate is capped at 8 KiB and never leaves
the host process. Denials are capped at 3 per turn, so a permanently
failing gate cannot livelock the loop — the turn settles durably as
`StopGateBlocked`, visible to replay.

## Cooperative cancellation (#178/#184/#185)

All three server faces expose cooperative turn cancellation: ACP
clients send `session/cancel`, REST callers `POST /sessions/{id}/cancel`,
MCP clients call `tole_session_cancel`. Every path sets the same
per-session token; a blocking turn observes it at checkpoints (between
provider calls and tool executions) and settles
`stopReason: "cancelled"` as a normal, durable turn end — the session
resumes as usual. Per the ACP and MCP specs the receiver MAY ignore
cancellation for work that cannot be stopped: a single in-flight tool
call (including `run_command`) is not interrupted; the token is
checked before the next one. Unknown session ids 404; cancelling an
idle session is an idempotent no-op. The `tole_session_*` glob in the
`--trust internal` preset covers the new MCP tool.

## Depth-1 child agents (#171/#174)

`agent_start` spawns a child tole session; `agent_poll` reads its
result. Both are `Risk::Write` (#300): a settled poll consumes the
mailbox (uteke forget + `mailbox_consumed` in meta.json), so the tier
matches the side effect and plan mode drops it (plan mode also drops
`agent_start`, so no child exists to poll). `--trust internal`
allowlists `agent_poll` by exact name to keep the poll loop unattended;
`agent_start` still prompts. The registry enforces a structural depth cap (no
grandchildren), results travel via ephemeral uteke mailboxes, and the
parent-only `--agents-worktree` flag gives each child its own git
worktree. A child's prompt is model/operator-supplied — the same trust
tier as the session system prompt. Child sessions are ordinary durable
sessions: approval gates, risk tiers, and secret redaction apply
unchanged inside them.

## `systemone_decide` (#172/#173)

ReadOnly tool, active only when `SYSTEMONE_API_KEY` is set
(`SYSTEMONE_BASE_URL` selects the backend — hosted Jev default,
self-hosted compatible). The decision payload sent to the backend is
model-controlled context: treat the System One backend as an external
data flow. The tool executes no writes; results are advisory input to
the session like any other ReadOnly tool.

### Task-list tools (#198)

`todo_write` mutates only the session's task list, which lives in the
write-once session log itself (the tool's result entry is the record; no
side channel, no file). The threat surface equals any Write tool: prompt
injection could rewrite the plan, but the list is data, never executed —
and every revision is auditable in the replay. `todo_read` is ReadOnly.

### Mission mode (#199)

`tole mission` chains normal durable turns autonomously. The risk frame
is unchanged and deliberate: the same approval gates, risk tiers, and
write-once audit apply per chained turn — autonomy does not widen the
boundary. Budgets bound blast radius in time, steps, and tokens
(#201); exhaustion is resumable, never a dead session. The durable cost
report keeps autonomous spend honest and comparable — an unmeasured
mission is an unaudited one. `--verify` gives the operator a
machine-checkable completion condition stronger than the model's own
claim. Destructive tools remain structurally un-auto-allowable in
missions.

### Remote approver trust boundary (#200)

The `/approvals` decision endpoint is the remote trust boundary: it is
behind the same bearer-token auth + rate limiter as every serve route,
and a decision is a one-shot for exactly one queued effect (fingerprint
of tool + canonical input) — never a blanket allow. Decisions expire to
denied so a lost connection cannot strand a mission, and every decision
writes a durable audit register naming the approval id, tool, and
verdict. The phone/CLI holder is therefore a full approver: treat the
token as approval authority and scope it accordingly.

### Phone as approval surface (#202)

uteke-mobile consumes the #200 queue: the phone becomes a remote
approver. The boundary is the serve token — it carries approval
authority, so device compromise equals write access to the box. Locked
down accordingly: the token is a revocable secret (rotate = serve
restart), decisions are one-shot per (session, effect) with expiry to
denied (a stolen device cannot bank future approvals), every decision
is audited on the session, and there is no push endpoint in tole — the
phone pulls, so the attack surface tole exposes is exactly the
authenticated REST face. Scoped device tokens (per-device, revocable,
read-only vs approver roles) are the recognized follow-up; until then
the deployment guidance is a dedicated OS user + minimal
`--allow`/`--trust` on the serve process.

### Web tools (#215)

`web_fetch`/`web_search` are ReadOnly: results enter context as model
content and are never executed — the exfiltration framing is identical
to any tool output. Fetch is text-only (no JS, no browser), size-capped
(512 KB), content-type allowlisted; search requires an explicit
`TOLE_WEB_SEARCH_URL` backend (probe-first — no keyless scraping). A
crafted page CAN steer a mission via prompt injection; the mitigation
is the same as every other untrusted input: tool-result fencing,
approval gates on anything that matters, and mission budgets bounding
the blast radius.

### Project config trust (#208)

`<cwd>/.tole/config.toml` is untrusted input from a cloned repo: it may
carry `allow`, `trust`, hooks, `skill` and `mcp_server`. Part 2 added the
trust machinery, part 3a wired the startup gate and the low-risk keys, part 3b
applies the security-sensitive ones (`trust`, `allow`, `mcp_server`,
`on_pretool`/`on_posttool`/`on_turnend`, `skill`, `plan_mode`, `no_auto_mcp`,
`no_skills`, `[mission]` `verify`/`verify_timeout`):

- **Content-bound, outside the repo.** The approval is a snapshot of the exact
  file bytes in `$CODECORA_HOME/tole/trusted-configs.json` (default
  `~/.codecora`), keyed by canonical project directory. Any byte change
  (including whitespace or CRLF) makes it untrusted again; the repo cannot
  pre-trust itself. There is no home-less fallback: with no `CODECORA_HOME`
  or `HOME` tole errors instead of writing a store next to the project. The
  home is resolved by ONE shared function (`tole_core::paths`, #344) also used
  for user-global skills and the update-check cache: empty = unset, a relative
  value is refused, and skills / the update check are skipped (never read from
  or written to the cwd) when it is unresolvable.
- **Terminal-safe prompt.** The path, the full content and the diff against the
  previously trusted version are attacker-controlled text shown to a human at
  the decision. Control characters (ESC, CR, NUL, DEL, C1), bidi
  overrides/isolates, line/paragraph separators and zero-width characters are
  printed as visible `\u{hex}` escapes, so the prompt cannot be rewritten.
- **Fail closed.** Without a terminal there is no question: the answer is an
  error naming the file and `tole config trust`. A corrupt, unreadable or
  unsupported-version store is a hard error and is never overwritten.
  `tole config trust` refuses a file that does not validate.
- **Explicit path = intent.** A file named on the command line (`--config`) is
  user intent and does not consult the store. `--no-config` skips discovery,
  the gate and any output.
- **Startup gating.** Every command except `config`, `upgrade` and `approvals`
  runs the gate before anything else (before the update banner, session
  creation or provider access). Only `run`/`chat`/`resume`/`mission` with a
  terminal on stdin AND stderr (and not `--prompt-file -`) may ask; `sessions`,
  `status` and the protocol faces `serve`/`acp`/`mcp` NEVER prompt — their
  stdin/stdout are protocol channels — and fail closed with the exact
  `tole config trust` instruction while the command does not run.
- **Vetted bytes only.** The gate reads the file once; that exact content is
  what gets parsed into the settings. The file is never re-read after vetting,
  so a swap between check and use cannot smuggle in other content.
- **No config, no change.** Without a config file nothing is read, printed or
  looked up (not even the trust store location).
- **Secrets stay out.** The API key and serve token are env-only; the schema
  rejects secret-like keys, and `config check` prints only model/base_url
  values.
- **Sensitive keys apply only after trust.** `allow`, `trust`, the hooks,
  `mcp_server` and `skill` take effect through the SAME single startup path as
  every other key: nothing from the file is read into the settings before the
  gate passed, and `--no-config` drops all of them. A config `allow` / `trust`
  list feeds the very same allowlist machinery as the flags.
- **Destructive is never allowlistable via the config.** `allow = ["*"]`,
  `allow = ["delete_file"]` or a trust preset cannot skip the Destructive
  prompt on the interactive CLI (the approver checks `Destructive` before any
  pattern), and on the non-interactive faces the Destructive tools remain
  structurally unregistered. Tests drive the real binary with such a config and
  assert the file survives.
- **Faces that refuse hooks refuse config hooks too.** `serve`/`acp`/`mcp`
  refuse `--on-pretool`/`--on-posttool` (and `mcp` `--on-turnend`), `mission`
  refuses the hooks, `--plan-mode`, `--memory` and the client-session flags
  (`--skill`, `--no-skills`, `--mcp-server`) on every one of these faces. The
  check runs on the EFFECTIVE (post-config) values, so a safety hook or deny
  policy arriving from the file makes the command fail loudly (with a
  `--no-config` hint) instead of being silently ignored.
- **Replace, never merge; booleans only up.** A higher layer replaces a whole
  list key, so a flag cannot be "extended" by a hostile file; a boolean key is
  `flag || config` (a flag cannot switch a config `true` off, `--no-config`
  can).
- **Unsupported keys are loud.** A build without the `mcp` feature rejects
  `mcp_server`/`no_auto_mcp`, one without `shell-tools` the hook keys and
  `memory`, naming the key and the feature.

Residual risks: a trusted config is trusted — a user who approves a malicious
file at the prompt (or with `tole config trust --yes`, or names it with
`--config`) grants exactly what the equivalent flags could grant (allowlisted
Write tools, hooks that run as the user, MCP servers spawned as the user,
skills injected into the prompt); it is shown in full, but a human decides; the trust store is a plain 0600 file in the
user's home, so anything running as that user can edit it; the store update is
an unlocked read-modify-write, so two concurrent `tole config trust` runs can
lose one of the two records (the loser is simply asked again; the file itself
is replaced atomically and never corrupted).
