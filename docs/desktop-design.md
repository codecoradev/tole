# tole-desktop: local and remote over the same serve API (design note, #949)

**Status: DESIGN, nothing here is decided.** Step 7 of epic #941. Every
decision in the [decision table](#4-decisions-open-for-the-owner) is **OPEN FOR
THE OWNER**; the "recommended default" column is a proposal, not a record. No
code follows from this note until the owner answers and the UI stack is
un-parked (#910 says UI work stays postponed until the owner commands it).

Inputs: #949 (this issue), #941 (parity epic), #527 (GPUI desktop), #523 (UI
stack epic), #561 (UI/UX plan), #910 (gateway), #742 (serve auth),
[mobile-control.md](mobile-control.md) (the route reference),
[daemon-design.md](daemon-design.md), [threat-model.md](threat-model.md),
[faces.md](faces.md). The visual draft (a Design canvas, 11 boards) is not in
the repository and was not read for this note; the screen mapping below is
derived from the TUI inventory in #527/#561 and from the routes, and the owner
should check it against the canvas.

## 1. One API, two transports

The desktop client is a thin client of `tole serve`. It uses the **same
routes** whether the server is on the same machine or on another one. The only
differences between local and remote are:

1. the address (loopback versus a name or address the owner exposes),
2. how the credential is stored and how the link is protected (TLS or a
   private network, section 4),
3. how the server process is started and kept alive (section 3).

This is the face rule of [faces.md](faces.md) applied to a client: a face may
differ only in identity and authentication, in how input and output are shown,
and in how an approval is delivered. The desktop adds no route that only it
uses.

Note on #527. The #527 text says the default host is the **in-process session
host** with a remote adapter beside it. This note is written for the serve
route as the contract for both; whether a local desktop should use serve at all
or the host as a library is decision D2.

### 1.1 Routes a desktop needs

Every route below exists in `crates/tole-cli/src/serve.rs` (routing in
`route`, plus `serve_events`, `upload_image` and `read_image` in
`handle_conn`) and is documented in [mobile-control.md](mobile-control.md),
unless the status column says otherwise.

| Desktop need | Route | Status |
|---|---|---|
| Liveness probe (no auth) | `GET /health` | exists |
| Live sessions | `GET /sessions` (`id`, `busy`, `profile` when set) | exists; **live sessions only** |
| New session, optionally plan mode or a profile | `POST /sessions {cwd, plan_mode?, profile?}` | exists (#947, #740) |
| Reopen a session | `POST /sessions {"resume": id}` | exists (#524) |
| Send a prompt (blocks until the turn ends) | `POST /sessions/{id}/prompt {text, images?}` | exists |
| Live stream | `GET /sessions/{id}/events?after=<seq>` (SSE) | exists (#524) |
| History, paged | `GET /sessions/{id}/entries?after&limit` | exists (#524) |
| Status, usage | `GET /sessions/{id}/status` | exists; `entries` and `usage_report` are `null` while a turn runs; `model`, `effort`, `contextWindow`, `contextUsed`, `name` come from a last-known snapshot (`stale: true`, `snapshotAt`) then (#573) |
| Cancel, steer, detach a turn | `POST /sessions/{id}/cancel`, `/interrupt`, `/detach` | exist |
| Approvals queue | `GET /approvals`, `GET /approvals?session=<id>` | exists |
| Decide an approval | `POST /approvals/{id}/decision` | exists |
| Delegated children's approvals | same two routes, `dap-` ids | exists (#714) |
| Prompt stopped for an approval | `stopReason: "approval_required"` plus the pending row | exists (#571) |
| Images | `POST /sessions/{id}/images`, `GET /sessions/{id}/images/{hash}` | exists (#558) |
| Fork, tree | `POST /sessions/{id}/fork`, `GET /sessions/{id}/tree` | exists (#947) |
| Compact, rename | `POST /sessions/{id}/compact`, `POST /sessions/{id}/name` | exists (#947) |
| Chained mission with a budget | `POST /missions`, `GET /missions/{id}`, `POST /missions/{id}/cancel` | exists (#947) |
| Session list of past sessions (name, folder, last activity) | `GET /sessions/index?limit&cursor` | exists (#572); `GET /sessions` still lists only sessions live in the server process |
| Model list, set model, set thinking level | `GET /models`, `POST /sessions/{id}/model`, `POST /sessions/{id}/effort` | exists (#574) |
| Remove or archive a session | none | not planned in any issue read for this note |
| Server version or capability probe | none | not planned; `/health` returns only `{"status":"ok"}` |
| Push notification of a pending approval | none | by design (threat model: "no push endpoint in tole"); webhook delivery (#911) exists for the gateway |

I checked the route table by reading the `route` function and the `handle_conn`
dispatch; I did not run the server.

### 1.2 Client contract (what every client must handle)

These come from #949, #561 and [mobile-control.md](mobile-control.md), and were
re-read against `serve.rs`.

- **Two connections.** `POST /prompt` holds its connection until the turn ends
  (the turn is run inside the request handler), and progress arrives only on
  the SSE stream. A client opens both.
- **Streams are capped.** At most 8 SSE streams per server and 32 connections
  in all (`MAX_STREAMS`, `MAX_CONNECTIONS`). A connection over the cap is closed
  with no HTTP response, so a client sees a reset or a timeout, not a `429` or
  `503`, and must retry with a delay. A desktop with several open
  sessions must not hold one stream per session indefinitely; this matters more
  when a phone or a gateway shares the same server.
- **Resumed turns have no observer.** After an `allow` answers `202`, the turn
  resumes on a server thread with no HTTP response. Its entries arrive as
  durable frames only (no text deltas), and a further approval is not announced
  by `approval_required`. The client watches `approval_requested` frames or
  polls `GET /approvals?session=<id>` after every allow.
- **`usage_report` and `entries` are `null` while a turn runs** in
  `GET /status`. Read token numbers from the stream and `entries`. The footer's
  model, effort, context window and name stay available: they come from the
  last-known snapshot, flagged `stale: true` with a `snapshotAt` (#573).
- **Ephemeral frames are best-effort**; the durable `entry` frames are the
  truth, and `?after=<seq>` (or `Last-Event-ID`) replays what was missed.
- **`seq` is not dense**; never assume `+1`.
- **Only the first approval decision counts**; a repeated or opposite decision
  answers as documented, so a retry after a lost response is safe.
- **`Destructive` tools are never auto-allowed** (`docs/threat-model.md`), and
  a client never offers "always allow" for them (#523 constraint).

### 1.3 Screens and readiness

The acceptance criteria of #949 ask for a screen-to-route mapping. The screens
below are the TUI-parity components named in #527 and the inventory in #561
(session list, transcript, editor/composer, footer, approval panel, model
picker, empty and error states). I did not see the canvas, so this table covers
those screens only.

| Screen or component | Calls | Readiness |
|---|---|---|
| Session list (live) | `GET /sessions` | exists |
| Session list (past sessions, picker) | `GET /sessions/index` | exists (#572) |
| Transcript | `GET /entries`, SSE `entry` frames | exists |
| Streaming text, tool cards | SSE `text_delta`, `tool_started`, `tool_finished` | exists; only for a turn that starts while a stream is open |
| Composer, send | `POST /prompt` | exists |
| Steer, follow-up queue | `POST /interrupt` | exists |
| Stop | `POST /cancel` | exists |
| Approval card | `approval_required` row, `GET /approvals`, `POST /decision` | exists |
| Footer (status, usage) | stream frames, `GET /status` | exists; during a turn `usage_report` is `null`, the model, effort and window are a `stale` snapshot (#573) |
| `@file` panel | none | not planned: serve has no file listing or search route (needs an owner decision on exposing the workspace tree) |
| Image attach and view | `POST /images`, `GET /images/{hash}` | exists |
| Model picker, thinking level | `GET /models`, `POST /model`, `POST /effort` | exists (#574) |
| Fork, tree, compact, rename | `/fork`, `/tree`, `/compact`, `/name` | exists |
| Mission panel | `POST /missions` and siblings | exists |
| Settings, profiles | `profile` on `POST /sessions` | partial: there is no route that lists the defined profile names |

## 2. Local desktop

### 2.1 What exists today

`tole serve` binds `127.0.0.1` by default (`--bind` overrides), listens on a
TCP port (default 7801 in the docs examples), and refuses to start without a
credential: a bearer token from `--token-file`, `--token` or `TOLE_SERVE_TOKEN`
(`serve_token`), or Basic (`--auth basic`). There is no Unix-socket listener in
`serve.rs`. The token is **supplied by the owner**; the code does not generate
one. The live-session map and the approvals queue are in memory; the session
JSONL files are what survive a restart. A session lives under the workspace it
was created in, and `POST /sessions` takes a `cwd` that must canonicalize
inside the server's workspace root (`--workspace`, default the server's
working directory).

### 2.2 Proposed local flow (all of it depends on D1 to D4)

- **Attach first.** The desktop reads its saved endpoint and token and calls
  `GET /health`. If it answers, it attaches. It never starts a second server on
  the same port.
- **Start only if absent and only if D3 allows it.** The desktop spawns
  `tole serve` as a child with `--bind 127.0.0.1` and a fixed port, passes the
  token through `--token-file` (never `--token`: the argument is readable by
  every local process, and `main.rs` already warns about it), and sets
  `--workspace` to the folder the user opened.
- **Workspace.** One server has one workspace root. The desktop picks the
  folder in its own UI and starts or attaches to a server for that folder.
  Opening a second folder means a second server on another port (or a
  restart), unless the owner decides the server should root at a parent folder.
  This is a consequence of `--workspace`, not a feature to add silently.
- **Lifecycle owner.** If the desktop started the process, the desktop owns it
  and stops it on quit. If the user started it (a terminal, a service), the
  desktop only attaches and never stops it.
- **Crash or restart.** The sessions are in JSONL, so nothing durable is lost.
  After a restart the live map is empty: the desktop must `POST /sessions
  {"resume": id}` for each session it shows, and `GET /entries` returns `404`
  until it does. A turn in flight when the process died is recoverable through
  the existing crash-resume (see [daemon-design.md](daemon-design.md)); the
  client treats it as a resumed session. A pending approval is in memory and is
  gone; the resumed turn asks again. Approvals expire to denied after 900 s
  (`EXPIRY_SECS`).
- **Writer lock.** A session has one writer. If the user has the same session
  open in a TUI, resume answers `409`; the desktop shows that and does not
  retry silently.

### 2.3 Local threat-model row (proposed for `threat-model.md`)

| Threat | Today | Residual |
|---|---|---|
| Another local user or process reaches the loopback port | The credential is required on every route but `/health` and the webhook routes (which keep their own HMAC gate); failures are limited to 10 a minute per IP and compared in constant time. | Loopback is shared by every local user: anyone who can read the token can approve writes. The token file must be mode `0600` in a directory only the owner can read. `read_plain_file` refuses links and FIFOs but I did not verify that it checks file permissions, so the note does not claim it does. |
| Another local user reads the token from a process list | `--token` is warned about; `--token-file` and `TOLE_SERVE_TOKEN` are the documented carriers. | An environment variable is readable by the same user's processes and, on some systems, by root only; the desktop should prefer a file. |
| A port squatter binds the port first | None. The client sends the token to whatever answers. | A desktop that sends the bearer token to a loopback port it did not start can hand it to another local process. Mitigation to decide (D1): a Unix socket in a `0700` directory, or a per-start random port plus a token the child receives. |

The unattended side stays the same: `tole serve` calls
`sandbox::mark_unattended()`, so an `optional` sandbox counts as `required`.

## 3. Remote desktop, without any vendor relay

### 3.1 The reach question and the recorded owner statement

See [the record](#the-no-private-reach-decision-where-it-is-recorded) below.
This note assumes the decision as recorded and asks the owner to confirm what
it covers (D5).

Reaching a `tole serve` on another machine is the **owner's own network
arrangement**, outside tole:

- a TLS reverse proxy the owner runs in front of `serve` (the arrangement
  `--auth basic` already assumes), or
- a private network the owner already uses (a WireGuard or Tailscale-like VPN,
  or an SSH tunnel; named as examples, not as supported or recommended
  products).

tole ships no relay, no rendezvous server, no NAT traversal and no hosted
endpoint, and this note adds none.

### 3.2 What the server already enforces

All checked in the code or in the docs cited:

- Credentials are mandatory; `serve` refuses to start without one.
- One `Authenticator` decides every route except `/health` and the webhook
  routes (which keep their own HMAC gate), including the SSE stream and image
  upload.
- Failed authentications are counted per source IP: 10 a minute
  (`MAX_AUTH_FAILURES`, `AUTH_WINDOW_SECS`), then `429` for everything from that
  IP. Behind a reverse proxy every client shares the proxy's address, and tole
  does not read `X-Forwarded-For`, so rate-limit at the proxy as well.
- Request bodies are capped at 1 MiB before authentication; an upload up to
  10 MiB is accepted only with a valid credential. Header block cap 32 KiB.
- Read deadline 15 s (120 s for an authenticated upload), IO timeout 30 s, 32
  concurrent connections, 8 SSE streams, a heartbeat every 15 s
  (`TOLE_SERVE_HEARTBEAT_SECS`, 1 to 3600).
- `--auth basic` with a non-loopback `--bind` is refused unless
  `--allow-insecure-basic` says TLS ends in front. The static token has no such
  refusal.
- Credentials are held as role secrets (redacted at the egress sinks). This note
  did not audit every log site, so it does not claim a credential is never
  logged.
- The token file is opened without following a link on its last path component
  and must be a regular file of at most 4 KiB. It is **not** checked for mode or
  owner: a world-readable token file is accepted silently, so the desktop must
  create it `0600` itself.
- Session creation is jailed to `--workspace`.

### 3.3 What is missing, and what is out of scope

- **TLS termination is out of scope for tole itself.** tole serves plain HTTP.
  A remote desktop needs `https://` from the owner's proxy or a link that is
  already encrypted (a VPN or tunnel). A bearer token over plain HTTP on an
  untrusted path is readable by anyone on the path.
- **No scopes.** Every credential is full authority, approvals included (see
  [mobile-control.md](mobile-control.md#auth-model)). A lost laptop is a lost
  approval authority until the token or hash is replaced and `serve`
  restarted. A read-only credential is part of #742 and is not built.
- **No per-device token or revocation** short of restart.
- **One user, no identity bound to a session.**
- **No certificate pinning or server identity check** is defined for the
  desktop client. With a proxy the client uses the system trust store; with a
  private CA the owner must add it. This is client work for #527 and a point
  for the owner (D6).
- **In-memory failure limiter**, reset by a restart.
- **The 8-stream cap is shared** by every client of the server.

### 3.4 Failure modes on a flaky link

| Situation | Behavior | Client rule |
|---|---|---|
| SSE drops | The durable log is the truth. Reconnect with `?after=<last seq>` (or `Last-Event-ID`) and receive exactly the missed entries, then `ready`. | Keep the last seen `seq`; back off between reconnects. Ephemeral text deltas of the gap are lost; the final entry is not. |
| `POST /prompt` connection breaks mid-turn | The turn runs inside the request handler on the server; I found no code that watches the client socket during a turn (no `peek`, `shutdown` or `take_error` on the stream, and the only cancel paths are the cancel routes), so a dropped client does not cancel it; this is an absence of code, not a tested behavior. The response cannot be delivered, and a write failure is bounded by the 30 s write timeout. | After a break, reopen the stream, read `GET /entries`, and check `busy` through `GET /sessions`, or through `POST /sessions {"resume": id}`, which answers `alreadyLive` and `busy` for a session that is live. Do not re-send the prompt without checking, or the turn runs twice. This behavior should be confirmed by a test before the client relies on it (not verified by this note). |
| Client offline while an approval is requested | The request waits in the queue up to 900 s, then expires to denied. | On reconnect, `GET /approvals`. A request that expired is shown as denied; the turn is not stranded. |
| Approval allowed, then link drops | `202` means the resume runs; it has no observer. | Poll `GET /approvals?session=<id>` and `GET /entries` after reconnect. |
| Server restarts | Live map and queue are empty; sessions are on disk. | `POST /sessions {"resume": id}`; reload entries. |
| Slow client | A bounded per-stream queue drops ephemeral frames and sends `resync`. | On `resync`, re-read `GET /entries?after=<last seq>`. |
| Proxy idle timeout shorter than 15 s of silence | The proxy may cut the stream. | Lower `TOLE_SERVE_HEARTBEAT_SECS` on the server. |
| Wrong credential repeated | `429` after 10 failures a minute from that IP, for every request including the right one. | Stop retrying with a bad credential; a locked-out client also locks out others behind the same proxy. |

## 4. Decisions open for the owner

Every row is **OPEN FOR THE OWNER**. None is decided by this note. The
recommendation is the default this note would implement if the owner says "go
with your proposal".

| # | Decision | Options and consequence | Recommended default |
|---|---|---|---|
| D1 | Local transport | (a) Loopback TCP, as `serve` is today: no new code; any local user or process can connect to the port and a squatter can receive the token. (b) A Unix socket in a `0700` directory: filesystem permissions are the boundary (the reference in [daemon-design.md](daemon-design.md)); needs a new listener in `serve`, a Windows story (the desktop targets Windows too, #527), and a second code path. (c) The session host as a library, no HTTP (the #527 text; #525 postponed a `tole-session` crate "until the GPUI client needs the host as a library"): fastest, but the desktop then has two paths (library locally, HTTP remotely), which is the drift #941 removes, and a local desktop could not share a session with a phone. | (a) loopback TCP over the same API. It keeps one contract for local and remote and needs no change in `serve`. Revisit (b) only if a local-user threat is a stated requirement. |
| D2 | Does a local desktop use serve at all, or the in-process host? | Same question as D1(c). The in-process host avoids a token and a port; serve gives one code path and lets another device drive the same sessions. | Serve for both, so the remote adapter is the only adapter. Keep the host-as-library path open behind the same interface #527 describes, but do not build it first. |
| D3 | May the desktop start `tole serve` itself? | (a) Yes, as a child it owns: simple first run; the desktop then owns a process that can run tools, and must find the right `tole` binary. (b) No, the user starts it: the desktop only attaches; more setup, no process ownership, and the user sees what is running. | (a) allowed, with explicit consent on first run, loopback only, token by file, stopped when the desktop quits, and never started when a server already answers `/health`. |
| D4 | Where the token lives | (a) A `0600` file in the user's codecora config directory, generated by the desktop with a CSPRNG on first run and passed with `--token-file`. (b) The OS keychain, with the desktop writing a temp file only for the child. (c) Prompt each time. | (a) for local; (b) for a saved remote credential, if the GPUI stack gets a keychain binding, else (a). Never `--token` and never logged. |
| D5 | Remote reach versus the recorded decision | (a) The owner's own means (TLS proxy, VPN, tunnel); tole ships no reach mechanism, only authentication (#742). Consistent with the decision recorded in #941 (see below). (b) Through the gateway (#910) as a relay: not supported by #910 as written (the gateway is a Telegram bot, a client of serve, and relays no serve API); it would be a new relay feature in a separate repository, and it would put the gateway in the path of every approval and every transcript. | (a). Treat (b) as a separate, later epic if ever wanted. Confirm that "private-reach mechanisms are out of scope" also rules out tole shipping a Tailscale integration, a hosted relay and a built-in tunnel, and allows only documentation of the owner's own arrangement. |
| D6 | Remote TLS expectations | (a) The desktop requires `https://` for any non-loopback host and refuses plain `http://`. (b) Allows `http://` with a warning. (c) Pinning of a self-signed certificate or private CA. | (a), with an explicit "I am on a trusted private network" override per saved server (a VPN already encrypts). Pinning later, if the owner runs a private CA. |
| D7 | Remote credential type | Static token (exists), Basic (exists, #742 first slice), OIDC and scopes (not built, #742). | Static token or Basic for the first release. Do not start OIDC for the desktop; a read-only credential is the first thing worth adding because the remote desktop is the first client that can be lost. |
| D8 | One server per workspace or a parent workspace | One `--workspace` root per server (today) versus rooting at a parent folder so one server serves several projects. A parent root widens the jail every session shares. | One server per project folder, started by the desktop (D3). |
| D9 | Is the model picker part of the first release | Asked in #574. If yes, #574 goes before #527; if no, a fixed model ships. | Fixed model first; keeps #574 off the critical path. |
| D10 | Past-session list scope | Asked in #572: workspace-only or the machine-wide index. | Workspace-only (the recommendation already in #572). |
| D11 | Credential scopes for remote | The remote desktop holds full approval authority. | Document it in the client's first-run text and in the threat model; add a read-only scope under #742 before remote is advertised. |

## 5. Threat-model rows for a remote desktop client

Proposed rows (not yet added to [threat-model.md](threat-model.md); the
decisions above could change them):

| Threat | Control that exists | Residual |
|---|---|---|
| Sniffing or replaying the credential on the path | Credentials are required; Basic is refused off loopback without `--allow-insecure-basic`. | tole serves no TLS. Without the owner's TLS or private network the credential is readable on the path. The desktop's D6 rule is the only client-side control. |
| Guessing the credential | Constant-time compare, 10 failures a minute per IP, `429`. | A proxy collapses every client into one IP: one attacker locks out the owner for a minute. Rate-limit at the proxy as well. |
| Stolen or lost desktop | The credential is a revocable secret; approvals are one-shot, bound to the effect, and expire. | Full authority until the server restarts with a new credential; no per-device revocation; no scopes. |
| Hostile or compromised server | None in the client. | The desktop renders server-provided text and tool output; it must treat entries, tool previews, approval descriptions and image bytes as untrusted data (escape, no auto-open links, bounded image decode). Approval cards must show the tool, target and risk tier from the queue row, not free text from the model. |
| Approval decided on a stale view | A decision is bound to the exact (session, tool, input) fingerprint; the first decision counts; `409` on a conflict. | The client must show an expired entry as denied and re-read before enabling "allow". |
| Remote reach widening the exposed surface | Same single route table and caps for every client. | Exposing `serve` publicly exposes webhook routes (`POST /hooks/<route>`, own HMAC) and the upload path. The proxy should forward only the paths the desktop needs. |

## 6. Comparison with other agent apps

Omitted. I did not read a source about how other agent desktop apps connect
locally or remotely, and #742 only names an analogue ("Hermes desktop to remote
backend") without a link, so nothing is cited here.

## 7. Dependencies and next steps

1. Owner answers D1 to D11 (D1 to D5 gate everything; D6 to D8 gate the remote
   first run).
2. #572 (session list) and #574 (models) have landed; D9 decides whether the first release uses the model picker.
3. Add the proposed local and remote rows to the threat model once decided.
4. Decide whether the first client release needs a read-only credential (#742).
5. #527 and #561 link to this note; their acceptance wording about the
   "in-process host" should be reconciled with D1 and D2.

## The "no private reach" decision: where it is recorded

The phrase "no private reach" does not appear in the repository docs or in the
comments of #941, #949 or #910 (those issues have no comments). What is
recorded, as issue text written by the owner:

- **#941, "Constraints", "Out of scope":** "Out of scope: deriving HTTP routes
  and MCP tools from one operation table (uteke#1344 style) until a second
  client needs it; private-reach mechanisms (owner decision, 2026-11-18)."
- **#949, "Open decisions (owner)", item 1:** "The owner decision of 2026-11-18
  skipped private-reach features (Tailscale and the like) and made the chat
  gateway (#910) the only reach path."

Observations the owner should confirm (D5):

- The second sentence makes the gateway "the only reach path". #910's own list
  of decisions (dated 2026-11-17) contains no statement about private reach, and
  describes the gateway as a Telegram, then Discord, client of `tole serve`
  with no relay of the serve API. So "the gateway as the only reach path"
  covers chat access, not a remote desktop.
- The wording "private-reach mechanisms" is not defined anywhere I could find.
  Whether it excludes documenting an owner-run VPN or reverse proxy is not
  stated; this note reads it as excluding tole shipping any such mechanism, not
  as forbidding the owner from running their own.
- I searched the `docs/` tree, `README.md`, `CHANGELOG.md` and `AGENTS.md` for
  "private reach", "no private", "Tailscale" and "WireGuard": the only hit is a
  CHANGELOG line about the webhook SSRF range check, which is unrelated.
