# Faces: what every way of running tole has, and why they differ

tole runs a session through several **faces**: the CLI commands (`run`,
`chat`, `resume`, `mission`) and the TUI, `tole serve`, `tole acp`, `tole mcp`
(stdio) and the MCP-over-HTTP host, and cron. The desktop and mobile apps are
faces too, but thin ones: each is a client of `tole serve` and runs no session
of its own (see [The desktop and mobile faces](#the-desktop-and-mobile-faces)).
A capability must not exist on one face only by accident: a desktop client or a
chat gateway talking to `tole serve` should get the agent `tole chat` gives in
the same repository.
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
| `delegate` | yes | no | no | no | drift #994 | Delegation over the delegation tree (#585) is wired for the CLI faces only: `FacePolicy::SERVER_SESSION` is `AgentsPolicy::Classic`, so a server session keeps the depth-1 `agent_start` / `agent_poll` tools. Whether server sessions should delegate is the owner's decision, open in #994. `tole mcp` and cron start no child agents on purpose. |
| `delegate_poll` | yes | no | no | no | drift #994 | The poll half of `delegate`; same reason. |
| `discuss` | yes | no | no | no | drift #994 | The `discuss` tools (#588) are registered only where `delegate` is (same host, same depth limit), so a server session has none. The owner can still list, start, resume and stop a discussion over the `/discussions` routes of `tole serve` (#1042); a model cannot start one from a server session, and a `--plan-mode` server starts none (409). Whether server sessions get the tools is the owner's decision, open in #994. Not offered to `tole mcp` or cron on purpose. |
| `discuss_status` | yes | no | no | no | drift #994 | Same as `discuss`. |
| `discuss_stop` | yes | no | no | no | drift #994 | Same as `discuss`. |
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
| Helper tools for a discussion (#1039: `tole discuss start --tool <server>.<tool>`; the helpers are independent read-only children that may use the owner's named connected tools, each asked once per discussion in the owner's approvals queue) | yes | no (the start has no field and `serve` has no delegation host, so a start is never offered the allowance; a discussion the terminal started with tools can be listed, shown and stopped over serve, but resuming it while open is refused and the terminal's `tole discuss resume` continues it; a stop and a finished discussion need no host) | no (the model-facing `discuss` tool refuses a `tools` field) | no | policy (the allowance and the owner's approvals queue are owner-at-terminal actions, and only the CLI builds the host that starts helper children; a request, a model or a schedule never names a tool for a discussion) |
| Graceful shutdown on SIGTERM and SIGINT (#1014: running turns detached through `detach_running_turns`, a bounded grace, exit 0 when drained) | no (a terminal turn ends with Ctrl+C as before) | yes, `serve` (REST) and the MCP-over-HTTP host share one drain (`serve_support::shutdown`); `tole acp` and `tole mcp` (stdio) end when their parent closes the pipe | n/a | no | policy (only the long-running daemons are stopped by a process manager with a signal; a mission is hosted by `serve` only, so only `serve` cancels one at shutdown) |

## The desktop and mobile faces

Neither app hosts a session. `tole-desktop` (a thin client built on the typed
`tole-client`, #1040) and the mobile app (uteke-mobile, a separate repository)
talk to `tole serve`, so the session they drive is a server session: its tools
and default prompt are the `server-session` column of the table above, and
neither app can add or remove a tool. That is why the parity table has no
column for them; a tool that a desktop or mobile session gained or lost would
show as a `server-session` difference and need a row there. What can differ is
how the app reaches serve and what the app itself does on the owner's machine.
Each difference is on purpose:

| Difference | Desktop | Mobile | Why |
|---|---|---|---|
| Transport and credential | A Unix socket in a `0700` directory (macOS, Linux; the Windows named pipe is #1051) with a per-launch credential for a local serve, or `https://` to a remote one (decision D6; the HTTPS transport is #1044 and is not built yet) | TCP to a serve the owner exposes, with a bearer token or HTTP Basic (`--auth basic`) | Identity and authentication is an allowed difference (table at the top). The routes are the same on every transport; the desktop adds no route only it uses. |
| Starting and stopping the server | Attaches to a matching local serve or starts one as a child; "Quit completely" stops it (owner decisions D3, #1029) | Never; the server is already running on the owner's machine | A phone cannot host the owner's workspace. Serve itself is the same process either way. |
| Choosing a folder with the operating system's dialog | Yes, in the desktop process | No dialog; a folder is one the server already allows | The dialog is a window on the client's own machine. The server only checks that a folder lies inside its workspace (`jail_session_cwd`), so a client can never name one outside it. |
| A file panel that reads the workspace folder directly (`@file`, a file tree) | Only if the desktop is on the same machine as the server, and then read-only display | No | Serve has no file listing, search or read route on purpose (the agent's file tools are jailed to the workspace, and every write goes through the approval gate). A client that reads the folder itself is the owner's own process on the owner's own files and does not go through the jail or the gate, so it must never write there; every write stays an approved tool call. A remote desktop and a phone have no such access. Exposing the workspace tree as a route would be an owner decision (#1042), not part of this row. |
| Push notification of a pending approval | None from tole; the desktop shows the approvals queue while it is open | The app owns its push channel (ntfy, UnifiedPush or FCM) | There is no push endpoint in tole by design (threat model); webhook delivery (#911) is how a gateway is told. |
| Owner actions that are not serve routes | Local only; the desktop shows them but a token holder cannot do them | Not available | See the next section; the rows there hold for both apps. |

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
| Read the workspace folder from the client (a file panel) | the desktop process, on the same machine as the server | Serve has no file route on purpose. A client that reads the folder itself does not go through the file tools' jail or the approval gate for writes, so it is display only and never writes; mobile and a remote desktop cannot do it. |

## Adding or changing a face capability

1. Put the capability in the shared assembly (`session::assemble`, `tool_roster`,
   `turn_runner`), not in one face's code.
2. If it must differ per face, make it a named `FacePolicy` field with the
   reason, and a row here.
3. Run `cargo test -p tole-cli --test face_parity`: it tells you which row is
   missing or stale.

See also: [docs/epics.md](epics.md) (epic #941), [docs/mobile-control.md](mobile-control.md)
(the `serve` surface), [docs/threat-model.md](threat-model.md).
