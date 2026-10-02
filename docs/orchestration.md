# Orchestrating Tole Workers Over `tole serve`

Field guide: running N tole sessions as coordinated workers using only shipped
surfaces — no orchestrator inside tole itself. Tole's core stays single-lane by
design (v0 identity); orchestration is a **caller-side** composition problem,
and every pattern below composes four existing features:

- `tole serve --transport mcp` — one HTTP endpoint, N multi-session MCP
  workers (#137/#138), hardened (connection cap, auth rate-limit, IO timeouts, #136)
- `resume <id> "<prompt>"` — continue a durable session with new instructions (#65)
- `job_start` / `job_poll` — long CPU work that outlives a tool-call ceiling (#64)
- per-session `--allow` / `--on-pretool` / `--on-posttool` / `--workspace` —
  each worker carries its own policy and file-tools root

## When NOT to (read this first)

Start from the lowest complexity that meets the need (Azure Architecture
Center guidance): a single tole session with tools solves most work. Every
level you add buys coordination cost, latency, and token spend. Anthropic's
published multi-agent numbers are the honest price tag: a lead+subagent system
beat a single agent by **90.2%** on research-class evals — but consumed
**~15× the tokens**, and token usage alone explained 80% of the performance
variance. Translation: multi-worker topologies are for **high-value tasks**,
not defaults. If a single session with `job_start` can do it, do that.

## P1 — Fan-out / fan-in (parallel subtasks)

**Shape:** one orchestrating process (Hermes, a script, your editor) opens N
sessions on the serve endpoint, gives each an independent subtask, waits, then
merges the results itself.

```
caller ──┬─ session A ("audit crates/tole-core file_tools.rs for path escapes")
         ├─ session B ("audit storage.rs commit path for torn-write windows")
         └─ session C ("audit subprocess.rs for stdin/stdout deadlock patterns")
caller: merge three reports into one findings table
```

**Use for:** multi-module review sweeps, parallel migrations, benchmark runs.
**Not for:** anything one session can do sequentially in reasonable time.

Each session is its own **approval domain**: give worker A
`--allow 'write_file'` and workers B/C read-only tools only (`--plan-mode`
exposes ReadOnly tools and nothing else). Blast-radius containment comes free
with the topology — a compromised or confused worker cannot reach another
worker's permissions.

## P2 — Pipeline (sequential context passing)

**Shape:** session A's final answer becomes session B's prompt (via
`resume <id> "<prompt>"` or by the caller pasting A's output into B's
mission). No shared state, no shared failure domain.

```
session A (plan):  "read the issue and produce a step-by-step change plan; do not edit"
   ↓ (plan text as prompt)
session B (execute): "apply this plan: <plan> — you may write; gates apply"
   ↓ (diff summary as prompt)
session C (verify):  "review this diff against the plan; report discrepancies only"
```

**Use for:** plan→execute→verify chains, draft→harden→audit flows.
**Why it works:** each stage gets a fresh context window focused on exactly
one job — the pattern that makes multi-agent systems effective is context
isolation, and P2 buys it without parallel cost.

## P3 — Review fan-in (N producers + 1 verifier + human merge)

**Shape:** N producer sessions attempt the same task (or different tasks);
one verifier session checks each result against the requirement; a **human**
makes the merge decision. This is the verification-first doctrine made
structural: producers race, the verifier filters, the human owns the gate.

```
producer 1 ─┐
producer 2 ─┼─→ verifier session ("which of these N diffs satisfies the spec;
producer 3 ─┘    score each; reject any with unexplained behavior changes")
                     ↓ ranked report
                  HUMAN merges
```

**Use for:** high-stakes changes where a second opinion is cheap relative to
the incident it prevents. **Rule:** the verifier must not share context with
the producers — a fresh session sees the artifacts, not the rationalizations.

## Operational notes

- **Session hygiene:** every worker session is a durable JSONL log; list them
  via the MCP surface (`tole_session_list`) and close what you finish.
- **Long work:** anything beyond a tool-call ceiling belongs in
  `job_start`/`job_poll` inside the worker session — not in a bigger timeout.
- **Policy per worker:** `--allow` pre-authorizes Writes per worker;
  `--plan-mode` removes mutation tools from the wire entirely;
  `--on-pretool` scripts enforce org-specific policy without recompiling.
- **Cost discipline:** fan-out multiplies token spend roughly by N. Budget
  per task, not per worker; promote a topology to default only with measured
  numbers (see `evals/` for how to measure).
