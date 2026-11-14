# Multi-client daemon — design note (#382)

**Status: PARKED (tier B) — 2026-11-04.**

**Update 2026-11-10:** the owner approved the *serve-face* part of the UI epic
(#523, #524): a graphical client (desktop, phone, web) needs a transcript read,
resume-by-id and an event stream from `tole serve`. That is additive REST/SSE on the
existing token-authenticated face, not this document's socket daemon; the daemon
itself (client registry, idle timeout, hot-reload) stays parked. Shipped so far:
`GET /sessions/{id}/entries` ([mobile-control.md](mobile-control.md#transcript-read-524)) resume-by-id ([mobile-control.md](mobile-control.md#resume-by-id-524)) and the SSE event stream ([mobile-control.md](mobile-control.md#event-stream-524)).

**Park decision:** tole has no multi-client need today. This note records the
jcode daemon design so a future need can be evaluated against a written
reference instead of re-researching jcode. **Do not build it without an
explicit user story** (see [Un-park triggers](#un-park-triggers) and
[Anti-goal](#anti-goal)).

## How jcode does it (reference)

Source: [jcode](https://github.com/1jehuang/jcode) (MIT), design adopted as
documentation only. These are the six mechanics recorded when the adoption
issues were filed (#377–#384); they are a summary, not a code audit. Anything
finer-grained (wire protocol, registry fields, reconnect backoff) was not
captured — read the jcode source before relying on it.

1. **Single server.** One long-lived server process owns the agent sessions;
   every front end is a client of that one process rather than running its own
   engine.
2. **Unix socket.** Clients reach the server over a local Unix domain socket
   (same host, filesystem permissions as the access boundary, no network
   listener).
3. **Client registry.** The server keeps a registry of connected clients, so
   several clients can be attached at once and the server knows who is
   attached.
4. **5-minute idle timeout.** With no clients attached the server exits after
   five minutes, so the daemon does not linger indefinitely.
5. **Hot-reload via `exec()`.** To pick up a new binary the server replaces
   its own process image with `exec()` instead of a stop/start cycle driven
   from outside.
6. **Client auto-reconnect.** Clients reconnect on their own when the server
   goes away (idle exit or reload), so a restart is invisible to the user.

## If tole ever needs this (SPECULATIVE — not a plan)

Nothing here is decided; it only maps the jcode shape onto what tole already
has, to size the question.

- **Starting point is `tole serve`, not a new server.** The token-authenticated
  REST / multi-session MCP-over-HTTP daemon already exists
  (`crates/tole-cli/src/serve.rs`, `mcp_http.rs`) and already routes N
  durable sessions through `session_host.rs` — see
  [architecture.md](architecture.md). The gap versus jcode is transport
  (HTTP vs Unix socket), lifecycle (idle exit, reload) and an explicit client
  registry, not the session machinery.
- **Crate placement.** Host concerns (socket, lifecycle, registry) belong in
  `tole-cli`; `tole-core` stays platform-agnostic
  ([AGENTS.md](../AGENTS.md) §6: no stdin/stdout or CLI assumptions in core).
- **Socket path.** A per-user path under the codecora home (next to the
  sessions dir) with owner-only permissions would be the natural convention;
  not chosen.
- **Relation to the other faces.** `tole acp` and `tole mcp` are stdio
  per-client processes; a daemon would let them become thin clients of one
  engine rather than each opening their own session handle.
- **Constraints that already bind any design.**
  - Session storage is append-only JSONL with an exclusive advisory lock per
    writable session (one writer; a second is refused). A shared daemon is
    effectively the "single writer" answer; it must not weaken that.
  - Every non-ReadOnly tool still goes through an `Approver`. With several
    clients attached, *which client answers an approval* is an open question
    the single-client faces never had to answer.
  - Hot-reload via `exec()` must not strand an in-flight turn: the effect
    sandwich and crash-resume already make an interrupted turn recoverable,
    but this would need its own verification.

## Un-park triggers

Revisit only when a concrete story like one of these exists (and someone owns
it):

- Two or more agents or clients must **share one provider session / one
  durable tole session at the same time** (today the second writer is refused).
- An **editor (ACP) and a terminal CLI on the same session** — e.g. start in
  the editor, watch or steer from a terminal — where resume-after-close is not
  enough.
- A **long-lived background session** that several short-lived front ends
  (TUI, mobile control, scripts) attach to and detach from, where
  `tole serve` plus polling proves insufficient.
- Measured **per-client start-up cost** (index/model/tool warm-up) that a
  resident engine would remove.

"It would be nice" or "jcode has it" is not a trigger.

## Anti-goal

**Do not start implementation from this note alone.** It is a reference, not a
spec: the speculative section is unreviewed, and the jcode section is a
summary. Building requires, first:

1. a concrete user story from the trigger list above (or an equivalent),
2. owner approval, and
3. an issue-first design that replaces this note's speculation with decisions.

Until then, prefer configuration or the existing `tole serve` / ACP / MCP faces
([AGENTS.md](../AGENTS.md) §6: adding code is a last resort).
