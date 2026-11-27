# Mobile control — driving missions from uteke-mobile (issue #202)

The 0.8.0 remote-approval surface (#200) is the entire backend a phone
needs: missions run on the box under `tole serve`, the phone lists
running missions, receives pending Write approvals, and approves/denies
from anywhere. The phone is a client of the token-authenticated serve face.
(Graphical clients need more than polling: a transcript read, resume-by-id and an
event stream ship with #524, image upload and read with #558; see the UI epic #523.)

## The surface uteke-mobile consumes

All routes live under `tole serve` (bearer-token auth, or HTTP Basic with
`--auth basic` (#742, "HTTP Basic auth" below), on everything except `/health`;
decisions revalidate per request; auth failures are rate-limited per source IP).

| Action | Route | Notes |
|---|---|---|
| Probe the server | `GET /info` (#1024) | `200 {"version", "protocolVersion", "features"}`: the binary version, the wire contract version and the feature names this server has, see [`GET /info`](#get-info). Behind the token, like every route but `/health` |
| List sessions | `GET /sessions` | id + busy per session (+ `profile` when it has one, #740) |
| Start a mission | `POST /sessions` `{cwd}` → `POST /sessions/{id}/prompt` `{text}` | the prompt is the mission goal (`tole mission` semantics chain turns; a serve prompt runs ONE turn; for chained turns use `POST /missions` below, #947) |
| Mission status | `GET /sessions/{id}/status` | busy flag, entry count, model, effort, context window, name (#573), `usage_report` (tokens, cache-hit rate, request-size split; #211), mission cost report (`fact/mission` register, #201) |
| **Pending approvals** | `GET /approvals` | queue entries: id, session, tool, status, and a `summary` (below); expires to denied after 15 min |
| **Approve / deny** | `POST /approvals/{id}/decision` `{"decision":"allow"\|"deny"}` | allow = one-shot for exactly that (session, tool+input) + automatic resume; deny = recorded verdict. Only the first decision counts: the same verdict again answers `200` with `"resuming":false` and audits and resumes nothing; the opposite verdict, or any decision on an entry that already expired, answers `409` with the current `status`. An allow for a session this server no longer hosts (evicted from the 256-session live map, or the server restarted) with no cron run parked on it answers `409` with `"resuming":false` and an `error` saying the decision is recorded but nothing was resumed (reopen with `POST /sessions {"resume":"<id>"}` and send a new prompt); it never answers `202`. A session with a pending approval is never evicted, and eviction takes the least recently used idle session |
| **Delegated children's approvals** | `GET /approvals` (entries with `source: "delegation"`), `POST /approvals/dap-…/decision` `{"decision":"allow","confirm":"<the same id>"\|"deny","fingerprint"?}` (#714) | the durable queue of delegated children, next to the list above: id, goal, node, tool, target, redacted `display`, age, seconds left, `fingerprint`, the `chain` of levels, and `text` (the whole request fenced as data, as `tole delegation approvals show` prints it). The same bearer token as every route. `allow` must repeat the id as `confirm` (no id, prefix or pattern means "all": `400`); only a pending request is decided (`409` with the reason for an expired, consumed, already decided or settled one, `404` unknown); the decision is bound to the request's fingerprint and recorded `via: serve`; the server refuses (`403`) when it runs under an agent session. No session is resumed (the child is already waiting). Requests exist only when `[delegation] approvals` is on |
| Prompt stops for an approval | `POST /sessions/{id}/prompt` answers `200 {"stopReason":"approval_required","approval":{"id","tool","status":"pending",…}}` (#571) | `approval` is the `GET /approvals` row (summary included) (`description` is sanitized to one line but not length-capped, as there) of the entry THIS turn queued for this session, found by a queue snapshot taken before the turn; its `id` is what `POST /approvals/{id}/decision` takes. If that entry expired or was decided before the lookup, `approval` is omitted: use `GET /approvals?session=<id>`. A stop that queued nothing (a pre-tool hook denial, a blocked stop gate, an unknown tool, a provider failure, a loop) stays `refusal` (its `problem` says which, see [Problem of a stopped turn](#problem-of-a-stopped-turn-1023)); ACP and the MCP session tool keep their own vocabulary (`refusal`) |
| Prompt on an interrupted turn | `POST /sessions/{id}/prompt` answers `409 {"error":"interrupted turn","state":"planning"}` (#1230) | The session's log ends in a turn that never settled, because the server died while the turn was in flight (a kill, a crash, an OOM kill): the user message is durable, the answer is not. `state` is the register (`planning` after a model call was cut off; `tool_call`, `awaiting_approval`, `executing` or `settling` for a cut-off tool step). The `409` is what a session answers until a resume has settled it: `POST /sessions {"resume":"<id>"}` settles it (see [Resume by id](#resume-by-id-524)), after which the prompt is accepted. Nothing is written for a refused prompt. It is a defined state, not a server error: do not retry it, resume. A turn parked at an approval or after a denial is not this state: the next prompt is accepted. The `409` of a running turn keeps its own body (`session is busy running a turn` on `POST /sessions/{id}/prompt`, no `state`) |
| Cancel a turn | `POST /sessions/{id}/cancel` (#178) | the turn settles `cancelled`, durably |
| Steer a turn | `POST /sessions/{id}/interrupt` `{text, urgent?}` (#377) | `202` queued; delivered at the turn's next safe point (after the running tool, before the next model call) as an attributed user-role entry, nothing is lost. `urgent` also withdraws a tool call that has not started. `409` when no turn is running, `400` for blank/oversized text or a full queue (16 pending) |
| Detach a turn | `POST /sessions/{id}/detach` (#217) | `202`; the running tool settles, the turn ends `detached` (resumable) and background jobs keep running, nudging the session when they finish. `409` when no turn is running |
| Follow a discussion | `GET /discussions/{id}/events?after=<seq>` (#1094) | Server-Sent Events: a small `changed` signal per ledger step, see [Following a discussion](#following-a-discussion-1094) |
| Live events | `GET /sessions/{id}/events?after=<seq>` (#524) | Server-Sent Events, see [Event stream](#event-stream-524); replays the log from a `seq`, then follows it |
| Start in plan mode | `POST /sessions` `{cwd, "plan_mode": true}` (#947) | the same registry filter as `--plan-mode`: mutation tools (write, run_command, agent_start, todo_write, ...) are ABSENT from the session's tool list, not asked about. Answers `planMode: true`. A request can only turn it on: `false` or absence leaves the default, and a `tole serve --plan-mode` process keeps every session in plan mode. `400` for a non-boolean, or combined with `resume` |
| Start with a profile | `POST /sessions` `{cwd, "profile": "<name>"}` (#740) | persona, allow list, memory room and model of a user-config profile, see [Session profiles](#session-profiles-740) |
| List past sessions | `GET /sessions/index?limit=<n>&cursor=<c>` (#572) | the sessions of this server's workspace that are not necessarily live, newest first, metadata only; see [Past sessions](#past-sessions-572) |
| Resume a session | `POST /sessions` `{"resume": "<id>"}` (#524) | reopen an existing session, see [Resume by id](#resume-by-id-524) |
| Read the transcript | `GET /sessions/{id}/entries?after=<seq>&limit=<n>` (#524) | one page of the session's log, see [Transcript read](#transcript-read-524); works while a turn runs |
| Upload an image | `POST /sessions/{id}/images` (raw body, #558) | returns `{id, mime, bytes, fnv1a64}`; see [Images](#images-558) |
| Prompt with images | `POST /sessions/{id}/prompt` `{text, images: [<id>]}` (#558) | the turn records the references, never the blob |
| Read an image | `GET /sessions/{id}/images/{fnv1a64}` (#558) | only images recorded in that session's log |
| Cost report | `GET /sessions/{id}/status` | the `mission` register: turns, steps, tokens, wall time, per-risk-tier tool calls |
| Fork a session | `POST /sessions/{id}/fork` `{"at"?: <seq>}` (#947) | `200 {"id", "forkedFrom", "at", "seq"}`: a NEW session file in the same sessions directory, the lineage up to the settled entry whose `seq` (as `GET …/entries` shows it) is `at`, default the last settled one; same `JsonlStorage::fork` as the TUI `/fork` and `tole branch`, and the source is never touched. The fork is not made live: `POST /sessions {"resume": "<id>"}` opens it. `404` unknown session, `409` while a turn runs, `400` for a bad `at`, an unknown seq or an entry that is not a settled final assistant message |
| Session family tree | `GET /sessions/{id}/tree` (#947) | `200 {"id", "tree": [{"id", "parent", "at", "state", "turns", "depth", "current"}]}`: the session's parent chain up to the root and every fork below it, depth-first (`depth` 0 = the root), the same reader as `tole sessions --tree` and the TUI `/tree`. Read from the session files, so it answers while a turn runs. `404` unknown session |
| Request a compaction | `POST /sessions/{id}/compact` (#947) | `200 {"id", "compaction": "requested"}`: the TUI `/compact` (the forced pass of #481). It records the request in the log; the session's NEXT prompt compacts first, when compaction is enabled. No model call runs in the route, and there are no instructions field. `404` unknown session, `409` while a turn runs |
| Rename a session | `POST /sessions/{id}/name` `{"name": "<label>"}` (#947) | `200 {"id", "name"}` (the trimmed label): the TUI `/name` (the `fact/session_name` register). `400` for a missing, blank, control-character or over-120-character name, `404` unknown session, `409` while a turn runs |
| List pickable models | `GET /models` (#574) | `200 {"models": ["<id>", ...], "default": "<id>" \| null}`: the models a client may pick, decided by the OWNER: the configured default model (always first, so a client can switch back) plus the ids of `TOLE_MODELS` (comma separated, the variable that advertises the ACP model picker; read once at start-up). With no `TOLE_MODELS` the default is the only entry. The provider's `/models` is not consulted: listing it would let the token holder spend on anything the key reaches |
| Set the session model | `POST /sessions/{id}/model` `{"model": "<id>"}` (#574) | `200 {"id", "model"}`: writes the `fact/model` register the TUI `/model` writes (through `session_settings`), so it survives `resume`. The request carries an id only, never a provider URL or key: the session keeps the server's provider configuration. `400` for a missing, blank or control-character id and for an id that is not in `GET /models` (exact match), `404` unknown session, `409` while a turn runs |
| Set the thinking level | `POST /sessions/{id}/effort` `{"effort": "minimal" \| "low" \| "medium" \| "high" \| null}` (#574) | `200 {"id", "effort"}` (canonical level, or `null`): writes or clears `fact/reasoning_effort` as the TUI `/thinking` and `--effort` do; `null` (or `"unset"`) goes back to the default. An invalid level is a `400`, never coerced; `404` unknown session, `409` while a turn runs |
| Start a mission | `POST /missions` `{"goal", "max_steps"?, "max_minutes"?, "max_tokens"?}` (#947) | `202 {"id", "status": "running"}`. A mission is its own session (its own pinned mission prompt and writer lock, as `tole mission` makes one), so it is NOT under `/sessions/{id}`; it runs the CLI's own loop (`mission::run_mission_outcome`) on a background thread over the serve workspace root. The `fact/mission` summary and the session file are the ones the CLI writes; `POST /sessions {"resume": "<id>"}` opens the session after it ends. The owner's configuration, never the request, decides everything else: the **verify command** and its timeout (`[mission] verify`, `verify_timeout` of the user config, read at start-up like `tole mission`), the budget **ceilings** (the `tole mission` tier, trusted tier or config `max_*`; a request may ask for less, a value above is a `400`), the workspace, the allow list and the sandbox. A body with a `verify` key, or any key besides the four above, is a `400` and starts nothing. Write approvals queue in the same `GET /approvals` queue under the mission id (the mission stops at the first, as the CLI does). `409` while another mission runs (one at a time) and on a `--plan-mode` server, `400` for a bad body |
| Read a mission | `GET /missions/{id}` (#947) | `200 {"id", "status", "running", ...}`: `status` is `running` or how it ended (the CLI's: `complete`, `exhausted_steps`, `exhausted_tokens`, `exhausted_time`, `exhausted_turn_cap` (3 turns in a row ended at the 32-step per-turn cap, #1239; one such turn alone does not end a mission), `verify_failed`, `verify_refused`, `stopped` for an approval stop, a cancel or a provider failure, `error`); once ended also `turns`, `steps`, `tokens`, `wallSeconds` and `summary` (the `fact/mission` register), and `error` when the run could not start. A mission of an earlier server process is read from its session file. `404` unknown |
| Cancel a mission | `POST /missions/{id}/cancel` (#947) | `202 {"id", "cancelled": true}`; the running turn is cancelled and the mission ends `stopped` with its durable summary. `404` unknown, `409` when it has already ended |

The round trip the app must support (the cross-repo acceptance):

0. A prompt that stops on a Write answers `stopReason: "approval_required"` with the
   pending row (#571): show the panel from that row, no poll needed. A client that was
   offline when the request was queued still finds it with step 1.
   **Resumed turns have no such answer.** After `POST /approvals/{id}/decision` allow
   (`202`), the resume runs in a server thread with no HTTP response and no observer, so
   when it stops on a further approval nothing says `approval_required` for it: watch the
   `approval_requested` frames (ephemeral, not replayed) or poll
   `GET /approvals?session=<id>`. A gateway should poll `GET /approvals?session=<id>` after
   every allow until push notifications exist (#911).
1. `GET /approvals` on a poll interval (the push-channel decision —
   ntfy/UnifiedPush vs FCM — belongs to uteke-mobile; tole exposes no
   push endpoint by design, see the threat model).
2. Render the pending Write (tool + description + session).
3. `POST /approvals/{id}/decision` — allow or deny.
4. `202` (allow) means the resume is running; `GET /sessions/{id}/status`
   until `busy` clears.

## Percent-encoding of ids

A client sends every path segment and query value percent-encoded (`tole-client` leaves `A-Za-z0-9-_.~` and writes
every other byte as `%XX`), and the server decodes the ones it reads as an id or a name: the session id of
`/sessions/{id}/...`, the approval id, the mission, discussion and cron job ids, the image hash, and the `job` of
`GET /cron/runs?job=<id or name>` and the `session` of `GET /approvals?session=<id>`. The path is split on the raw `/`
and `?` first and the query on `&`, so an encoded `/`, `?` or `&` is part of one value and never a new segment,
parameter or route. A `%` without two hex digits, or bytes that are not UTF-8, is a `400`. A value that decodes to a
`/`, a NUL or a control character is not decoded (it names nothing: the usual `404`). Decoding happens once
(`%252F` is the text `%2F`) and `+` is a plus, not a space: encode a space as `%20`. The decoded value meets the
same rules as before (a session id is `[A-Za-z0-9_-]`, 1 to 64), so an escaped form of a valid id is that id and
nothing else becomes valid. `/hooks/<route>` is not part of this: it is for webhook senders, matched as written.

## Approval summary (#1041)

Every approval row (`GET /approvals`, the `approval` of an `approval_required` stop, the `approval_requested` /
`approval_resolved` events; the TUI prompt shows its one-line sentence) carries the same structured, code-derived `summary` of what the call
will do, so a client does not word it from the tool name and arguments. It is built where the request is created (the
tool's own `action_summary`, called by the registry before the approver is asked), from the tool and its input only.
`kind`, `effect`, `insideWorkspace` and the English wording come from code; `target` and `preview` carry strings the
model sent (a path, a command, a profile name, the arguments) and are shown as data, sanitized and redacted, not
validated. The summary never changes the risk tier: the tier stays the authority.

```json
"summary": {
  "kind": "create_file",
  "target": "Reports/Weekly report.txt",
  "effect": "adds",
  "insideWorkspace": true,
  "targetTruncated": false,
  "preview": ["+ Done", "+ Next"],
  "previewTruncated": false
}
```

| Field | Meaning |
|---|---|
| `kind` | `create_file`, `replace_file`, `delete_file`, `run_program`, `use_connected_tool`, `start_discussion`, `delegate` or `unknown`. A client treats a code it does not know as `unknown`. |
| `target` | A path relative to the working folder (symlinks resolved; for `delete_file` of a symlink, the link as sent), a command line (tokens re-quoted exactly as `describe` shows them, so `printf 'a b'` differs from `printf a b`; a line that does not split reads `<unparsable command: ...>` followed by the line), `<server>.<tool>` for a connected tool, `profile <name> (requested by the model)` or the participant names for a delegation or discussion, or the tool name for `unknown`. One safe line, at most 1000 characters. |
| `effect` | `adds`, `replaces_no_backup`, `cannot_be_undone`, `runs_program`, `uses_connected_tool`, `starts_discussion`, `starts_child` or `unknown`. |
| `insideWorkspace` | `true` only when the tool confirmed the target resolves inside the working folder. Always `false` for programs, connected tools and a path that escapes. |
| `targetTruncated` | `true` when `target` was cut. |
| `preview` | At most 12 lines of at most 400 characters: `-` / `+` lines for a file change (a removed block cannot push the new text out: with both sides present each gets half of the lines), `$ command` for a program, the policy lines (child allowlist, worktree, mode, budget, rounds, each marked "requested by the model" when it is the model's request) then the task or question for a delegation or discussion, the raw arguments for a connected tool or an unknown tool. Raw data, not wording. |
| `previewTruncated` | `true` when `preview` is not all of it. |

A cut keeps the head AND the tail and names the omitted amount inside the text (`... (N more characters, see the full
description) ...`; a last line `... (N more lines, ...) ...` when lines were dropped; `+N more entries` for a list), and
sets `targetTruncated` / `previewTruncated`. **The whole text is in the row's `description`**: it is the tool's own
description (what the tool says about this call: the full command, the resolved repository, the allowlist), redacted
with the secret set but never cut or bounded, and the summary is the bounded view of it. A client that shows a
truncated summary should show `description` too, or offer it. Lists the human grants (a child's allowlist, the
participants of a discussion) get ONE preview line per entry up to 6, then a `+N more entries` line, so padding a list
cannot push an entry into a hidden middle; a budget shows its `tokens` / `steps` / `minutes` and only counts other keys.

The English wording is a pair of tables keyed by `kind` and `effect` (`tole_core::action_summary::KIND_TEXT`,
`EFFECT_TEXT`), so a client translates by code and never parses a sentence. Rules a client can rely on:

- **What is shown is what runs.** Every field of the `agent_start`, `delegate` and `discuss` summaries is parsed by the
  code their `execute` runs, not by a second reading of the input: the allowlist is the array's string entries (a lone
  string or non-string entries are no request, so the child inherits and the summary says `child allowlist (inherited
  from you)` with the parent's own list), a profile's allowlist is the profile's own and cannot be read by the summary
  (`the profile's own list`; an `allow` next to a profile reads `REFUSED`), the task is the first string of `task` /
  `prompt`, and the budget shows only the `tokens` / `steps` / `minutes` that are unsigned integers. A path a file tool
  would refuse (outside the working folder, not found) is `kind: "unknown"` with `refused ...: <path>` as the detail,
  never "creates a new file"; a resolved path whose name is not valid UTF-8 is shown lossy and marked, not refused.
- **What the summary does not show.** It shows what the model asked for and the allowlist that results, not everything a
  child is given: `discuss` `modes` (a `reviewer` participant is a read-only consult child session with workspace access,
  an inline one holds no tools; the summary lists only the participant names), the harness-owned tools `agent_start` adds
  to a child (`handoff_submit` for a `role` child, `delegate_report` for a delegated one: each writes one bounded file at a
  path the harness chose), the parent's deny patterns appended to a child's list when `allow` is given (they only narrow
  it), and, for `agent_start`, `role`, `consumes` and `keep_mailbox` (`delegate` shows `role`). None of these adds an allow
  the shown list lacks. The `discuss` summary re-reads its own fields instead of sharing a parser with `discuss`, which
  refuses a malformed input. `describe`, and so the row's `description`, is not cut for the task, the question or a
  connected tool's arguments, which is why the summary's cut markers can point at it.
- **Unknown tool.** A tool without a summary of its own (`git`, `gh`, `gitea`, `cron_create`, ...) is `kind: "unknown"`,
  `effect: "unknown"`, `target` the tool name, the tool's own description as the first preview line (`the tool's own
  description: ...`, so the resolved repository or the exact argv is there) and the raw input after it: "use the tool
  <name>", never a reassuring sentence.
- **Connected (MCP) tools.** Always `use_connected_tool` with `<server>.<tool>`, and the tier is `Write`. The server's
  description and any other metadata are not used.
- **Time of the check.** `create_file` / `replace_file` is decided when the request is made. The serve queue holds an
  entry for up to 15 minutes, and the file can appear or vanish before an allow; the wording says "did not exist when
  asked" for that reason, and the replayed call does what the tool does at that later time.
- **Secrets.** Target and preview are redacted with the process secret set as one text BEFORE they are split into
  lines and cut (a secret on the cut or spanning lines leaves no prefix), and again with the `web_fetch` header
  secrets when the row is served.
- **Older rows.** The field is additive: a row queued without a summary has no `summary` key, and a summary missing the
  optional fields (`insideWorkspace`, `targetTruncated`, `preview`, `previewTruncated`) still loads with their defaults.
- **The TUI prompt** is the summary's one-line sentence (what the call does and its effect), then ` :: ` and the tool's whole text character for character as before (`<tool> — <description> → <target>`), uncut; it does not show the preview lines or the flags; `job_start` reads `start job: ...` there and its summary says it STARTS A BACKGROUND JOB.
- **ACP.** The `title` of a `session/request_permission` is the summary as one line (`<what it does> — <its effect>`, plus the tool's own text for a tool without a summary of its own): control and bidi characters are made visible, and the target is shown as the summary bounds it (at most 1000 characters, a longer one cut in the middle with the true omitted count and `see rawInput`). The tool's own text of an unknown-kind call is bounded the same way and, only when the whole title would pass 1500 characters, its tail is cut with a count; the effect sentence is never cut. Options, tool-call id and risk are unchanged.
- **The stdio prompt** (the plain-terminal CLI paths) prints the sentence on a `does:` line between `tool:` and `what:`, which still holds the tool's whole text; every printed value is neutralized to one line; a request without a summary has no `does:` line.
- **Where it is not shown yet.** The delegated children's approvals
  (`source: "delegation"`, which keep their own `display` and `text`) and the webhook events still use the older text
  and carry no `summary`; see `docs/faces.md`.

## Problem of a stopped turn (#1023)

`POST /sessions/{id}/prompt` answers a turn that did not end with an answer with a `problem` object
next to the fields it always had. A client tells "the provider failed" from "a loop was detected" by
`problem.kind`, without parsing English. `stopReason`, `text` and `approval` keep their values and
meaning (an approval stop is still `approval_required`, every other stop still `refusal`,
`max_tokens`, `cancelled` or `detached` as before); `problem` is added, and is absent from a final
answer (`end_turn`) and from a cancel or a detach.

```json
{"stopReason": "refusal",
 "problem": {"kind": "provider", "subject": null, "retryable": true,
             "detail": "http status: 429 Too Many Requests — slow down"}}
```

| Field | Meaning |
|---|---|
| `kind` | `provider` (the provider failed after its retries), `storage`, `loop` (the loop guard tripped), `unknown_tool`, `stop_gate` (a turn-end gate denied the answer too often), `approval` (the turn stopped on a call that needs an approval), `internal`, `budget` (the step budget ran out). An open vocabulary: a client treats a kind it does not know as a generic problem. |
| `subject` | What it is about: the tool of a `loop`, `unknown_tool` or `approval`; `null` otherwise. Always present. Redacted and at most 256 bytes. |
| `retryable` | `true` for a provider failure (the provider failed after its retries, so the same prompt MAY succeed again with nobody changing anything: a rate limit, an outage; a permanent refusal such as a 400 is also `true` in this first cut, a client must not retry in a loop); `false` when a decision, a fix or another prompt is needed first. |
| `detail` | Optional. The technical text for a "show details" disclosure (a provider message, a gate's output). **Redacted with the session's secrets** (the process secret set: environment secrets, the provider key, webhook and serve credentials, `web_fetch` header values) and THEN cut to 4096 bytes, so a cut never leaves part of a secret behind. Data, not wording: never parse it. |

There is no wording in the answer: a client words a problem from `kind` and `subject`. Not covered
yet: a turn that fails with a storage error still answers `500 {"error": "<text>"}` without a
`problem`, the answer of the approval-resume thread (`202`, no turn outcome is reported there) and
the event stream (no frame carries the outcome of a turn). The ACP face carries the same object in
`result._meta.tole.problem`; `stopReason` there stays in the fixed ACP vocabulary
(`end_turn`, `max_tokens`, `refusal`, `cancelled`), and the `_meta` key is the extension point the
ACP schema reserves on every type.

## Status JSON fields

`GET /sessions/{id}/status` returns `{id, entries, busy, usage_report}` plus
the session-status fields below (#573). `entries` and `usage_report` are `null`
while a turn is in flight (the session storage is busy). `usage_report` was
added by #211 and the status fields by #573; both are purely additive: clients
must ignore unknown fields (the original keys keep their meaning).

| Field | Meaning |
|---|---|
| `model` | the session's pinned model (`fact/model`), else the configured default; `null` if neither |
| `effort` | the session's `fact/reasoning_effort` register; `null` while unset (the process default is not reported) |
| `contextWindow` | tokens: the configured window (`TOLE_CONTEXT_WINDOW` / config `context_window`), else the one the provider's `/models` reported for the session's model; `null` when unknown |
| `contextUsed` | prompt tokens of the last model call since the last compaction (`0` before the first call); with `contextWindow` it is the footer's `used / window` |
| `name` | `fact/session_name` (`POST /sessions/{id}/name`), else the name the session was created with; `null` if none |
| `snapshotAt` | epoch milliseconds when these values were read; `null` if there is no snapshot yet |
| `stale` | `false`: read live from an idle session. `true`: a turn holds the session, so the values are the last-known snapshot, not a live read |

These are the values `tole status` and the TUI footer show for the same
session (the same snapshot code reads them), so the faces cannot disagree. An
unknown value is `null`, never a made-up one. The snapshot is refreshed when
the session opens, when a turn settles, when a register changes (`model`,
`effort`, `name`, `compact`) and on every idle status read; the route never
waits for a running turn, so it answers promptly while one runs. During a turn
the snapshot is the state before it began (a model switch is refused while a
turn runs, so `model` and `effort` cannot be older than the turn). The context
window is looked up on the provider off the request path (after the session
opens or its first turn settles) and cached per model, so a window the
provider cannot report stays `null` rather than delaying the route. The
`usage_report` shape — every unknown value is `null`, never a fabricated 0:

| Field | Meaning |
|---|---|
| `steps` | provider steps that reported usage (the same count the mission step budget uses; rows holding only `tole_wire` are not counted) |
| `prompt_tokens`, `completion_tokens` | sums over the usage ledger |
| `reasoning_tokens` | sum of `completion_tokens_details.reasoning_tokens`; `null` if no step reported it |
| `cached_tokens` | sum of cached prompt tokens (`prompt_tokens_details.cached_tokens`, falling back to `cached_read_tokens` and the other gateway spellings); `null` if no step reported it |
| `cache_hit_rate` | cached / prompt over the steps that reported cached tokens; `null` if unknown |
| `wire` | `null` for sessions recorded before #211, else `{first, last, history_growth_per_step}`; `first`/`last` = `{prefix_chars, system_chars, tools_chars, history_chars, messages}` of the first/last step that has it (characters, not bytes; `prefix_chars` = system + tools) |
| `turns` | per user turn (#379), derived from the same ledger: `[{turn, user_entry, steps, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, cost_usd}]`. Each token field is the sum over the turn's steps that reported it and `null` when none did (unknown is never 0); no elapsed time is carried. `[]` for a session with no turn |

The mission cost report (`fact/mission`) is rendered by `tole status`; it
gained an additive `cached_tokens` (`null` when no step reported it). The
MCP/ACP `tole_session_status` tool returns the same `usage_report` next to
`busy` and `entries`.

## Transcript read (#524)

`GET /sessions/{id}/entries?after=<seq>&limit=<n>` returns one page of a live
session's log, so a client that connects late or reconnects can render history:

```json
{"id": "srv-…", "entries": [{"seq": 1, "id": "e_1", "parentId": null, "type": "message",
 "timestamp": 1787807227226, "payload": {"role": "user", "text": "…"}}],
 "nextAfter": 1, "hasMore": false, "lastSeq": 1}
```

- `entries` are exactly the session JSONL's entries with `seq > after` (the same
  entries `tole resume` replays; nothing is reformatted). `after` defaults to 0.
- `limit` defaults to 100 and is clamped to 500; a page also stops once its
  serialized entries pass about 1 MiB (it always carries at least one entry), so
  `hasMore` can be true with fewer than `limit` entries.
- Page through with `after = nextAfter` while `hasMore` is true. `seq` is strictly
  increasing but not dense (other log lines share the counter): never assume `+1`.
- It reads the session **file** (replayed read-only, a torn tail is skipped, never
  truncated), not the live storage handle, so it works while a turn is in flight and
  shows entries as soon as they are durable. Cost is a replay of the file per call.
- `400` for a bad `after`/`limit`, `404` for an unknown (not live) session, `404`
  (`session transcript not found`) when a live session's file or its directory has
  been removed from outside (#915; never a `500` with a raw OS error), `401`
  without the token. A session that is not live (after a serve restart) is not readable
  until it is resumed (see below).
- Entries carry tool inputs and outputs. The token already grants approval authority
  and `POST …/prompt`; the read route adds no new trust, but anything printed into a
  session is readable by every holder of the token.

## Past sessions (#572)

`GET /sessions/index?limit=<n>&cursor=<c>` lists the sessions on disk, so a client can offer "continue" after a
restart or from a second device. `GET /sessions` stays the live list (`id`, `busy`).

```
200 {"sessions": [{"id", "name", "folder", "lastActivity", "entries", "turns", "parent", "live", "busy"}],
     "nextCursor": "<c>" | null, "skipped": <n>, "scanTruncated": <bool>}
```

- **Metadata only.** `name` is the operator's label (`fact/session_name`, or `null`), `folder` the working
  directory the session recorded (a session opened by serve records none, so it shows the workspace the directory belongs to), `lastActivity` the session file's
  modification time in ms, `entries` and `turns` counts, `parent` the session it was forked from, `live`/`busy`
  whether this process holds it. No prompt, preview, tool output or transcript text is ever returned.
- **Order and paging.** Newest first (ties by id). `limit` defaults to 50 and is clamped to 100 (`0`, a negative or a
  non-number is `400`). Pass `nextCursor` back as `cursor` for the next page; `null` means the end. A cursor that is
  not `<digits>.<session id>` is `400`. A page can hold fewer rows than `limit` while `nextCursor` is set (the
  per-request ceilings below, or skipped files): keep following it.
- **Scope.** Only this server's own sessions directory (`--sessions-dir`, else `<workspace>/.tole/sessions`);
  the request never names a path. With `--sessions-dir` the directory may be shared, so a session whose recorded
  folder is outside the workspace is left out. An id from the list goes to `POST /sessions {"resume": "<id>"}`.
- **Bounded cost.** Ordering uses names and file times only. Per request at most 10,000 directory entries are
  read (`scanTruncated: true` when more exist), at most 200 session files are opened, none over 16 MiB (counted in
  `skipped`, never replayed) and 64 MiB in total. A torn or unreadable file is skipped (`skipped`), never an error.
- `401` without the token, like every route.

## Discussions (#1042)

The owner's `tole discuss` over serve: read, dry run, start, resume and stop. They are the same library
(`tole_cli::discuss`) the terminal uses; the routes only turn a request into its arguments and an error into a
status. Starting is the owner's action under the server's auth, at depth 0 (inline, read-only voices with no tools),
with the contract, participant, budget and file rules of `tole discuss start`. A discussion that a terminal started
with `--tool` (helpers with the owner's named connected tools, #1039) can be listed, shown and stopped from here, but
not resumed while it is open: `serve` has no host to start helper children, so that resume is refused and the
terminal's `tole discuss resume` continues it (a stop and a finished discussion need no host). A discussion a model starts with the
`discuss` tool is still only an approval and is not started from here.

| Action | Route | Answer |
|---|---|---|
| List | `GET /discussions` | `{"discussions": [{"id", "status", "rounds", "tokens_used", "budget", "question"}]}`, the rows of `tole discuss list --json` (without the server's directory); `status` is `open`, `closed (<gate>)` or `unreadable` (then `error`). Ids sort ascending, at most 200 |
| Show | `GET /discussions/{id}` | the audit view of `tole discuss show --json`: `question`, `criteria`, `participants`, `budget`, `max_rounds`, `tokens_used`, `open_calls`, per round the `speakers`, the scribe's `summary` (decisions, claims, `disagreements` with each position, open questions, actions) or the speakers when it was passed on as written, the claim-checker's `checklist`, the `gate` and the `tokens`, and the `outcome` (`gate`, `why` and the report `text`) once closed. Never a raw contribution. Every string is redacted. `state` is `running` (a thread of this server drives it), `closed` or `interrupted`. `404` unknown, `400` malformed id |
| Start | `POST /discussions` | `202 {"id", "status": "running", "plan", "advice"}` and returns at once; the discussion runs on a thread of the server. The body is the estimate's. Refused before anything is spent or written: the estimate's `400`s, `409` for an id that is already taken and on a `--plan-mode` server. There is no limit on how many discussions run at once; each one's own budget bounds it |
| Resume | `POST /discussions/{id}/resume` | `tole discuss resume` for one that is open and not running (`state: interrupted`: a crash, a failed call). `202 {"id", "status": "running"}`; `409` when it is closed, already running or on a `--plan-mode` server; `404` unknown. No body: nothing can be changed. A file that is gone or changed since the start is refused, as on the CLI |
| Stop | `POST /discussions/{id}/stop` | `200 {"id", "outcome", "state"}`. The ledger's own escalation (`stopped`), which no participant text can start or delay: a running discussion closes at its next step (`state: running` until then), one nobody runs is closed now. Repeating it, or stopping a closed one, changes nothing. Allowed on every server: it only ends spending. `404` unknown |
| Estimate | `POST /discussions/estimate` | the dry run of `tole discuss start --dry-run`: `{"plan", "advice", "participants", "max_rounds", "max_calls", "budget_tokens", "round_floor_tokens", "files"}`. Validates the contract, the participants and the background files; writes nothing and calls no model |

Not here yet, and left to the owner (see the open questions of #1042): continuing a discussion with a note for the next
round or reopening a closed one (the ledger has no owner-note event and a closed discussion is final), per-discussion
tool allowance (#1039; it is the terminal's `--tool`, see `docs/faces.md`). A running discussion is followed on its own event
stream, `GET /discussions/{id}/events` (below), not on a session's.

The request body carries what the owner types for `tole discuss start`: `question`, `criteria`, `participants`,
`budget_tokens` (required), `max_rounds`, `files` and `id`. Any other key (`tools`, `allow`, `profile`, `modes`,
`secrets`, ...) is a `400` naming it. `participants` are profile names of the server OWNER's user config, read per
request; a built-in role or an unknown name is a `400`. `files` are paths inside the serve workspace root, confined
and read by the library exactly as `--file` is (a path outside it, a symlink, a FIFO, a file over 1 MiB, more than 4,
a duplicate are refused). The contract's own rules (question length, 1 to 12 criteria, a positive budget, at most 10
rounds) are the CLI's, because the library applies them.

## Cron jobs (#1042)

`GET /cron/runs` (below, the pull delivery target) is joined by the job list and two controls. They read and write
the same cron ledger `tole cron` does, through `tole_cli::cron::view`.

| Action | Route | Answer |
|---|---|---|
| List jobs | `GET /cron/jobs` | `{"jobs": [{"id", "name", "schedule", "timezone", "mode", "state", "next_run", "next", "last", "missed", "blocked", "created_by", "awaiting_review"}], "scheduler": {"alive", "host", "age_seconds"}}`. `state` is `active` or `paused`; `next_run` is Unix seconds (`null` while paused); `last` is the last run's status; `created_by` is `cli` (saved by its owner) or `agent` (the `cron_create` tool); `awaiting_review` is true for an agent's job nobody has enabled. `scheduler` says whether a ticker has passed lately and where it runs (`serve` for `tole serve --cron`, `tick` for `tole cron tick`) |
| Run history | `GET /cron/runs?job=<id or name>&limit=<n>` | unchanged (#579): newest first, at most 500, each with its redacted output capped at 16 KiB. `job` is an id, else a name (case-insensitive ASCII), percent-encoded when it has a space or a non-ASCII letter. `404` for an unknown `job` |
| Pause | `POST /cron/jobs/{id}/pause` | `200 {"id", "state": "paused"}`. Any job, on any server: a paused job has less authority |
| Resume | `POST /cron/jobs/{id}/resume` | `200 {"id", "state": "active"}` for a job its owner stands behind (saved with `tole cron create`, or an agent's job the owner already enabled). `403` for an agent's job nobody enabled: reviewing and enabling it is trust approval and stays `tole cron resume <id>` at a terminal. `409` on a `--plan-mode` server. The owner's review record (`enabled_by`) is never written over serve |

A job is addressed by its `id` (the name does not resolve; a wrong id is `404`). There is no route to create, edit,
remove or run a job: those stay local-only owner actions (a job carries a prompt, an allow list and, for a script, a
command run with the owner's authority).

## Profiles, skills and connected tools (#1042)

Three read-only views for the Helpers and Skills and tools pages. They answer from the owner's own files and change
nothing; **no other verb is a route** (`POST`, `PUT`, `PATCH` or `DELETE` on these paths is `404`), because writing a
profile, approving or revoking a skill or a connected tool, and changing the allow list stay with the owner at a
terminal (`docs/faces.md`, "Owner actions that are not serve routes"). A test snapshots the owner's data directory and
sends every such verb to every such path: nothing changes.

| Action | Route | Answer |
|---|---|---|
| Profiles | `GET /profiles` | `{"profiles": [{"name", "persona", "persona_chars", "models", "memory_room", "may_use", "session_allow"}]}` from the user config (re-read per request; none under `--no-config`). `persona` is the first line of the persona file (200 characters at most); `models` has the tiers the profile sets (`light`, `standard`, `heavy`, `advisor`) and its `default` tier name; `may_use` is the profile's own allow list and `session_allow` what a session opened with it gets on this server (#740). Secret grants, skill paths and the rest of the persona are not shown. A profile that cannot be loaded is a row with `name` and `error` |
| Skills | `GET /skills` | `{"skills", "drafts", "archived", "notes"}`. `skills`: every skill found from the serve workspace with `state` (`approved`, `changed` since its approval, `not_approved`, `refused` by the ledger, `unreadable`), its hash against the approved one, source and last ledger lines. `drafts`: skills the agent proposed that wait for review (`skill_propose`), with `tainted` when the session read untrusted text. `archived`: skills the curator moved to the archive. A part that cannot be read is a line in `notes` (`skills not read: ...`, `drafts not read: ...` for a draft store that cannot be listed or a draft that cannot be read, `archive not read: ...` for a curator ledger that cannot be read): a non-empty `notes` means the lists may be incomplete, so show unknown, not zero. A store or ledger that does not exist yet is empty, with no note |
| Connected tools | `GET /tools` | `{"servers", "notes", "probed": false}`. The owner's `--mcp-server` list against the plugin lock: per server `trust` (`trusted`, `changed`, `not_trusted`, `removed` for revoked or no longer configured), `state` (the sentence `tole plugin list` prints), tier, source, version, approval time, granted secret names and sandbox. **No server is started to answer**, so a manifest that changed since approval is not found here: a server the lock admits reads `locked (not probed: ...)` and `tole plugin list` at a terminal does the live check |

## Resume by id (#524)

`POST /sessions {"resume": "<id>", "cwd"?: "..."}` makes an existing session live
again, for example after a `tole serve` restart (the live map is in memory; the
session's JSONL is what survives).

- The id must be a valid session id and is only ever joined into the session directory
  (`--sessions-dir`, else `<cwd>/.tole/sessions`); a client never supplies a path.
  `cwd` defaults to the server workspace and goes through the same jail check as a new
  session (`403` outside it). A session lives under the workspace it was created in, so
  resuming it from another directory is a `404`.
- Success: `200 {"sessionId", "workspace", "resumed": true, "alreadyLive": false}`; the
  session keeps its history (read it with `GET …/entries`) and takes new prompts.
- **Interrupted turn** (#1230): when the log ends in a turn the server's death cut off, the
  answer also carries `"interrupted": true` (additive; absent otherwise) and the resume has
  already settled it: an open tool call is failed with its outcome UNKNOWN and never re-run,
  then `error: interrupted` and the register goes to `final` (`settle_interrupted`, appends
  only). No model is called and no tool runs, so a reconnect costs nothing and the route stays
  instant. **The prompt that was cut off is not answered**: the model sees it on its next
  request, but the client resends it (or a new one), and that prompt is accepted. A second
  resume is the already-live answer and writes nothing. `tole resume <id>` is the other way
  to recover: it continues the turn by calling the model. A serve-side continue is not built.
- **Already live**: `200 {"sessionId", "resumed": false, "alreadyLive": true, "busy": <bool>}`.
  Nothing is reopened or replaced, so a running turn is never disturbed; a client that
  reconnects mid-turn gets its id back together with the `busy` flag. An idle live session
  that holds a cut-off turn (a failed provider call) is settled and says `"interrupted":
  true`; a busy one, or one with an approval pending, is never touched.
- `404` no such session in that directory; `400` bad id or a `resume` that is not a string;
  `409` another process (a TUI, a second `tole serve`) holds the session's writer lock;
  `503` at the live-session cap with every session busy or holding a pending approval.
- Approvals: a resumed session queues non-preauthorized Writes exactly like a new one.
  Nothing about resuming grants authority; the token is the same.

## Session profiles (#740)

`POST /sessions {"cwd"?, "profile": "<name>"}` (create) and
`POST /sessions {"resume": "<id>", "profile": "<name>"}` (resume) open the session as
one `[profiles.<name>]` table of the **user** config (#584), so one daemon can host
several personas. `profile` is optional and a session without it is exactly as before.

- **What the profile fixes for that session:** the persona (`system_prompt_file`), the
  model (the profile's `default` tier, else the server default), the allow list, and the
  memory room (`memory_room`, default `profile-<name>`; only when the daemon runs with
  `--memory`, and a child's `TOLE_MEMORY_NAMESPACE` still wins). Skills, grants, the
  advisor and the router stay process-wide on this face, and child agents, the advisor
  and the router use the daemon's model, not the profile's (they still read the global
  provider config). Profiles have no budget
  field, so the server's existing budget logic applies unchanged.
- **Which profiles can be picked:** only the user config's, re-read on every create or
  resume (an edit or removal applies without a restart). A project config cannot define
  profiles. An unknown name is a `400` that lists the defined names; a malformed name
  (not 1-64 chars of `a-z`, `0-9`, `_`, `-`) or a non-string value is a `400` that does
  not echo it; a profile that cannot be loaded (unreadable persona) is a `500`.
  Selecting a profile needs the same credential as every route (the bearer token or,
  with `--auth basic`, the Basic user); there is no per-profile authorization, so
  whoever is authenticated may pick any profile.
- **Allow list precedence** (the one of `--profile` on the CLI): an `--allow` flag of
  `tole serve` pins the daemon's list and the profile's is ignored, so a profile never
  widens it; otherwise a profile that sets `allow` replaces the daemon's config/env
  list, and one that sets none leaves it. `--trust` presets are added in every case.
  Every approval still goes through the approval queue (#200, #722).
- **`memory_room` in a request is refused** (`400`, create and resume): the room comes
  from the profile and nothing in a request can set or widen it.
- **Fixed for the life of the session process.** The choice is recorded in the session
  log as a `fact/profile` register (a new entry, never a mutation of an old one).
  On resume the request's `profile` wins over the recorded one and is recorded again;
  without one, the recorded profile is used. A recorded profile that is no longer
  defined, a log entry with a malformed name, or a log that cannot be read to look for one,
  is a `409` (`500` for an unreadable log), never a silent fall-back to the daemon's
  defaults: resume with an explicit `profile`. On resume the persona and model come from
  the effective profile, not from the header pinned when the session was created (a
  resume without any profile is unchanged). Asking for another profile on a session that is already live is a
  `409`; the same one, or none, returns `alreadyLive` as before. Two concurrent
  resumes with different profiles: one opens the session, the other gets `409`.
- The success answer carries `"profile": "<name>"` when one was used.
- **Seeing profiles from the CLI:** `tole sessions` adds a `profile` column when some listed session recorded one (`?` for a malformed recorded name), and `tole doctor` prints `serve profiles: name (room <room>, <n> allow), ...` from the user config (names, rooms and counts only; nothing when no profile is defined).

## Event stream (#524)

`GET /sessions/{id}/events?after=<seq>` is a Server-Sent Events stream (`text/event-stream`,
same bearer token). `curl -N -H "Authorization: Bearer $TOKEN" …/events` shows it.

Two kinds of frames, deliberately separate:

| Kind | `event:` | `id:` | Replayable | Data |
|---|---|---|---|---|
| **Durable** | `entry` | the entry's `seq` | yes | the log entry exactly as `GET …/entries` returns it |
| Marker | `ready` | none | no | `{id, after, lastSeq, busy}`, sent once the replay is complete (withheld while the log cannot be read, so `ready` always follows the full replay) |
| Ephemeral | `text_delta`, `reasoning` | none | no | `{text}` |
| Ephemeral | `tool_started` | none | no | `{tool, input}` |
| Ephemeral | `tool_finished` | none | no | `{tool, ok, preview}` |
| Ephemeral | `approval_requested`, `approval_resolved` | none | no | the `GET /approvals` row (`id`, `tool`, `status`, …) |
| Ephemeral | `resync` | none | no | `{dropped}`: this client was too slow and `dropped` frames were skipped |
| Ephemeral | `closed` | none | no | `{id}`: the session left the live map; the stream ends |

- **Lossless replay.** The durable frames come from the append-only log, so a client that
  reconnects with `?after=<last seq seen>` (or the `Last-Event-ID` header, which `EventSource`
  sends by itself; an explicit `after` wins) receives exactly the entries it missed, once each,
  in order, then `ready`. No `after` means from the start of the log. Ephemeral frames carry no
  `id`, so a reconnect can never skip a real entry.
- **Ephemeral frames are best-effort.** Streamed text, reasoning and tool start/finish are
  never stored, so they are not replayed; the final answer is always an `entry`. A slow client
  loses ephemeral frames (never the turn: the server's per-stream queue is bounded and never
  blocks) and gets a `resync` marker. A decision (`approval_resolved`) is announced before the
  durable entries it causes.
- **When text deltas and tool events appear.** Only for a turn that starts while a stream is
  open on that session (with no stream open a turn runs exactly as it did before streams
  existed). A stream opened mid-turn gets the durable entries but no deltas for that turn. The
  turn that resumes after an approval `allow` has no live observer either: its entries arrive
  as durable frames.
- **Limits.** At most 8 streams are open per server (the 9th gets `429`); streams count within
  the server's overall connection cap but cannot starve `/approvals` and `/prompt`. A comment
  heartbeat (`: ping`) goes out after 15 s without any write and ends a stream whose peer is gone
  (the write fails); `TOLE_SERVE_HEARTBEAT_SECS` (1 to 3600) changes the interval, e.g. behind a proxy
  with a short idle timeout. The proxy settings that keep the stream unbuffered (Caddy, nginx) and a
  probe that checks them are in [container.md](container.md).
  `401` without the token, `404` for a session that is not live (resume it first), `400` for a
  bad `after`.
- **Cost.** Following the log is one `stat` per poll (about every 150 ms) while nothing
  changes, and a read-only replay of the session file when it did, so a very large session
  costs more per change.
- **Browsers.** `EventSource` cannot send an `Authorization` header: a web client reads the
  stream with `fetch` and its own SSE parser (or the token has to be passed another way, a
  decision left to the web client, #529).

## Following a discussion (#1094)

`GET /discussions/{id}/events?after=<seq>` is a Server-Sent Events stream (`text/event-stream`, same bearer token) of one
discussion, so a client follows it to its close without polling `GET /discussions/{id}`. A discussion is not a session, so
it has its own stream (one per discussion, not one for all of them, which a client opens as many as it follows) fed by
the discussion's own ledger, and it is a **signal, not a copy of the ledger**: a frame says that the ledger moved and
where the discussion stands, and the client answers it with `GET /discussions/{id}`, the redacted audit view above.

| `event:` | `id:` | Data |
|---|---|---|
| `changed` | the ledger `seq` | `{seq, round, step, tokens_used, closed}`. `seq` is the position of the last ledger line; `step` is what the ledger says must happen next (`contribute`, `summarize`, `check`, `decide`, `finish`, `done`); `round` the round that step belongs to |
| `ready` | none | `{id, after, lastSeq, busy}`, sent after the first look at the ledger (`busy` is whether this server drives the discussion) |
| `state` | none | `{state}`: `running` or `interrupted` flipped (a failed call, a resume), which the ledger does not record. Best effort, not replayed; `ready.busy` tells the state at the start |
| `closed` | none | `{id}`: the discussion is closed; the stream ends |

- **No polling, no loss, no repeat.** Frames are level signals: several ledger steps between two looks of the server (it
  checks the ledger file about every 150 ms, one `stat` when it did not change) are one `changed` frame at the latest
  `seq`, so nothing a client needs is skipped, only merged. A client that reconnects with `?after=<last seq seen>` (or
  `Last-Event-ID`; an explicit `after` wins) gets one `changed` frame at once when the ledger is past it, and none when it
  is not, so a dropped connection resumes without missing or repeating a frame. `tole-client`
  `Client::follow_discussion` does this, with the same reconnect policy as `follow_events`, and ends on `closed`.
- **Nothing a participant wrote reaches the stream.** The frames hold numbers, a flag, one word from a closed set and the id:
  no contribution, digest, question, criterion or closing text, redacted or not (read those with `GET /discussions/{id}`).
  Every frame still passes the process secret set, the one of that route, before it is written. A test plants a secret in
  a contribution, a digest and the closing text and proves none of it is on the stream.
- **When it ends.** `closed` ends it (also for a discussion that was already closed when the stream opened). A server that
  shuts down ends it without `closed` and the client reconnects. A discussion that is open but not driven (`interrupted`)
  keeps the stream open, with heartbeats, until a resume or a stop; `POST /discussions/{id}/resume` shows as `state: running`.
  A ledger that cannot be read for about 3 s in a row (deleted, garbled, replaced by a link) ends the stream without a frame;
  the reconnect gets the `404` or `500` of the first look. A ledger replaced by another readable one is not detected. A peer that
  closed its connection is noticed within about 150 ms and frees its slot.
- **Limits.** Counted in the same 8 open streams as a session's (the 9th gets `429`), heartbeat and
  `TOLE_SERVE_HEARTBEAT_SECS` as in the [event stream](#event-stream-524). `401` without the token, `404` `no such
  discussion`, `400` for a bad id or `after`. A ledger that cannot be read is a `500` that names no path.
- `GET /info` lists the feature `discussions.events`. The frames are the `discussionEvents` group of the
  committed JSON Schema (`crates/tole-client/schema/tole-client.schema.json`; `changed`, `ready`, `state`, `closed`).

## Images (#558)

A graphical client attaches images in two steps: upload, then reference the upload in a prompt.

```sh
curl -H "Authorization: Bearer $TOKEN" --data-binary @shot.png \
  http://127.0.0.1:7801/sessions/$ID/images
# {"id":"9f2c…","mime":"image/png","bytes":48213,"fnv1a64":"9f2c…"}
curl -H "Authorization: Bearer $TOKEN" -d '{"text":"what is wrong here?","images":["9f2c…"]}' \
  http://127.0.0.1:7801/sessions/$ID/prompt
curl -H "Authorization: Bearer $TOKEN" -o shot.png \
  http://127.0.0.1:7801/sessions/$ID/images/9f2c…
```

- **Upload.** `POST /sessions/{id}/images` takes the image as the raw request body (no
  multipart, no inline `data:` URL in the prompt JSON). The type is sniffed from the bytes
  (PNG, JPEG, GIF, WebP); the request's `Content-Type` is ignored. The file goes through the
  same core resolve path as `--image` (jail, 10 MiB cap, sniff, FNV-1a 64) and is stored
  content-addressed under `~/.codecora/tole/images/serve/<session id>/<fnv1a64>.<ext>` (inside
  the spill dir the provider already reads `--image` data URLs from). The `id` is the content
  hash, so uploading the same bytes twice returns the same reference and costs no quota. The
  session must be live (resume it first). The answer never contains a server path.
- **Prompt.** `images` is an optional array of upload ids next to `text`. Only uploads of the
  same session resolve; each is re-checked against its hash before the turn starts. The user
  message records the references (`path`, `mime`, `bytes`, `fnv1a64`) exactly like `--image`,
  so the provider re-reads the file on every step, including after a restart and `resume`.
- **Read.** `GET /sessions/{id}/images/{fnv1a64}` answers the bytes only for an image that the
  session's log records. The jail, the recorded size and the hash of the bytes served are
  re-checked on every read. The answer has the recorded `Content-Type`,
  `X-Content-Type-Options: nosniff` and `Cache-Control: private, no-store`.
- **Limits.** An upload body may be up to 10 MiB, and only once the headers carry the right
  token: without it the plain 1 MiB body cap applies, so nothing larger is buffered before
  auth. A declared body over the cap is authenticated before it is answered: a wrong or
  missing token gets the usual `401` and counts against the per-IP auth limiter (`429` after
  10 failures a minute), and only the right token gets `413`, so the size answer is never a
  token check outside the limiter. An authenticated upload body gets 120 s instead of the usual 15 s read budget. Uploads
  count within the server's 32-connection cap. Quotas on the stored uploads, in bytes:
  `TOLE_SERVE_IMAGE_SESSION_QUOTA` per session (default 64 MiB) and
  `TOLE_SERVE_IMAGE_TOTAL_QUOTA` across all serve sessions (default 512 MiB); `0` disables
  uploads, an unparsable value keeps the default (with a warning on stderr).
- **Errors.** `400` empty body, malformed id or `images` that is not an array of ids; `401`
  without the token; `404` unknown or not-live session, unknown upload id, or an image that the
  log does not record (or whose file is gone); `403` a recorded image outside the image jail;
  `409` a stored or recorded file that no longer matches its hash, size or type (reported,
  never served or sent); `413` over 10 MiB; `415` not a PNG/JPEG/GIF/WebP image; `503` no data
  directory (`CODECORA_HOME` / `HOME` unset); `507` a quota would be exceeded (the message
  names the variable).
- **Known limitation.** Uploads that no prompt references are not garbage-collected, and
  removing a session does not remove its uploads; they keep counting against the quotas until
  removed by hand from the directory above. A cleanup is a follow-up.

## Auth model

The serve credential, the token or the Basic password, **is approval authority**
(see the threat model): a
phone holding it can let code write to the box. Practical scoping until
dedicated device tokens exist (#follow-up): run missions under a
dedicated OS user, keep `--allow`/`--trust` minimal on the serve
process, and treat the token as a revocable secret (rotate = restart
serve).

## Local socket and per-launch credential (#1043)

`tole serve --socket <path>` listens on a Unix domain socket (macOS, Linux) instead of TCP.
It is for a client on the same machine, the desktop, and it answers exactly what the TCP listener
answers: the same routes, the same bearer token (or Basic) check, the same per-peer auth-failure
limiter (every socket peer shares one bucket) and the same event stream. Only the listener
differs. `--socket` replaces `--bind` and `--port` (giving both is an error) and serves the REST
transport only (`--transport mcp` stays on TCP). A Windows named pipe is not implemented yet:
`--socket` there says so and exits; use the loopback TCP listener until it lands.

```bash
# A launcher makes a fresh token per start and hands it over a pipe (here: stdin is the pipe).
printf '%s' "$TOKEN" | tole serve --socket "$XDG_RUNTIME_DIR/tole/serve.sock" --token-fd 0
curl --unix-socket "$XDG_RUNTIME_DIR/tole/serve.sock" http://localhost/health
```

- **The directory is the boundary.** The socket's parent directory is created `0700`. Serve
  refuses to start when that directory is a link, is not a directory, is owned by someone else
  or has any group or other permission (so `/tmp` itself is refused: use a private
  subdirectory), when the socket path is a link or any file that is not a socket, and when the path
  is longer than a socket address holds (103 bytes on macOS, 107 on Linux). The socket file is
  set to `0600`. A stop by SIGTERM or SIGINT drains and then removes the socket file (only the
  file this serve bound; also when the grace runs out or a second signal arrives, see
  [Shutdown](#shutdown-on-sigterm-and-sigint-1014)). A serve that is killed (SIGKILL, a crash)
  leaves the file behind, and the next start removes it, as below.
- **One serve per socket path.** A start takes an exclusive `flock` on `<socket>.lock` in the
  same private directory and holds it for the life of the process (the kernel drops it when
  the process ends, however it ends; the lock file itself stays). A second start on the same
  path is refused with "already runs on it or is starting", naming the version when the
  running serve answers `/health`. Two starts at once cannot delete each other's socket.
- **Stale sockets.** Under that lock, an existing socket is removed only when nothing answers
  on it (`connection refused`). A socket that answers is never taken over, so a second desktop
  window or a CLI client attaches to the running serve (`GET /health`, below) instead of
  starting another.
- **The token is still required**, on every route but `/health`. The socket narrows who can
  reach serve; it does not replace the credential. The per-launch token reaches serve in one of
  two ways, never on the command line and never in the environment:
  - `--token-fd <n>`: read to the end from an inherited file descriptor (at most 4 KiB, then the
    descriptor is closed). Not a terminal, not 1 or 2. The parent writes the token into a pipe,
    closes the write end and passes the read end. The token lives as long as that serve process.
  - `--token-file <path>`: as always (a regular file, no link or FIFO, at most 4 KiB). With
    `--socket` the file must also be owned by the current user and have no group or other
    permission (`0600`), ideally inside the same private directory.
  `--token` (visible in the process list) and `TOLE_SERVE_TOKEN` (the environment) still work
  where they did, but a launcher that wants the local boundary should use neither.
  The descriptor keeps the token from other users and from the argument list and environment;
  it does not hide it from other processes of the same user (they can read the launcher's
  memory, or connect to the socket themselves and need only the token to act).
  `--token-fd` waits at most 5 seconds for the writer to close its end, then fails with an
  error that does not contain the token.
- **Webhook routes** (`/hooks/...`) keep their own gate. A socket peer counts as a loopback
  client, so a route the owner configured as `insecure_loopback` (unsigned, loopback only)
  loads with `--socket` too and lets any process of the same user fire its job over the socket
  without the per-launch token. That route is the owner's explicit choice; do not configure
  one on a serve whose token is meant to separate the desktop from the user's other processes.
- **Failed-auth limiter.** Every socket peer shares one bucket (a socket has no peer address):
  10 wrong tokens in 60 s answer `429` to everything else for the rest of the window, including
  the right token. `GET /health` is exempt on the socket (it needs no token), so a client can
  still see that serve is up. A same-user process holding a stale token can therefore lock the
  desktop out of the authenticated routes for the window. TCP is unchanged (per source IP,
  `/health` limited too).

### `GET /health`

No auth. Besides `{"status":"ok"}` it reports what a client needs to decide whether to attach:

```json
{"status":"ok","protocol":1,"version":"0.12.0"}
```

`protocol` is the version of the serve wire contract (`SERVE_PROTOCOL_VERSION`); it is raised
only for a change a client written against the previous number cannot handle, never for a new
route or field. `version` is the `tole` binary's version. A client refuses a serve whose
`protocol` it does not know. The fields are on both listeners.

### `GET /info`

Authenticated like every route but `/health` (a missing or wrong credential is `401`). It lets a
client check the server it reached before it relies on a route or a field:

```json
{"version":"0.12.0","protocolVersion":1,"features":["approvals","compact","cron","delegation.approvals","detach","discussions","discussions.events","effort","entries","events.sse","fork","hooks","images","interrupt","mcp","missions","models","profiles","resume","session.name","sessions.index","skills","tools","tree"]}
```

- `version` is the `tole` binary's version, and `protocolVersion` the number `GET /health` reports
  as `protocol` (`SERVE_PROTOCOL_VERSION`): a client that refuses a protocol it does not know may
  use either route.
- `features` is a sorted list of stable names, one per route group of this document (`entries`,
  `events.sse`, `resume`, `images`, `approvals`, `delegation.approvals`, `fork`, `compact`,
  `missions`, `models`, `sessions.index`, `profiles`, ...). A name is never renamed or removed
  without a protocol bump; a newer server may list more, and a client ignores the ones it does
  not know. Two names depend on the process: `mcp` (this binary can list connected MCP servers,
  `GET /tools`) and `auth.basic` (the server also accepts HTTP Basic, `--auth basic`).
- Nothing else is reported: no path, user name, key or configuration value.
- A server older than the route answers `404 {"error": "not found"}`. `tole-client` has
  `Client::info()` and `Info::has(name)`.

## Wire compatibility (#1024)

The serve API is **additive-only**. Graphical clients (the desktop, Flutter, web), the chat
gateway and `tole-client` are written against the bodies, routes and event frames in this
document, and an old client must keep working against a newer `tole serve`.

**Allowed in any release (additive):** a new route; a new key in a response body; a new
optional key in a request body; a new SSE event name; a new entry `type` or a new key in an
entry's `payload`; a new word in a vocabulary this document calls open (a `kind` or `effect`
code, a `status`), which clients already read as "unknown".

**A break:** removing or renaming a route, a response key, an SSE event or an entry `type`;
changing the JSON type of a key or what a value means (including turning a number's unit or a
string's vocabulary into another); making an optional request key required, or refusing input
the document says is accepted; changing a status code this document names. A break

1. raises `SERVE_PROTOCOL_VERSION` (`tole_cli::protocol`; reported as `protocol` by
   `GET /health`, see above), and
2. gets a line under `### BREAKING` in `CHANGELOG.md`, and
3. is an owner decision: say so in the PR.

**Clients ignore what they do not know:** unknown keys in a body or an entry payload, unknown SSE
event names (`tole-client` keeps them as `Event::Other`), unknown codes in an open vocabulary. A
client that refuses an unknown key turns every additive release into a break.

**Entries are the ledger.** `GET /sessions/{id}/entries` and the `entry` frames return the
`Entry` the append-only session file holds. A change to `Entry` or to a payload the turn loop
writes is therefore also a ledger-format change: old session files must keep replaying, and
AGENTS.md section 4 (callers, impact, tests to rerun) applies.

**What makes a change visible.** None of these is a substitute for review; each fails a build
when the surface moves, so the change is made on purpose:

| Check | Pins |
|---|---|
| `cargo test -p tole-cli --test client_routes` | the route table: every route `serve.rs` registers is listed, answered by a real serve, in this document and (unless allowlisted) in `tole-client` |
| `cargo test -p tole-cli --test serve_wire_routes` | golden JSON of the read routes (`/info` included) and of the refusals (`tests/golden/serve/routes/`) |
| `cargo test -p tole-cli --test serve_wire_entries` | golden transcripts, one per way a turn ends, with every entry kind; the session file's lines; one sample of each event frame (`tests/golden/serve/entries/`, `ledger/`, `events/`) |
| `cargo test -p tole-core --test entry_wire` | the `Entry` serialization byte for byte, and the line the storage layer writes |
| `scripts/client-schema.sh check` (CI job `Client schema`) | the JSON Schema of every request, response and event type of `tole-client` |

A golden file that differs is accepted with `TOLE_UPDATE_GOLDEN=1 cargo test -p tole-cli --test
<name>`; review the diff: only added lines are additive. Values that change from run to run (ids,
clocks, paths, the binary version, the size of the prompt) are replaced by placeholders before the
comparison; the `seq` numbers are replaced by their rank, so a new log line does not rewrite every
file.

`GET /info` (above) is the capability probe: a client that needs to know whether a route group
exists tests its name in `features`. A server older than `GET /info` answers `404 {"error": "not
found"}` to it, and such a server is the one to treat as "older" (a client may still call a route
it needs and read a `404` the same way).

## Shutdown on SIGTERM and SIGINT (#1014)

`tole serve` (REST and `--transport mcp`) handles SIGTERM and SIGINT (Unix; on Windows Ctrl+C
still ends the process at once). The handler only counts the signal; the rest runs on ordinary
threads. From then on every new request is answered `503` (`GET /health` too, with
`Retry-After: 1`), event streams end once their turn has settled, the cron ticker and the
webhook senders stop (a delivery attempt already in flight is finished, a cron run already
started is not waited for and is recovered from the ledger at the next start, as after a
crash), and every running turn is ended through the existing detach path: its prompt answers
`stopReason: "detached"`, the log stays append-only and the session resumes with a new
prompt (a running mission is cancelled the way `POST /missions/{id}/cancel` does). A detach
takes effect when the model call or tool that is running settles, so the wait is as long as
that call, up to the grace period: `--shutdown-grace <secs>` (or `TOLE_SERVE_SHUTDOWN_GRACE`;
default 20, `0` does not wait, at most 3600). Serve exits `0` once nothing is left running, and
`1` when the grace runs out (the stderr line says what was left; the turn is cut where it
stands, as a kill would cut it) or when a second signal arrives. On the local socket the
socket file is removed and `<socket>.lock` is released only after the drain, so a supervisor
that waits for the lock never sees a serve that is still finishing a turn. Set the stop timeout
of the process manager above the grace (`docker stop` waits 10 s by default, `systemctl stop`
90 s) or lower the grace to fit it. Without a signal nothing changes.

## HTTP Basic auth (#742, first slice)

`tole serve --auth basic` authenticates with HTTP Basic instead of (or next to) the
static bearer token. It is the small answer for one person with many devices:
**one user, one password, no scopes, no identity bound to a session** (anyone who
authenticates sees and resumes every session, as a token holder does). OIDC is a later,
separate issue (#742 stays open for it); scopes (a read-only credential that cannot
`POST`) are not built either, so every credential is full authority, approvals included.

```sh
# 1. Make the hash. The password is read from stdin (a terminal is asked twice,
#    without echo, restored even on an error; a Ctrl-C at the prompt can leave echo off,
#    `stty echo` repairs it); it is never an argument and is not printed. At least 8 characters.
printf '%s' "$PASSWORD" | tole serve-hash > ~/.config/tole-serve.hash   # PBKDF2-HMAC-SHA256, 600000 iterations
chmod 600 ~/.config/tole-serve.hash

# 2. Serve it. Loopback bind, TLS at a reverse proxy in front.
tole serve --auth basic --basic-user owner --basic-hash-file ~/.config/tole-serve.hash
#    or: TOLE_SERVE_BASIC_USER=owner TOLE_SERVE_BASIC_HASH='$pbkdf2-sha256$...' tole serve --auth basic
curl -u owner:"$PASSWORD" http://127.0.0.1:7801/sessions
```

- **The hash, never the password, is what the server holds.** Format
  `$pbkdf2-sha256$i=<iterations>$<salt>$<hash>` (random 16-byte salt). It comes from
  `--basic-hash-file` (a regular file, at most 4 KiB) or `TOLE_SERVE_BASIC_HASH`; there
  is no hash flag, because a command-line value is readable by every local process. A
  hash with fewer than 10000 iterations (or more than 10 million) is refused at start-up,
  as is a plaintext password given where a hash belongs; no error repeats the value. The
  hash is held as a secret by role (redacted at every egress sink, removed from children's
  environments, like the token). Basic is not available from the config file: flags and
  environment only, so a project config can never turn it on or change the user.
  Rotating the password = make a new hash and restart.
- **Hashing is deliberately slow** (PBKDF2, 600000 iterations by default, about a
  tenth of a second). A header that verified is remembered for 5 minutes (keyed by its
  SHA-256, in memory, at most 64; a failure is never remembered), so a client that sends
  the header on every request pays once, not per request. An unauthenticated client can
  still make the server hash, once per request, up to the per-IP limit below.
- **One check for every route.** The bearer token and Basic go through the same
  `Authenticator::check`, in the REST face (including the event stream, entries, images,
  approvals, cron and the upload path), and in `--transport mcp`. Everything but `/health`
  and the webhook routes is behind it. Webhooks (`POST /hooks/<route>`) keep their own
  signature gate: a valid Basic header does not open one and a signature needs no Basic
  credential. With `--auth basic` a bearer token that is set (`--token-file`,
  `TOLE_SERVE_TOKEN`) keeps working next to it, for scripts; it is optional there. Without
  `--auth basic` nothing changes: `--auth token` is the default, the token is required, and
  a `401` carries no `WWW-Authenticate`.
- **A `401` says nothing about which part was wrong.** A wrong user, a wrong password, a
  malformed header and a missing header get the identical response: status `401`, the body
  `{"error":"unauthorized"}` and `WWW-Authenticate: Basic realm="tole", charset="UTF-8"`
  (the challenge only when Basic is on). The password hash is computed whether or not the
  user matched, and compared in constant time (ring's `pbkdf2::verify`; the user name with
  `ct_eq`), so the answer and its timing do not tell a wrong user from a wrong password.
  A Basic credential longer than 512 bytes is refused before it is hashed.
- **Failures are limited** per source IP, as for the token: 10 a minute, then `429` for
  everything (the right password included, `/health` too) until the window rolls over. The
  oversized-upload answer (`413`) is given only to a valid credential, as before. Behind a
  reverse proxy every client has the proxy's address, so ten failures from anywhere lock out
  everyone for a minute; tole does not read `X-Forwarded-For`. Rate-limit at the proxy as well.
- **TLS is not served here.** Basic puts the password in every request, so over plain
  HTTP it is readable by anyone on the path, and the password may be one you use elsewhere
  (the token at least is single-purpose). Keep the default loopback bind and terminate TLS at
  a reverse proxy on the same host. `--auth basic` with a non-loopback `--bind` is
  **refused** unless `--allow-insecure-basic` says TLS ends in front of this server (a
  container that must bind `0.0.0.0` behind its proxy, say). The static token does not have
  this refusal (unchanged).
- **Reaching it from `tole-client`.** The Rust client connects to the proxy's `https://` address
  (`Client::connect`, or `TlsTransport` to trust a private CA) and verifies its certificate; there is
  no way to turn that off (#1071). It sends the bearer token only; Basic has no client yet.
- **Credentials are never logged**: the server writes no request header anywhere, an error
  names the flag or file and never a value, and the secret set covers the hash. The password
  itself is never known to the server, only checked per request.
- **Every approval still goes through the approval queue.** Basic authenticates the
  client; it changes no approver, no risk class and no allow list. Whoever holds the
  password can decide approvals, exactly as a token holder can.

## What lives where

- **tole** (this repo): the REST surface above, its hardening, audit
  trail, expiry semantics. Done in #200/#214.
- **uteke-mobile**: the Flutter client, the push channel, device
  storage for the token, biometric gating of the approve button. The
  parked FFI plan (flutter_rust_bridge over tole-core) is NOT required
  for control — the REST surface is the contract.
