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
| List sessions | `GET /sessions` | id + busy per session (+ `profile` when it has one, #740) |
| Start a mission | `POST /sessions` `{cwd}` → `POST /sessions/{id}/prompt` `{text}` | the prompt is the mission goal (`tole mission` semantics chain turns; a serve prompt runs ONE turn; for chained turns use `POST /missions` below, #947) |
| Mission status | `GET /sessions/{id}/status` | busy flag, entry count, model, effort, context window, name (#573), `usage_report` (tokens, cache-hit rate, request-size split; #211), mission cost report (`fact/mission` register, #201) |
| **Pending approvals** | `GET /approvals` | queue entries: id, session, tool, status; expires to denied after 15 min |
| **Approve / deny** | `POST /approvals/{id}/decision` `{"decision":"allow"\|"deny"}` | allow = one-shot for exactly that (session, tool+input) + automatic resume; deny = recorded verdict. Only the first decision counts: the same verdict again answers `200` with `"resuming":false` and audits and resumes nothing; the opposite verdict, or any decision on an entry that already expired, answers `409` with the current `status` |
| **Delegated children's approvals** | `GET /approvals` (entries with `source: "delegation"`), `POST /approvals/dap-…/decision` `{"decision":"allow","confirm":"<the same id>"\|"deny","fingerprint"?}` (#714) | the durable queue of delegated children, next to the list above: id, goal, node, tool, target, redacted `display`, age, seconds left, `fingerprint`, the `chain` of levels, and `text` (the whole request fenced as data, as `tole delegation approvals show` prints it). The same bearer token as every route. `allow` must repeat the id as `confirm` (no id, prefix or pattern means "all": `400`); only a pending request is decided (`409` with the reason for an expired, consumed, already decided or settled one, `404` unknown); the decision is bound to the request's fingerprint and recorded `via: serve`; the server refuses (`403`) when it runs under an agent session. No session is resumed (the child is already waiting). Requests exist only when `[delegation] approvals` is on |
| Prompt stops for an approval | `POST /sessions/{id}/prompt` answers `200 {"stopReason":"approval_required","approval":{"id","tool","status":"pending",…}}` (#571) | `approval` is the `GET /approvals` row (`description` is sanitized to one line but not length-capped, as there) of the entry THIS turn queued for this session, found by a queue snapshot taken before the turn; its `id` is what `POST /approvals/{id}/decision` takes. If that entry expired or was decided before the lookup, `approval` is omitted: use `GET /approvals?session=<id>`. A stop that queued nothing (a pre-tool hook denial, a blocked stop gate, an unknown tool, a provider failure, a loop) stays `refusal`; ACP and the MCP session tool keep their own vocabulary (`refusal`) |
| Cancel a turn | `POST /sessions/{id}/cancel` (#178) | the turn settles `cancelled`, durably |
| Steer a turn | `POST /sessions/{id}/interrupt` `{text, urgent?}` (#377) | `202` queued; delivered at the turn's next safe point (after the running tool, before the next model call) as an attributed user-role entry, nothing is lost. `urgent` also withdraws a tool call that has not started. `409` when no turn is running, `400` for blank/oversized text or a full queue (16 pending) |
| Detach a turn | `POST /sessions/{id}/detach` (#217) | `202`; the running tool settles, the turn ends `detached` (resumable) and background jobs keep running, nudging the session when they finish. `409` when no turn is running |
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
| Read a mission | `GET /missions/{id}` (#947) | `200 {"id", "status", "running", ...}`: `status` is `running` or how it ended (the CLI's: `complete`, `exhausted_steps`, `exhausted_tokens`, `exhausted_time`, `verify_failed`, `verify_refused`, `stopped` for an approval stop, a cancel or a provider failure, `error`); once ended also `turns`, `steps`, `tokens`, `wallSeconds` and `summary` (the `fact/mission` register), and `error` when the run could not start. A mission of an earlier server process is read from its session file. `404` unknown |
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
- `400` for a bad `after`/`limit`, `404` for an unknown (not live) session, `401`
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
- **Already live**: `200 {"sessionId", "resumed": false, "alreadyLive": true, "busy": <bool>}`.
  Nothing is reopened or replaced, so a running turn is never disturbed; a client that
  reconnects mid-turn gets its id back together with the `busy` flag.
- `404` no such session in that directory; `400` bad id or a `resume` that is not a string;
  `409` another process (a TUI, a second `tole serve`) holds the session's writer lock;
  `503` at the live-session cap with every session busy.
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
  with a short idle timeout.
  `401` without the token, `404` for a session that is not live (resume it first), `400` for a
  bad `after`.
- **Cost.** Following the log is one `stat` per poll (about every 150 ms) while nothing
  changes, and a read-only replay of the session file when it did, so a very large session
  costs more per change.
- **Browsers.** `EventSource` cannot send an `Authorization` header: a web client reads the
  stream with `fetch` and its own SSE parser (or the token has to be passed another way, a
  decision left to the web client, #529).

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
