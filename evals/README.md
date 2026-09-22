# tole evals — agentic regression harness (issue #73)

Unit tests verify **mechanisms**. This directory verifies **agentic
outcomes**: does a tole release still *behave* like the last one when
driven end-to-end? (Mandate: arXiv:2607.03691 — harness evolution itself
causes regressions practitioners misattribute to the model.)

## Tiers

### Tier 1 — deterministic harness behavior (`evals/tier1/`, wired into CI)

Runs on every PR via the `Evals` job. No network, no provider, fast.
These are *behavioral contracts of the harness itself*, exercised
through the real turn loop + storage (not unit-asserted internals):

- **Wire-shape stability** — the provider request body for a canonical
  transcript is byte-identical across releases. This is the
  cache-friendliness contract: any drift must be a deliberate,
  reviewed change (KV-cache prefix hits depend on it).
- **Resume equivalence** — replaying a crashed session produces the
  same durable outcome as the uninterrupted run (extends the E5
  golden-file property to the CURRENT tool set).
- **Abort-path contracts** — every abort (approval required, unknown
  tool, budget, loop guard, provider failure, transient retry) parks at
  a resumable state with a durable audit record.
- **Approval interaction matrix** — allow/deny × read/write/destructive
  × fresh/crashed: the verdicts that #68 hardened, as a table.

Run locally: `cargo test -p tole-core --test evals_tier1`

### Tier 2 — small live missions (`evals/tier2/`, nightly/manual)

Real provider (BYOK via `TOLE_*/OPENAI_*` env), cost-capped missions.
Each mission is a directory: `mission.txt` (the user prompt), optional
fixtures, and an expected-outcome spec. The runner scores:

- task success (model answer / produced artifacts match the spec)
- steps used, tokens in/out (from the durable usage ledger — #66)
- tool-error rate

Run locally: `python3 evals/tier2/run.py --all` (requires provider env
and the freshly built release binary — see CONTRIBUTING binary
hygiene).

### Tier 2.5 — offline trace replay (`evals/replay/`, issue #107)

The Dream-RSI replay-evaluation pattern (arXiv:2609.14858) applied to
this harness: with `run.py --archive`, each Tier-2 run keeps a
**redacted** copy of its session JSONL under `evals/traces/<model>/<mission>/`
(secret-shaped tokens and host paths scrubbed). A recorded trace is an
exact simulator of what the model saw, so a candidate prompt can be
**replayed** over the recorded tool outcomes at zero tool executions —
one completion call per (candidate, mission) is the whole cost.

```sh
python3 evals/tier2/run.py --all --archive     # record (real provider)
cargo build -p tole-cli --features replay --bin tole-replay
target/debug/tole-replay --traces evals/traces \
  --revised evals/replay/prompts/my-candidate.txt [--model <id>] [--json]
```

The scorer ALWAYS evaluates the incumbent (the shipped default prompt,
extracted from the CLI source — never a drifting copy) in the same run
and only reports `SHIP: revised` when the revision's average score over
the trace corpus is strictly better (monotone selection, Dream-RSI §3).
Scoring: a COMPLETED walk scores 0.7 × positional tool-path agreement +
0.3 × final-answer match; a walk that diverges or errors scores 0 —
dreaming is exact only inside the recorded world, so credit past the
divergence point would be fiction (`path_agreement` is still reported
for diagnosis).

Rules: replay is an **advisory pre-check** for prompt/policy revisions —
it cannot see real tool effects, so it does not replace Tier 2; live
missions remain the release gate. Traces are model-pinned (the
`--model` filter exists for a reason); do not compare scores across
models. `evals/traces/` is still reviewed like any committed artifact:
the archiver redacts, but grep before committing.

### Tier 3 — baselines (`evals/baselines/`)

`baselines/<tag>.json` stores Tier-2 results per release tag.
`evals/tier2/diff.py baseline.json results.json` emits PASS/REGRESS per
metric with thresholds (success must not drop; steps/tokens must not
grow >25% with the same pinned model). A PR that regresses is a HARNESS
regression by definition — discuss before merging, don't blame the
model.

## Rules

- Never run Tier 2 in CI — it costs money and needs secrets.
- Tier 1 must stay under ~5 s total.
- New harness feature → add a Tier-1 contract; new "smart" behavior →
  add a Tier-2 mission before trusting it.
