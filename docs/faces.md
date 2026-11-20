# Faces: what every way of running tole has, and why they differ

tole runs a session through several **faces**: the CLI commands (`run`,
`chat`, `resume`, `mission`) and the TUI, `tole serve`, `tole acp`, `tole mcp`
(stdio) and the MCP-over-HTTP host, and cron. A capability must not exist on
one face only by accident: a desktop client or a chat gateway talking to
`tole serve` should get the agent `tole chat` gives in the same repository.
This page is the record of what differs on purpose. It exists because the
drift happened before (#536 closed after only `mission` got the review
guidance); epic #941 moves the assembly into one place and this page, with the
test that reads it, keeps it there.

## The only allowed differences

A face may differ from another in exactly these ways. Anything else is either
a documented row below or a bug.

| Difference | Why it is allowed |
|---|---|
| **Identity and authentication** | The CLI runs as the owner at a terminal. `serve` and `mcp-http` authenticate a bearer token. `acp` and `mcp` trust the parent process that spawned them. Cron runs as the owner without anyone present. |
| **Input and output format** | A terminal line, a JSON route, an ACP message, an MCP tool call, a scheduled prompt and its delivered summary. The turn runner underneath is the same (#944). |
| **Approval delivery channel** | The interactive prompt (CLI, ACP editor), the approvals queue answered over a route (`serve`), a pre-authorized allowlist (`tole mcp`, cron), or a delegated approver for a child agent. The gate itself, `gate::authorize`, is one function on every face. |

## What the test enforces

`crates/tole-cli/tests/face_parity.rs` builds a session through each face with
the same inputs (the CLI, `tole mcp`, a real `serve`/ACP session from the
session host, and a cron run) and compares the sorted tool names and the text of the
default system prompt (modulo its one context line). **Every difference must be a row of the table
below**, with the face cells matching what was built, a status (`policy` or
`drift #N`) and a reason. A face that gains or loses a tool or a prompt without
a row fails the build; so does a row that no longer describes a difference. To
add a capability to one face only, add the row and say why.

`status` is `policy` when the difference is intended, and `drift #N` when it is
a known gap that issue N is closing or deciding. A `drift` row is removed when
the issue lands, and the test then fails until it is.

<!-- parity:begin -->
| tool | cli | mcp-server | server-session | cron | status | reason |
|---|---|---|---|---|---|---|
| `delegate` | yes | no | no | no | drift #945 | Delegation over the delegation tree (#585) is wired for the CLI faces only; server sessions keep the classic depth-1 agent tools. `tole mcp` and cron start no child agents on purpose. |
| `delegate_poll` | yes | no | no | no | drift #945 | The poll half of `delegate`; same reason. |
| `discuss` | yes | no | no | no | drift #945 | The owner-started model discussions (#588) are a CLI session tool today; whether server sessions get them is decided with the other session capabilities. Not offered to `tole mcp` or cron on purpose. |
| `discuss_status` | yes | no | no | no | drift #945 | Same as `discuss`. |
| `discuss_stop` | yes | no | no | no | drift #945 | Same as `discuss`. |
| `agent_start` | yes | no | yes | no | policy | The classic depth-1 child agent tools belong to a session. `tole mcp` is a stateless tool server with no session, and a scheduled run starts no children. |
| `agent_poll` | yes | no | yes | no | policy | The poll half of `agent_start`; same reason. |
| `cron_create` | yes | no | yes | no | policy | A session may schedule work for later. `tole mcp` has no session, and a scheduled run must not create schedules (no unattended recursion). |
| `skill_propose` | yes | no | yes | no | policy | A skill draft needs a session as its provenance (#592). `tole mcp` has none, and an unattended run does not draft skills for the owner to find later. |
| `todo_read` | yes | no | yes | yes | policy | The task list is a session record (#198). `tole mcp` serves files and commands, not a session. |
| `todo_write` | yes | no | yes | yes | policy | The write half of `todo_read`; same reason. |
<!-- parity:end -->

## Capabilities the test cannot see in a tool list

These are part of the same epic. They are not tools, so a person keeps them
true; the status column says where the work is.

| Capability | CLI | `serve` / ACP session | `tole mcp` | cron | Status |
|---|---|---|---|---|---|
| Default system prompt (tool discipline, one context line naming the session's workspace; pinned in the header) | yes | yes (#945) | n/a | cron's own | one path |
| Skill sections and `load_skill` (approved skills only, #591; discovered in the session workspace; the owner's `--skill`/`--no-skills` for the process; appended to the pinned prompt at creation, so `resume` keeps it) | yes | yes (#945) | no | no | one path |
| Review guidance (named only when both review tools are present) | yes | yes (#945), and active when the owner has configured the cora review tools as an approved MCP server (#946); otherwise absent | n/a | no | one path in code |
| Model routing for a new session (#586) | yes | yes (#945: routed once, at the session's first prompt, from the owner's `[router]` config; a session that already has a model, a resumed one included, is left alone; ACP and the MCP session tools share the path) | n/a | its own (cron routes by job) | one path |
| MCP client servers (`--mcp-server`, the cora preset) | yes (flag, user or project config, and the cora preset when `cora` is on the PATH) | only the OWNER's list (`tole serve` / `tole acp` `--mcp-server` or the user config, read at start-up; a project-config `mcp_server` is refused at start-up; no PATH preset; never a request field), through the same plugin lock and sandbox rules, on `serve` (unattended) an `optional` sandbox counts as `required`, on `acp` (attended, like the CLI) it starts the server unsandboxed with a warning and a ledger fallback line; every tool `Write` through the approval gate; a refused server leaves one notice in the session log (#946) | no | no | policy (a trust boundary: the owner names the servers, a client never does; `tole mcp`, `POST /missions` and cron attach none) |
| Plan mode | per process flag | per process flag, or per session with `POST /sessions {"plan_mode": true}` (`serve` only, #947; ACP keeps the process flag) | per process flag | per job (`read_only`) | one path for `serve` (a request can only turn it on) |
| Fork, tree, compact, name a session | yes | yes (`serve` only, #947; ACP has none of these routes) | n/a | n/a | one path for `serve` |
| Mission chaining with a budget | yes | yes (`serve` only, `POST /missions`, #947; a mission is its own session, one at a time, refused under `--plan-mode`) | n/a | n/a | one path for `serve` (the verify command, the budget ceilings and the workspace are owner configuration; a request may only lower a budget) |
| The `turn_finished` outbound webhook event (#962; ids and a status word, opt-in per destination) | no | yes (`serve` only: the serve prompt route and the approval-resume thread; ACP and `tole mcp` have no webhook sink) | no | no (a cron run has `cron_run_finished`) | policy |
| Every turn through `turn_runner` | yes (#944) | yes | n/a | yes | one path |
| Tool roster built by `roster_for` and a `FacePolicy` (#943) | yes | yes | yes | yes | one path |
| The approval gate (`gate::authorize`) | yes | yes | yes | yes | one path |
| The action summary of an approval (#1041; built by `ToolRegistry::decide`, so every face's request carries it) | TUI prompt shows it; the stdio prompt (the plain-terminal CLI paths) still prints the tool's `describe` text | `serve` shows it (`GET /approvals`, the `approval_required` stop, the approval events); the ACP permission `title` still uses `describe`; a delegated child's `dap-` approval and the webhook events carry none | n/a (pre-authorized allowlist, no prompt) | n/a (a parked run is answered through the `serve` queue) | drift #1041 follow-up: each of these is a prompt or row that does not render the summary yet, because it is a different presentation (a one-line ACP title, the delegation queue's own row, a fixed webhook payload). The summary is already on the request, so closing it is a rendering change, not new derivation |
| Sandbox profile for process tools | yes | yes | yes | yes | one path |

## Owner actions that are not serve routes

`tole serve` is also the interface of the desktop and of mobile clients (#1042), so every read a person needs and
every action that only spends the owner's own budget is a route. What changes who is trusted or what is allowed
stays with the owner at a terminal, and has no route on purpose. Each row is a decision of the owner (desktop
design, section 0) or a design rule, not a gap:

| Action | Where it lives | Why it has no serve route |
|---|---|---|
| Write the user config or a profile | the owner's editor, `tole config` | A profile carries the allow list and the secret grants. A token holder must not widen them (design rule: authority is the owner's file). |
| Approve, revoke or re-approve a skill or a connected tool (`tole skills approve`, `tole plugin approve`) | `tole skills`, `tole plugin` at a terminal | Trust approval is never a serve route; whether the desktop may approve at all is the owner's pending decision (#1034). Serve only reads the trust state. |
| Change the allow list | the owner's config and `--allow` | An approval answers one call; the standing allow list is configuration. |
| Create or edit a cron job | `tole cron create`, `tole cron edit` | A job carries a prompt, an allow list and, for a script job, a command run with the owner's authority. Stays local-only unless the owner decides otherwise (#1042). |
| Choose a folder with the operating system's dialog | the desktop process | It is a window on the client's machine; the server only checks that a folder is inside its workspace. |

## Adding or changing a face capability

1. Put the capability in the shared assembly (`session::assemble`, `tool_roster`,
   `turn_runner`), not in one face's code.
2. If it must differ per face, make it a named `FacePolicy` field with the
   reason, and a row here.
3. Run `cargo test -p tole-cli --test face_parity`: it tells you which row is
   missing or stale.

See also: [docs/epics.md](epics.md) (epic #941), [docs/mobile-control.md](mobile-control.md)
(the `serve` surface), [docs/threat-model.md](threat-model.md).
