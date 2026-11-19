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
| Skill sections and `load_skill` (approved skills only, #591) | yes | no | no | no | drift #945, needs the owner's go |
| Review guidance (named only when both review tools are present) | yes | yes (#945) | n/a | no | one path |
| Model routing for a new session (#586) | yes | no | n/a | no | drift #945 |
| MCP client servers (`--mcp-server`, the cora preset) | yes | no | no | no | drift #946 (a trust boundary; the owner's configured servers only, never the request) |
| Plan mode | per process flag | per process flag, or per session with `POST /sessions {"plan_mode": true}` (#947) | per process flag | per job (`read_only`) | one path (a request can only turn it on) |
| Fork, tree, compact, name a session | yes | yes (`serve` routes, #947) | n/a | n/a | one path |
| Mission chaining with a budget | yes | yes (`POST /missions`, #947; a mission is its own session) | n/a | n/a | one path (the verify command, the ceilings and the workspace stay owner configuration) |
| Every turn through `turn_runner` | yes (#944) | yes | n/a | yes | one path |
| Tool roster built by `roster_for` and a `FacePolicy` (#943) | yes | yes | yes | yes | one path |
| The approval gate (`gate::authorize`) | yes | yes | yes | yes | one path |
| Sandbox profile for process tools | yes | yes | yes | yes | one path |

## Adding or changing a face capability

1. Put the capability in the shared assembly (`session::assemble`, `tool_roster`,
   `turn_runner`), not in one face's code.
2. If it must differ per face, make it a named `FacePolicy` field with the
   reason, and a row here.
3. Run `cargo test -p tole-cli --test face_parity`: it tells you which row is
   missing or stale.

See also: [docs/epics.md](epics.md) (epic #941), [docs/mobile-control.md](mobile-control.md)
(the `serve` surface), [docs/threat-model.md](threat-model.md).
