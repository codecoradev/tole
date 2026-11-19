# `turn_finished` outbound webhook event: design note

Status: **implemented with provisional defaults** (issue #962). Step 1 (this note)
was design only; step 2 implements the recommendations below as DEFAULTS while
D1-D9 are still open. Each is a named constant or one seam in
`crates/tole-cli/src/webhook_out/turn.rs`. The one deviation from the text: the
log position field is `last_seq`, not `entries` (D2, verified: receivers page by
`Entry.seq`, and `GET /sessions/{id}/entries` returns `lastSeq`/`nextAfter`). Extends [webhook-delivery-design.md](webhook-delivery-design.md)
(#911, implemented) and reads against [threat-model.md](threat-model.md).
Consumer: the chat gateway of epic #910 (`codecoradev/tole-gateway#16`, and the
mention-to-persona routing of #969).

**How to read the markers.** Each point is tagged:

- **FIXED** comes from #911 or #962 and is not reopened here.
- **RECOMMENDATION** is this note's proposal; the owner may overrule it.
- **DECISION NEEDED** has no safe default; section 11 lists them all.

## 1. Problem

The outbound events today are `cron_run_finished`, `approval_requested` and
`approval_resolved`. `approval_resolved` fires when the decision is made, before
the turn continues. After an Allow the gateway still polls
`GET /sessions/{id}/status` once a second, then reads new log entries, to learn
that the resumed turn ended. A resumed turn that stops on a further approval
already has `approval_requested`; a turn that simply finishes has nothing.

Two consumer needs follow:

1. **Drop the status and log polling after an Allow** (`tole-gateway#16`). The
   gateway wants one signal per turn end, then a single authenticated read of
   the new entries.
2. **Route a persona's reply to its origin thread** (#969). The gateway keeps
   one durable session per persona and a map from session to the chat thread a
   mention came from. A `turn_finished` for that `session_id` tells it which
   thread to post into. The event never tells it what to post.

## 2. Scope and non-goals

- **FIXED** One new event type, `turn_finished`, on the existing outbox, signing
  and retry path of #911. No new endpoint, header, key or crate.
- **FIXED** Destinations stay in the user config only. No request, session or
  agent text can name one.
- **FIXED** The payload carries ids and status words only: no transcript text,
  no tool output, no prompt text, no secret. The receiver reads the transcript
  through the authenticated serve API.
- Non-goals: delivering the assistant's answer; replacing
  `GET /sessions/{id}` reads; delegated-child (`dap-*`) turns; exactly-once
  delivery; any new way to decide an approval (the webhook stays a notification,
  [webhook-delivery-design.md](webhook-delivery-design.md) section 8).

## 3. Event name and body

Event type (also `X-Tole-Event`): `turn_finished`. It passes the shape check
`webhook::valid_event`; the real closed list is the `Event` enum in
`webhook_out/mod.rs` (`ALL`, `as_str`, `parse`), which gains a `TurnFinished`
member (`ALL` goes from 3 to 4 entries; `WebhookTest` stays outside it). Envelope is the unchanged schema v1 of #911 section 2
(`v`, `id`, `type`, `destination`, `at`, `data`); `v` stays 1 because adding an
event type is additive.

```json
{
  "v": 1,
  "id": "turn:s-1f2e3d:9c8b7a6d5e4f3a2b:14",
  "type": "turn_finished",
  "destination": "gateway",
  "at": 1795000123,
  "data": {
    "session_id": "s-1f2e3d",
    "status": "completed",
    "entries": 14
  }
}
```

`data` fields (**RECOMMENDATION**, all of them closed-type, none free text):

| field | type | meaning |
|---|---|---|
| `session_id` | string, id charset | the serve session whose turn ended |
| `status` | closed word, section 4 | how the turn ended |
| `entries` | integer | the session log position when the turn settled (see D2: it must be the number `GET /sessions/{id}/entries?after=` pages by, i.e. `last_seq`, not a raw count unless the two are proven equal); the receiver reads entries after the cursor it already holds |
| `approval_id` | string, id charset, only when `status = approval_pending` | the approval the turn stopped on |
| `code` | closed word, only when `status = failed` or `blocked` | a fixed reason code, never a message (section 4) |

- **RECOMMENDATION** No `summary`, no `text`, no `model`, no `tool`, no
  `profile`, no token or cost counts, no duration. The gateway already maps
  `session_id` to a chat thread and a persona, so the persona name is not
  needed, and a name or count is one more field a test has to prove clean.
- **RECOMMENDATION** There is no `truncated` field and no cap branch: the body
  is a few hundred bytes by construction (section 8).
- `session_id` is validated by `validate_session_id` (`[A-Za-z0-9_-]`, at most
  64), so it is always inside the delivery-id charset; it may be chosen by the
  client on resume but is always validated. `approval_id` is already inside the
  charset (#911 section 2). The producer keeps a defensive refusal (counted, one
  stderr line with the code `bad_id`) that should be unreachable.

## 4. Which turn outcomes emit

Source: `TurnOutcome` in `crates/tole-core/src/turn/mod.rs`. **Where it is
observed matters.** The serve routes never see `TurnOutcome`:
`run_session_turn_detailed` (`session/host.rs`) consumes it and returns a
`TurnResult` with a collapsed `stop` string (`stop_reason` folds
`StopGateBlocked`, `ApprovalRequired`, `UnknownTool`, `ProviderFailed` and
`LoopDetected` into `"refusal"`); a `Storage` error or a runner error returns
`Err(String)`; the resume path returns only `(String, Option<String>)`. So:

- **RECOMMENDATION** The notifier hooks inside `host.rs`, next to `stop_reason`
  and `log_outcome`, or `TurnResult` and the resume return gain a small typed
  `TurnEnd` enum built from `&TurnOutcome` with every payload string discarded.
  There is one exhaustive match (no wildcard arm); this is a new rule, since
  `stop_reason` collapses today. Passing a `message`, `name`, `tool` or `reason`
  string through to the event is the leak the typed enum exists to prevent.
- An `Err` from the turn call maps to `failed` with `code = "storage"` or
  `"internal"`, and the string is dropped. A busy 409, an invalid id and any
  request rejected before a turn starts emit nothing.
- The mapping below is a closed set; an unmapped variant would be `failed` with
  `code = "internal"`.

| `TurnOutcome` | `status` | `code` | note |
|---|---|---|---|
| `Final` | `completed` | none | the answer is in the log at `entries` |
| `ProviderFailed` | `failed` | `provider` | the provider message is never sent (it can echo a URL, key hint or response text) |
| `Storage` | `failed` | `storage` | error text never sent |
| `Cancelled` | `cancelled` | none | the owner cancelled the turn |
| `Detached` | `detached` | none | **DECISION NEEDED** (D4): emit, or fold into `cancelled` |
| `BudgetExhausted` | `budget_exhausted` | none | |
| `LoopDetected` | `blocked` | `loop` | the repeated tool name and count are not sent |
| `UnknownTool` | `blocked` | `unknown_tool` | the requested tool name is model output and is not sent |
| `StopGateBlocked` | `blocked` | `stop_gate` | the gate reason is durable in the log but not sent |
| `ApprovalRequired` | `approval_pending` | none | only when a Pending queue entry exists; see below |

**Approval stop.** `approval_requested` already fires when the entry is queued.
Whether the turn's end also emits `turn_finished(approval_pending)` is a choice:

- `ApprovalRequired { name }` carries no approval id, and the same outcome also
  covers a denied or hook-refused call. The id is read from the queue entry
  (`queued_since` / `approval_required_body` in `serve.rs`), never from the
  outcome. **RECOMMENDATION** emit `approval_pending` only when a Pending entry
  exists, with its `approval_id`; when none exists emit `blocked` with
  `code = "approval"` (added to D6). The resume path has no id plumbing today
  and needs it (section 12).
- **RECOMMENDATION** Emit it. Then every turn leg ends with exactly one
  terminal event, the gateway has a single rule ("stop waiting on this leg"),
  and a lost `approval_requested` can still be recovered from the later
  `approval_pending` plus `approval_id`. The cost is one extra small event per
  approval. (**DECISION NEEDED**, D3: the alternative is to leave
  `ApprovalRequired` silent because the earlier event covers it.)

**Which sessions.** **RECOMMENDATION** Only turns that run in a `tole serve`
session started or resumed through the serve routes. Cron agent runs already
have `cron_run_finished`; ACP and the TUI have no outbound webhook sink, so this
is a serve-only item and needs a row in the capability table of
[faces.md](faces.md) (status `policy`, with the reason; AGENTS.md section 6),
with `face_parity` passing. Mission turns (`POST /missions`) start outside
`run_session_turn_detailed`; whether they emit is part of D5 (recommended: no in
v1). ACP and MCP-HTTP share the host turn path, so the hook is gated to serve. (**DECISION NEEDED**, D5.)

**Never emitted:** a turn that never started (a rejected request), a
`webhook_test`, and any state that is not a turn end. A turn that continues after
an approval produces a later `turn_finished` for the later settle; it is a new
event with a new id.

## 5. Opt-in per destination

- **FIXED** `events` is the existing allow-list in `[webhook_out.<name>]`.
  `turn_finished` is a new accepted name. Default stays
  `["cron_run_finished"]`: a destination that does not list it never receives
  it, and an existing config behaves exactly as before.
- **RECOMMENDATION** Same loader rule as approval events: when no destination
  lists `turn_finished`, the turn path installs no notifier and is unchanged
  (no allocation, no hook).
- Downgrade note for the CHANGELOG: an older binary rejects an unknown event
  name, so a config that lists `turn_finished` fails to load on it, by design
  (`events` rejects unknown names, #911 section 10 item 10).
- The cron outbox rule "`body.type` is in that destination's `events`" applies
  unchanged wherever a line is re-validated.

## 6. Signing, delivery and idempotency

- **FIXED** Same v2 HMAC (`v2\n<ts>\n<id>\n<event>\n<body>`), same headers, same
  per-attempt timestamp, same address policy, no redirects, same response
  handling (status only), same `X-Tole-Delivery` equal to `body.id`. The pinned
  test vector of #911 section 3 is unchanged; the implementation adds a second
  vector for a `turn_finished` body to the test list.
- **Idempotency key (`id`).** **RECOMMENDATION**
  `turn:<readable>:<hash>:<entries>`: `<readable>` and `<hash>` are produced
  from the session id by the same function as the cron id (readable part with
  every byte outside `[A-Za-z0-9._-]` replaced, cut to 64; first 16 hex digits of
  SHA-256 of the full session id), and `<entries>` is the log length at settle.
  Reasons: it is a pure function of durable state, so it is the same across
  retries and a restart; every settle of a session has a different `entries`
  (the log is append-only, [AGENTS.md](../AGENTS.md) section 6), so a resumed
  leg never collides with the first one; no turn counter has to be invented.
  At most 5 + 64 + 1 + 16 + 1 + 20 = 107 characters, inside the 128 cap. Reuse
  the cron helper rather than copy it (one small shared item, as #911 section 9
  PR 1 already plans for `sha2`).
- The receiver dedupes on `(destination, id)` for at least 24 h, as for every
  event.

## 7. Ordering against the approval events

**FIXED** (#911 section 4): no ordering is guaranteed on the wire. What this
note adds is the order the producer enqueues, so a receiver can reason about it:

1. `approval_requested` is enqueued when the entry is queued, before the turn
   stops.
2. The turn stops: `turn_finished(approval_pending, approval_id)` is enqueued.
3. The owner decides: `approval_resolved` is enqueued at decision time.
4. The resumed turn ends: `turn_finished(completed | ...)` with a larger
   `entries`.

On the wire these can still arrive in any order, because a failed event is
rescheduled while later ones go on. Receiver rules (**RECOMMENDATION**, for the
gateway and any other receiver):

- Key turn state by `session_id`. A `turn_finished` whose `entries` is not larger
  than the cursor the receiver already holds is stale and is ignored (it was
  already read by polling, or a later leg overtook it).
- `approval_resolved` does not mean the turn is done. Only `turn_finished` with
  a larger `entries` does.
- A `turn_finished(approval_pending)` that arrives after the matching
  `approval_resolved` is stale for the same reason.

## 8. Size cap and backpressure

- **Size.** **FIXED** the 16 KiB envelope cap and the receiver's 64 KiB read cap stay. **RECOMMENDATION** the implementation adds a test that the largest
  possible `turn_finished` body (longest ids, longest destination name,
  largest `entries`) is at most 1 KiB, so a cap can never cut it and no
  truncation path exists.
- **Producer.** **FIXED** it never does network I/O: it builds the typed body,
  redacts and serializes it, enqueues, returns. It runs on the turn's thread
  after the turn settles, outside the entries and session locks. A full or dead
  queue never delays the turn, never changes its outcome and is never reported to
  the model.
- **Queue and retry.** **DECISION NEEDED** (D1). Approval events are volatile
  because a pending approval does not survive a restart, so replaying one would
  announce something that cannot be decided. A turn end is different: the session
  and its answer are durable, and a lost `turn_finished` leaves a user waiting
  for a reply that already exists.
  - Option A, **RECOMMENDED for v1**: the existing volatile bounded in-memory queue like the
    approval events (256 per destination, counted), with a longer retry
    horizon than approvals (2 s, 10 s, 30 s, 2 min, then every 5 min up to 15 min,
    then dead), and the gateway reconciles through
    `GET /sessions/{id}/status` at start and after a reconnect, which it
    already does for approvals. No outbox, no ledger row, no reconcile pass. It is
    not free: today's queue is drop-newest only, has a fixed 900 s deadline per
    event and a fixed 2/10/30/120 s backoff, and the sender thread starts only
    when a destination lists an approval event. Option A therefore changes
    (a) the sender start condition to include `turn_finished`, (b) the per-event
    deadline and backoff tail, (c) the overflow rule of D8, plus a new doctor
    counter. These are changes to a reviewed path.
  - Option B: the durable cron outbox. Survives a serve restart and gives the 6 h
    horizon, but the outbox, its validation rules, its reconcile pass and its
    ledger rows are keyed to a cron `fire_id`; a turn event has none, so this is
    a real extension of a reviewed path.
- **Overflow policy.** **RECOMMENDATION** when the per-destination queue is full,
  drop the **oldest** `turn_finished` for a session that has a newer one queued
  (only the latest settle of a session matters to a receiver), and otherwise
  drop the newest, counted and shown in `tole doctor` and as one stderr line with
  the destination name and the code `queue_full`. This bounds the queue by the
  number of live sessions instead of by turn rate.
- **Rate.** One event per settled turn. No new flooding vector beyond what the
  owner's own sessions produce; the existing one-request-in-flight-per-destination
  rule applies.

## 9. Redaction, what is never sent, and the leak-check plan

**What cannot leave** (restating #911 section 6 for this event): transcript
text, the final answer, tool arguments or output, the prompt, the provider
message, the tool name from `UnknownTool` or `LoopDetected`, the stop-gate reason,
the storage error, paths, model and provider names, the destination URL and key.

**Design for leak resistance** (**RECOMMENDATION**):

- The payload is a Rust struct whose fields are an id type, an enum, an integer,
  and an optional id. There is **no `String` field that can hold free text**, so
  a future change cannot route model output into it without changing the type.
  Closed words are enums serialized to fixed strings.
- It still goes through the same redaction order as every event (redact every
  string, then cap, then a second pass over the serialized envelope), so the
  destination key and URL secrets are covered even for an id that is somehow
  secret-shaped.
- An agent can influence only which `status` word appears (by causing a failure,
  a loop or a stop-gate trip); it cannot choose a string value. That is the same
  trust position as `approval_resolved.status`.

**Test plan** (all in the implementation PR; the design note adds none):

- *Unit, schema:* serialize every `TurnOutcome` mapping and assert the exact key
  set per status (an allow-list test, so a new key fails it), every string value
  is an id or a member of the closed vocabulary, and the body is at most 1 KiB.
- *Unit, mapping:* a table test over every `TurnOutcome` variant, plus a test that
  adding a variant without a mapping does not compile (no wildcard arm).
- *Unit, id:* charset, length at the maximum, determinism, two settles of one
  session differ, two sessions that collide under a lossy rename do not collide.
- *Unit, opt-in:* a destination without `turn_finished` in `events` receives
  nothing and no notifier is installed; the default `events` is unchanged.
- *Real-binary e2e* (`crates/tole-cli/tests`, the built `./target/release/tole`,
  a receiver that verifies v2 independently, with the mock provider): a turn that
  completes yields one verified `turn_finished`; after an Allow the resumed turn
  yields `approval_resolved` then `turn_finished` and the receiver never needs a
  status read; a second approval yields `approval_pending`; cancel yields
  `cancelled`; a provider failure yields `failed` with `code = "provider"`;
  failure injection as in #911 (500 then 200 with the same id and body and a new
  timestamp, 400 no retry, a hung receiver does not delay the turn, a killed
  receiver and a restarted serve).
- *Leak check (#781 pattern), scoped to this event:* the existing `log_outcome`
  stderr line already prints provider messages, tool names and gate reasons
  (`host.rs`), so the stderr assertion covers the notifier's own lines only and
  `log_outcome` is a named pre-existing sink (a separate item may redact it). a canary secret placed in the environment, the
  user prompt, a tool result, the assistant's final text, the provider error
  message, the stop-gate reason and an unknown tool name; neither the canary nor
  the destination key may appear in any received body, the outbox if used, the
  ledger, doctor output, stderr or `config check`. The test fails if the canary
  appears in any `data` value, and a separate assertion proves the final-text
  canary was in the session log (so the test cannot pass vacuously).
- *Docs in the same PR:* a threat-model row (new event does not widen the egress
  set: ids and a status word), the design doc event table, CHANGELOG, `faces.md`
  row, README status line and `docs/epics.md` (AGENTS.md section 7).

## 10. What the gateway does with it

Consumer rules, for `tole-gateway#16` and #969 (**RECOMMENDATION**, the gateway
repository decides its own implementation):

- Verify the delivery exactly as for the other events (signature, window,
  `destination`, dedupe on `id`), drop a bad one without a reply.
- On `turn_finished` for a `session_id` it knows, look up the origin thread (the
  persona session map of #969), then make **one** authenticated read of the log
  entries after its cursor (up to `entries`) and render the assistant's answer
  there as plain text. The event carries a pointer, never the answer.
- By `status`: `completed` shows the answer; `cancelled`, `budget_exhausted`,
  `failed`, `blocked` show a fixed gateway-written line chosen from the status
  word and `code` (never text from the event, which has none);
  `approval_pending` waits for the approval card it already shows.
- The one-second status poll after an Allow is no longer needed when the
  receiver is on. Polling stays as a **slow fallback** (for example every 30 s
  while a turn is known to be running) and as the only path when the receiver is
  not configured, which keeps the "works when the receiver is off" criterion of
  `tole-gateway#16`.
- Because delivery is at-least-once with no ordering, the gateway applies the
  `entries` cursor rule of section 7, so a duplicate or stale event is a no-op.
- It must not treat the event as authority: nothing in it decides an approval or
  selects a persona; routing comes from its own session map and config.

## 11. Open questions for the owner

Each has a recommendation; none is decided by this note.

- **D1 Durability.** Volatile queue with a longer retry horizon (A,
  recommended) or the durable outbox (B)? Section 8.
- **D2 Cursor.** The receiver pages entries by `Entry.seq` (`last_seq` /
  `next_after`), and `seq` is not obviously equal to the entry count (the
  `entries` field of `GET /sessions/{id}` is a count and is `null` while a turn
  runs). Recommended: name the field after what the receiver pages by
  (`last_seq`) and use it as the idempotency suffix; verify against the log
  before accepting section 6. A separate turn counter is the alternative and
  needs new state.
- **D3 Approval stop.** Emit `turn_finished(approval_pending)` (recommended) or
  stay silent because `approval_requested` covers it?
- **D4 `detached`.** Keep it as its own word (recommended, it is a different owner
  action) or fold it into `cancelled`?
- **D5 Scope.** Serve sessions only (recommended), or also cron agent runs
  that end a turn? Cron runs already emit `cron_run_finished`, so a second event
  would double-notify.
- **D6 Status vocabulary.** Approve the closed set `completed`, `failed`,
  `cancelled`, `detached`, `budget_exhausted`, `blocked`, `approval_pending` and
  the `code` words `provider`, `storage`, `loop`, `unknown_tool`, `stop_gate`,
  `internal`, `approval`. Adding a word later is additive; renaming one is a `v` bump.
- **D7 Persona field.** Keep `profile` out of the body (recommended) or add the
  profile name so a receiver without a session map can route? It is
  config-derived and not agent text, so it is safe, but it is one more field.
- **D8 Overflow rule.** Replace-oldest-per-session (recommended) or plain
  drop-newest like the approval events?
- **D9 Retry horizon.** 15 minutes for Option A is a guess; the owner may prefer
  matching the approval schedule (about 2.7 min) or the cron 6 h.

## 12. Implementation plan (for the next step, not this PR)

1. `feat(cli)`: `Event::TurnFinished` in `webhook_out/mod.rs` (`ALL`, `as_str`,
   `parse`), `TurnFinished` payload type
   with the closed enums and the id function (shared with cron), the outcome
   mapping with no wildcard arm, schema and mapping unit tests.
2. `feat(serve)`: the typed `TurnEnd` through `host.rs` and the resume return,
   approval id plumbing for the resume path, the sender start condition,
   notifier at the turn settle in the serve runner, queue policy
   from D1 and D8, `faces.md` row, e2e with failure injection and the leak check,
   threat-model row, design-doc table, CHANGELOG, README status line,
   `docs/epics.md`. Needs an **independent review** (it touches the outbound data
   path and the turn runner) and `cora callers` / `cora impact` output for the
   touched public items.
