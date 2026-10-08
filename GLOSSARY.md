# Glossary

Short definitions of the vocabulary used in the tool-call gate work (#303).
Extended lazily; add terms only when code or docs start using them.

- **Gate** — `crates/tole-core/src/gate.rs`. The single crate-internal module that decides whether a tool call may run (risk, then approver, then opt-in pre-hooks). It decides authorization only; it records and executes nothing.
- **PDP (policy decision point)** — the part that answers "may this call run?". Here: `gate::authorize`.
- **PEP (policy enforcement point)** — the part that acts on the answer. Here: the turn loop in `turn.rs` (`drive`, `resume_turn`), which maps a denial to durable records and a `TurnOutcome` and owns the effect sandwich, cancel checkpoints, observer and post-hooks.
- **Permit** — a crate-internal proof value issued by the gate for a non-ReadOnly call, with a private constructor. The crate-internal execute funnel (`Authorized::execute`) is the only way the turn loop runs a tool, so executing a Write/Destructive tool without the gate is a compile error inside the crate. ReadOnly tools need no permit.
- **Mode** — how a call reaches the gate: `Fresh` (a provider tool call) or `Replay { recorded }` (a crash-replayed intent with its recorded `ReplaySafety`).
- **Denied** — the gate's typed refusal: approver denial (or no approver), pre-hook denial, or unknown tool.
- **Effect sandwich** — durable `begin` (intent) → execute → `settle` around every tool effect, so a crash is recoverable (see docs/architecture.md §6).
- **Complete mediation** — the property that every non-ReadOnly execution passes the check; the reason the gate and `Permit` exist.
