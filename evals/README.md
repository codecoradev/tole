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
