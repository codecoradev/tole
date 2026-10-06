# Mobile control — driving missions from uteke-mobile (issue #202)

The 0.8.0 remote-approval surface (#200) is the entire backend a phone
needs: missions run on the box under `tole serve`, the phone lists
running missions, receives pending Write approvals, and approves/denies
from anywhere. No new backend exists or is planned — the phone is a
client of the token-authenticated serve face.

## The surface uteke-mobile consumes

All routes live under `tole serve` (bearer-token auth on everything
except `/health`; decisions revalidate per request; auth failures are
rate-limited per source IP).

| Action | Route | Notes |
|---|---|---|
| List sessions | `GET /sessions` | id + busy per session |
| Start a mission | `POST /sessions` `{cwd}` → `POST /sessions/{id}/prompt` `{text}` | the prompt is the mission goal (`tole mission` semantics chain turns; a serve prompt runs ONE turn — mission chaining over serve is a follow-up) |
| Mission status | `GET /sessions/{id}/status` | busy flag, entry count, usage totals, mission cost report (`fact/mission` register, #201) |
| **Pending approvals** | `GET /approvals` | queue entries: id, session, tool, status; expires to denied after 15 min |
| **Approve / deny** | `POST /approvals/{id}/decision` `{"decision":"allow"\|"deny"}` | allow = one-shot for exactly that (session, tool+input) + automatic resume; deny = recorded verdict |
| Cancel a turn | `POST /sessions/{id}/cancel` (#178) | the turn settles `cancelled`, durably |
| Cost report | `GET /sessions/{id}/status` | the `mission` register: turns, steps, tokens, wall time, per-risk-tier tool calls |

The round trip the app must support (the cross-repo acceptance):

1. `GET /approvals` on a poll interval (the push-channel decision —
   ntfy/UnifiedPush vs FCM — belongs to uteke-mobile; tole exposes no
   push endpoint by design, see the threat model).
2. Render the pending Write (tool + description + session).
3. `POST /approvals/{id}/decision` — allow or deny.
4. `202` (allow) means the resume is running; `GET /sessions/{id}/status`
   until `busy` clears.

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
