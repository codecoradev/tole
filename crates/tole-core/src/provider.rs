//! LLM provider abstraction (E3).
//!
//! Phase 1: a thin, synchronous trait. Streaming is out of scope; the mock
//! implementation is scripted and deterministic so the whole turn loop is
//! testable without a network or API key (Tier A). The real HTTP client
//! (E4) implements the same trait — the turn loop cannot tell them apart.

use crate::cancel::CancelToken;
use crate::entry::Entry;
use serde_json::Value;

/// One completion step: the model either finishes the turn or asks for a
/// tool. There is no mixed text+tool response — the loop forces a single
/// next step, which keeps the state machine single-path.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderOutput {
    /// The model produced a final answer; the turn ends.
    Final {
        /// Assistant text to return to the host.
        text: String,
    },
    /// The model wants a tool executed before continuing.
    ToolCall {
        /// Registry name of the tool.
        tool: String,
        /// Tool input as declared by the tool's spec.
        input: serde_json::Value,
    },
    /// The model requested a tool but its `arguments` were not valid JSON.
    /// The tool is **not** executed; the model receives the parse error as
    /// the settlement and may retry with well-formed arguments.
    InvalidToolArgs {
        /// Registry name of the tool the model tried to call.
        tool: String,
        /// The raw `arguments` string exactly as received from the provider.
        raw: String,
        /// Human-readable serde error.
        reason: String,
    },
}

/// Failures from a provider call (transport, auth, malformed reply...).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("provider error: {0}")]
pub struct ProviderError(pub String);

/// LLM call abstraction over the replayed transcript.
pub trait Provider {
    /// Produce the next step given the full materialized transcript
    /// (entries in commit order — user, assistant, intent, results...).
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError>;

    /// [`complete`](Provider::complete) that may return early once `cancel`
    /// is set (#484). The turn loop calls this variant so a cancel (Esc,
    /// `--timeout`, ACP `session/cancel`) interrupts a call already in
    /// flight; on cancel it returns an error and the loop settles the turn
    /// as cancelled, committing nothing for the abandoned call. The default
    /// ignores the token (mocks and embedders keep working unchanged). An
    /// abandoned request may still be billed by the provider.
    fn complete_with_cancel(
        &mut self,
        transcript: &[Entry],
        _cancel: &CancelToken,
    ) -> Result<ProviderOutput, ProviderError> {
        self.complete(transcript)
    }

    /// Provider-reported usage for the LAST completed `complete()` call,
    /// if the provider exposes it (OpenAI-compatible: the response body's
    /// `usage` object). Consumed by the turn loop to append a durable
    /// UsageRecord; default None so scripted/mocks stay free to ignore it.
    fn last_usage(&self) -> Option<Value> {
        None
    }

    /// The model's reasoning/thought for the LAST completed `complete()`
    /// call, when the provider exposes one (OpenAI-compatible GLM-class
    /// responses carry a `reasoning` field next to `content`). Consumed
    /// by the turn loop's observer (issue #196) for host-side thought
    /// surfacing — never persisted to the durable log (the ledger keeps
    /// the answer, not the deliberation). Default None: mocks and
    /// providers without a notion of thought stay silent.
    fn last_reasoning(&self) -> Option<String> {
        None
    }

    /// The tool calls that FOLLOWED the one `complete()` returned, in model
    /// order, when the model emitted several in one response (#480). Only
    /// calls whose arguments parsed are listed; the first malformed one ends
    /// the list. The turn loop runs them sequentially after the returned
    /// call, and only when the whole batch is ReadOnly — otherwise it
    /// ignores them (the model re-asks, as before). Like `last_usage` it
    /// describes THIS call only. Default empty: mocks and one-call
    /// providers are unchanged, and `ProviderOutput` keeps its single call.
    fn last_extra_tool_calls(&self) -> Vec<(String, Value)> {
        Vec::new()
    }

    /// Request-size breakdown for the LAST `complete()` call (issue
    /// #211): `{system_chars, tools_chars, history_chars, messages}`,
    /// measured on the request body before any streaming envelope (see
    /// `openai::wire_stats`). Like `last_usage`, it describes THIS call
    /// only — providers reset it per call. The turn loop stores it under
    /// the namespaced `tole_wire` key of the usage record. Default None:
    /// mocks and providers without a wire notion add nothing.
    fn last_wire_stats(&self) -> Option<Value> {
        None
    }

    /// Set when the LAST `complete()` call only succeeded (or failed)
    /// after the provider dropped a parameter it rejected (#457:
    /// `reasoning_effort`): `{param, level, rejected}`. Stored by the turn
    /// loop under the namespaced `tole_degraded` key of the step's usage
    /// record, so a degraded step is durable. Describes THIS call only.
    /// Default None.
    fn last_degraded(&self) -> Option<Value> {
        None
    }

    /// Which provider answered the LAST `complete()` call, and whether it
    /// took a failover to get there (#209, `failover::FailoverProvider`).
    /// Stored by the turn loop under the namespaced `tole_route` key of the
    /// step's usage record. Default None: a single provider reports no route.
    fn last_route(&self) -> Option<Value> {
        None
    }
}

/// A boxed provider is a provider (hosts pick the concrete type at runtime,
/// e.g. a plain provider or a `failover::FailoverProvider`). Every method is
/// forwarded, including the defaulted ones.
impl<P: Provider + ?Sized> Provider for Box<P> {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        (**self).complete(transcript)
    }
    fn complete_with_cancel(
        &mut self,
        transcript: &[Entry],
        cancel: &CancelToken,
    ) -> Result<ProviderOutput, ProviderError> {
        (**self).complete_with_cancel(transcript, cancel)
    }
    fn last_usage(&self) -> Option<Value> {
        (**self).last_usage()
    }
    fn last_reasoning(&self) -> Option<String> {
        (**self).last_reasoning()
    }
    fn last_wire_stats(&self) -> Option<Value> {
        (**self).last_wire_stats()
    }
    fn last_extra_tool_calls(&self) -> Vec<(String, Value)> {
        (**self).last_extra_tool_calls()
    }
    fn last_route(&self) -> Option<Value> {
        (**self).last_route()
    }
    fn last_degraded(&self) -> Option<Value> {
        (**self).last_degraded()
    }
}
