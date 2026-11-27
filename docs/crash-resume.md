# Crash and resume: what a SIGKILL leaves and what recovers it

The README says a session can be killed mid-turn and resumed where it
stopped. This page is what the SIGKILL loop tests (#1018) show that to mean,
per face and kill point, including the places where the answer is not "it
just works". Each row names the test that pins it. The tests start the real
`tole` binary against a scripted provider, kill it with SIGKILL at a random
point, and recover it.

Run them: `cargo test -p tole-cli --test serve_crash_resume --test
serve_crash_effect --test mission_crash_resume --test serve_crash_outbox`.
`TOLE_CRASH_LOOP_N` sets the rounds (2 to 4 by default; the nightly stress job
sets 25), `TOLE_CRASH_LOOP_SEED` replays the kill delays of a red run (each
test prints its seed). The helpers are in `crates/tole-cli/tests/crash_loop/`.

## What a kill leaves

- The session file parses: every complete line is valid JSON and `seq` only
  goes up. A line the kill cut short is never trusted; `open` discards it
  (`docs/architecture.md`, storage). A user message is in the log once or not
  at all, never twice.
- The register says where the turn was: `planning` (cut off in the model
  call), `tool_call`/`executing` (inside an effect), `final` (settled).
- A SIGKILL ends the process, not the kernel: every line the process wrote is
  in the page cache and survives it. The deferred settlement fsync of #483
  matters for a power cut or a kernel crash, which a process-level kill test
  cannot cause; it is not tested here.

## Recovery by face

| Cut off in | `tole resume <id>` | `tole serve` | `tole mission --resume <id>` |
|---|---|---|---|
| The model call (`planning`) | finishes the turn once, same session | `POST /sessions {"resume"}` settles the turn without a model call and answers `"interrupted": true`; the cut-off prompt is not answered, the next prompt is accepted (#1230) | **exit 1, `status: error`**, nothing written; run `tole resume` first |
| A tool effect (`executing`) | see "Replay" below | the same settle: the open call is failed, outcome UNKNOWN, never re-run | as above |
| An approval pending in serve's queue | the write never ran; nothing to resume | the queue is in memory and is gone; resume, and the model asks again: one new row | (a mission started over serve) the session is parked; `tole mission --resume` continues it directly |
| Settled | `nothing to resume` (exit 1) | a new prompt works | continues the mission |

Pinned by `serve_crash_resume.rs`, `serve_crash_effect.rs` and
`mission_crash_resume.rs`.

The mission row is the surprise: `mission.rs` says a crash mid-mission
"resumes exactly where it stopped", but `tole mission --resume` starts its
next turn with `run_turn`, which refuses a register that is not at rest
(`run_turn requires pc Idle or Final`). The refusal changes nothing in the
transcript. Recover in two steps: `tole resume <id>` finishes the cut-off
turn, then `tole mission --resume <id>` continues; the mission then completes
and its `fact/mission` register says so.

## Replay: where an effect runs again

A write is recorded as an intent with `replay: guarded` before it runs, and
settled after. A kill in between leaves the intent without a settlement:

- `tole resume <id>` **without an approval** refuses the replay: the intent
  settles as an error that says the outcome is UNKNOWN (the effect may have
  run), the effect does not run again, exit code 2, and the session takes the
  next prompt.
- `tole resume --allow <tool> <id>` (or `--yes`) **runs the effect a second
  time**, under the same intent id, and settles it once. A command has no
  idempotency key, so a `run_command` that appends to a file appends twice.
  This is the contract of a guarded intent: the replay goes through the
  approver again, and what the operator allows is what runs.
- A settled effect is never replayed.

A read-only tool carries an `idempotent` intent and is not gated again on replay (read from `turn/mod.rs` and `gate.rs`; the loops do not kill inside a read).

## Approvals

A parked approval is in memory. After a kill the restarted serve lists none
(an allow that was already granted went to the killed process and is not
carried over), the effect has not run, and nothing runs it on its own. When
the model asks again there is exactly one new row, and one allow runs the
effect once. Pinned in `serve_crash_effect.rs`.

## Outbound webhooks (#911)

`<cron dir>/outbox.jsonl` is append-only; an append is one `write` of the
whole line, so a SIGKILL leaves every line whole. Pinned in
`serve_crash_outbox.rs`:

- **At least once.** A kill between the send and the `done` line leaves a
  lease that runs out (the destination timeout plus 5 s); the restarted serve
  sends the event again with the same `X-Tole-Delivery` id and the same body.
  A receiver that must not act twice keys on that id. A kill after the `done`
  line sends nothing again.
- A fragment at the end of the file (a partial write at the file system; a
  SIGKILL cannot cause one) is never sent, and the next line starts on a line
  of its own.
- A hook run killed before it finished (the `fire` claim has no `done`) is
  not run again (`cron/tick.rs`: "never a re-run"): the same delivery id sent
  again is answered `duplicate`, another id `409 already running`, until the
  next cron tick settles the claim as `interrupted`. Nothing is queued in the
  outbox meanwhile. A sender that retries an accepted hook after a restart
  should therefore expect a lost run, not a second one.
