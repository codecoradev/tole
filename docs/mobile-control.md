# Mobile control — driving missions from uteke-mobile (issue #202)

The 0.8.0 remote-approval surface (#200) is the entire backend a phone
needs: missions run on the box under `tole serve`, the phone lists
running missions, receives pending Write approvals, and approves/denies
from anywhere. The phone is a client of the token-authenticated serve face.
(Graphical clients need more than polling: a transcript read ships with #524,
resume-by-id and an event stream follow; see the UI epic #523.)

## The surface uteke-mobile consumes

All routes live under `tole serve` (bearer-token auth on everything
except `/health`; decisions revalidate per request; auth failures are
rate-limited per source IP).

| Action | Route | Notes |
|---|---|---|
| List sessions | `GET /sessions` | id + busy per session |
| Start a mission | `POST /sessions` `{cwd}` → `POST /sessions/{id}/prompt` `{text}` | the prompt is the mission goal (`tole mission` semantics chain turns; a serve prompt runs ONE turn — mission chaining over serve is a follow-up) |
| Mission status | `GET /sessions/{id}/status` | busy flag, entry count, `usage_report` (tokens, cache-hit rate, request-size split; #211), mission cost report (`fact/mission` register, #201) |
| **Pending approvals** | `GET /approvals` | queue entries: id, session, tool, status; expires to denied after 15 min |
| **Approve / deny** | `POST /approvals/{id}/decision` `{"decision":"allow"\|"deny"}` | allow = one-shot for exactly that (session, tool+input) + automatic resume; deny = recorded verdict |
| Cancel a turn | `POST /sessions/{id}/cancel` (#178) | the turn settles `cancelled`, durably |
| Steer a turn | `POST /sessions/{id}/interrupt` `{text, urgent?}` (#377) | `202` queued; delivered at the turn's next safe point (after the running tool, before the next model call) as an attributed user-role entry, nothing is lost. `urgent` also withdraws a tool call that has not started. `409` when no turn is running, `400` for blank/oversized text or a full queue (16 pending) |
| Detach a turn | `POST /sessions/{id}/detach` (#217) | `202`; the running tool settles, the turn ends `detached` (resumable) and background jobs keep running, nudging the session when they finish. `409` when no turn is running |
| Live events | `GET /sessions/{id}/events?after=<seq>` (#524) | Server-Sent Events, see [Event stream](#event-stream-524); replays the log from a `seq`, then follows it |
| Resume a session | `POST /sessions` `{"resume": "<id>"}` (#524) | reopen an existing session, see [Resume by id](#resume-by-id-524) |
| Read the transcript | `GET /sessions/{id}/entries?after=<seq>&limit=<n>` (#524) | one page of the session's log, see [Transcript read](#transcript-read-524); works while a turn runs |
| Cost report | `GET /sessions/{id}/status` | the `mission` register: turns, steps, tokens, wall time, per-risk-tier tool calls |

The round trip the app must support (the cross-repo acceptance):

1. `GET /approvals` on a poll interval (the push-channel decision —
   ntfy/UnifiedPush vs FCM — belongs to uteke-mobile; tole exposes no
   push endpoint by design, see the threat model).
2. Render the pending Write (tool + description + session).
3. `POST /approvals/{id}/decision` — allow or deny.
4. `202` (allow) means the resume is running; `GET /sessions/{id}/status`
   until `busy` clears.

## Status JSON fields

`GET /sessions/{id}/status` returns `{id, entries, busy, usage_report}`.
`entries` and `usage_report` are `null` while a turn is in flight (the
session storage is busy). `usage_report` was added by #211 and is purely
additive: clients must ignore unknown fields (the existing three keep their
meaning). Its shape — every unknown value is `null`, never a fabricated 0:

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

## Event stream (#524)

`GET /sessions/{id}/events?after=<seq>` is a Server-Sent Events stream (`text/event-stream`,
same bearer token). `curl -N -H "Authorization: Bearer $TOKEN" …/events` shows it.

Two kinds of frames, deliberately separate:

| Kind | `event:` | `id:` | Replayable | Data |
|---|---|---|---|---|
| **Durable** | `entry` | the entry's `seq` | yes | the log entry exactly as `GET …/entries` returns it |
| Marker | `ready` | none | no | `{id, after, lastSeq, busy}`, sent once the replay is complete |
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
  heartbeat (`: ping`) goes out after 15 s of silence and ends a stream whose peer is gone.
  `401` without the token, `404` for a session that is not live (resume it first), `400` for a
  bad `after`.
- **Cost.** Following the log is one `stat` per poll (about every 150 ms) while nothing
  changes, and a read-only replay of the session file when it did, so a very large session
  costs more per change.
- **Browsers.** `EventSource` cannot send an `Authorization` header: a web client reads the
  stream with `fetch` and its own SSE parser (or the token has to be passed another way, a
  decision left to the web client, #529).

## Auth model

The serve token **is approval authority** (see the threat model): a
phone holding it can let code write to the box. Practical scoping until
dedicated device tokens exist (#follow-up): run missions under a
dedicated OS user, keep `--allow`/`--trust` minimal on the serve
process, and treat the token as a revocable secret (rotate = restart
serve).

## What lives where

- **tole** (this repo): the REST surface above, its hardening, audit
  trail, expiry semantics. Done in #200/#214.
- **uteke-mobile**: the Flutter client, the push channel, device
  storage for the token, biometric gating of the approve button. The
  parked FFI plan (flutter_rust_bridge over tole-core) is NOT required
  for control — the REST surface is the contract.
