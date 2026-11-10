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
  without the token. Sessions that are not live (after a serve restart) are not readable
  yet; resume-by-id (next in #524) makes them live.
- Entries carry tool inputs and outputs. The token already grants approval authority
  and `POST …/prompt`; the read route adds no new trust, but anything printed into a
  session is readable by every holder of the token.

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
