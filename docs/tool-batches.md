# Sequential tool-call batches (#480)

**Owner decision (3 Nov 2026, after reading #480):** the non-goal "no parallel
tool calls" is revisited and the **sequential batch** option is approved. This
is *not* concurrent execution: the turn loop stays single-threaded.

## Scope of the first version

A response with several tool calls runs in full **only when every call is a
registered `ReadOnly` tool** (the first call and each extra). If ANY call is
non-ReadOnly or unknown, the loop falls back to the previous behaviour for
that response: first call only, the model re-asks for the rest. This keeps the
approval UX intact; extending batches to Write tools is a separate decision.

## Where it lives

- `ProviderOutput` is unchanged (still one `ToolCall`). The calls after the
  first travel through a new defaulted trait method,
  `Provider::last_extra_tool_calls`, which `OpenAiProvider` fills (stream and
  non-stream) and `Box<P>` / `FailoverProvider` forward. Mocks and embedders
  that do not override it see no change.
- The turn loop (`turn.rs`) iterates the batch; the transcript flattening
  (`openai.rs`) merges it back into one assistant message.

## Design points

- **Step budget.** One model response = one step, whatever the call count
  (`MAX_STEPS` counts provider steps). A poll-tool refund applies once, to the
  first call; each poll call draws from the poll budget.
- **Loop guard.** Every call is fingerprinted in order and feeds the same
  consecutive-identical streak, so three identical calls in one batch trip the
  guard exactly as three identical single calls would.
- **Failure / denial mid-batch.** Each call goes through `gate::authorize`, the
  loop guard and its own sandwich. The first *execution failure* settles that
  call as an error and writes every remaining call as an intent settled with
  `not executed: an earlier call in this batch failed`, so each `tool_call` has
  a result (API rule), and the turn continues. Gate denials, loop trips,
  cancel and unknown-tool aborts keep their existing terminal behaviour; the
  calls not yet started are simply not recorded, so history stays valid. In an
  all-ReadOnly batch the approver is never consulted, so a gate denial is
  unreachable in practice. An URGENT interrupt (#377) stops a batch before the
  next call starts.
- **Crash-resume mid-batch.** Each call is its own sandwich with its own
  `ReplaySafety` (`Idempotent` for ReadOnly). A crash leaves at most one pending
  intent; resume replays it as before. The not-yet-started calls lived only in
  memory, so after resume the model re-asks for them — correct, just not
  free. No batch bookkeeping is persisted beyond the intent marker below.
- **Observer events.** ACP/serve still see strictly alternating
  `tool_started` / `tool_finished` pairs, one per executed call, in model
  order; `reasoning` fires once per step, before the first call. Stub results
  fire no events. No observer change is needed.
- **Cache prefix.** History stays append-only. The 2nd..Nth intents carry
  `"batch": true`; the wire flattening joins a marked intent into the assistant
  message of the call before it, with tool results following in call order.
  Intents without the marker (every pre-existing session, and one-call
  responses) flatten byte-identically to before.
