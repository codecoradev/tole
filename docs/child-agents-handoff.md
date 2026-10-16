# Child-agent handoff and verify gate (#383)

Status: implemented — the schema, validation and close gate
(`tole_core::handoff`), the child-side `handoff_submit` tool, the `role` /
`consumes` inputs of `agent_start` and the gate in `agent_poll`.

## Problem

Child agents (#171) are durable, depth-1 `tole run` sessions. Today the only
way a child's result reaches another agent is through the parent: a settle
summary in a per-child mailbox, relayed in the parent's own words. That
loses the artifact (what was built, where, at which commit) and lets a child
"finish" without anyone checking its work.

## Handoff document

When a child closes it leaves one validated JSON document in its agent dir:

```json
{
  "version": 1,
  "role": "implement",
  "summary": "built the parser",
  "artifacts": [
    { "path": "src/parser.rs", "rev": "abc1234", "note": "the new parser" }
  ],
  "verify": { "command": "cargo test", "exit_code": 0, "summary": "12 passed" }
}
```

- **Artifacts are references**, not content: a workspace-relative `path`
  and, optionally, the `rev` (commit, tag or branch) it was built in.
  Downstream children read them with their own file tools; the parent is not
  the data channel.
- **Validation is strict** and treats the document as untrusted
  model-authored input: unknown fields are errors; every field is bounded;
  a path must be workspace-relative with no `..`, no backslash, no colon
  (a Windows drive prefix would replace the workspace root when joined), no
  control characters and no leading `-`; a `rev` must be a plain commit or ref name
  (alphanumeric start, safe charset, no `..`), so neither can be parsed as a
  command-line option or escape the workspace.
- Text rendered for another model (`consume_preamble`, `fix_prompt`) escapes
  control characters, so a field cannot break out of its line.

## Roles

A vocabulary, not a scheduler. Each role consumes and produces the same
document.

| Role | Does | Verification required to close |
|------|------|--------------------------------|
| `explore` | finds out: reads, searches, reports | no |
| `implement` | builds something | **yes** |
| `verify` | checks someone else's work | no (its verdict is its summary) |
| `fix` | repairs after a failed verification | **yes** |

## Verify gate

`Handoff::gate()` decides whether the work may be treated as closed:

- `Closed` — not an implement/fix child, or verification ran and exited 0.
- `Unverified` — an implement/fix child reported no verification.
- `VerifyFailed { exit_code }` — verification ran and failed.

Neither failure is closed silently. `Handoff::fix_prompt()` builds the prompt
of the `fix` child that should follow: what failed, the evidence the child
reported, and the artifact references to read.

## How it is wired

- **Start**: `agent_start` takes an optional `role` and an optional
  `consumes` list of artifact references. The child's mission is the
  requested prompt, preceded by a short role brief and followed by the
  artifacts to read (rendered with control characters escaped). With neither
  input the child's prompt is exactly what was asked for.
- **Submit**: a role child gets the `handoff_submit` tool (the parent sets
  `TOLE_AGENT_ROLE` and `TOLE_AGENT_HANDOFF`). The tool validates the input
  and writes `<agent dir>/handoff.json`; the role comes from the parent, not
  from the model. It is Write-tier and the parent adds exactly this name to
  the child's allowlist: it can only write that one validated, bounded file,
  so it does not widen what the child may do.
- **Poll**: for a settled role child `agent_poll` adds `role`, `gate`
  (`closed`, `unverified`, `verify_failed`, or `no_handoff` for roles that
  are not gated), the `handoff` itself, and — whenever the work is not
  closed — a `fix_prompt`. A gated child that submitted nothing, or an
  invalid document, is `unverified`.
- **Fix**: nothing is spawned automatically. The parent model sees the
  failed gate and the ready-made `fix_prompt` and decides whether to start a
  `fix` child (an `agent_start` with `role: fix`), so spawning, approvals and
  budgets stay under the parent's policy.

## Anti-goals

- **No DAG scheduler or dependency engine.** The dependency edge is the data
  channel (an artifact reference), nothing more. The parent model still
  decides what to start and when; nothing is spawned automatically.
- No new authority: spawning, approvals and the child allowlist are
  unchanged (`agent_start` stays Write-class and bounded by the parent's
  approved patterns).

## Limits

The verification evidence is self-reported by the child. The gate guarantees
that a close without evidence, or with failing evidence, is surfaced; it does
not prove the command was run. For ground truth, run the check from outside
the child (as `tole mission --verify` does).
