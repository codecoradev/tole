# Mobile control — driving missions from uteke-mobile (issue #202)

The 0.8.0 remote-approval surface (#200) is the entire backend a phone
needs: missions run on the box under `tole serve`, the phone lists
running missions, receives pending Write approvals, and approves/denies
from anywhere. The phone is a client of the token-authenticated serve face.
(Graphical clients need more than polling: a transcript read, resume-by-id and an
event stream ship with #524, image upload and read with #558; see the UI epic #523.)

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
| Upload an image | `POST /sessions/{id}/images` (raw body, #558) | returns `{id, mime, bytes, fnv1a64}`; see [Images](#images-558) |
| Prompt with images | `POST /sessions/{id}/prompt` `{text, images: [<id>]}` (#558) | the turn records the references, never the blob |
| Read an image | `GET /sessions/{id}/images/{fnv1a64}` (#558) | only images recorded in that session's log |
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
