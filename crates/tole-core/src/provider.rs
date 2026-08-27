//! LLM provider abstraction (E3).
//!
//! Phase 1: a thin, synchronous trait. Streaming is out of scope; the mock
//! implementation is scripted and deterministic so the whole turn loop is
//! testable without a network or API key (Tier A). The real HTTP client
//! (E4) implements the same trait — the turn loop cannot tell them apart.

use crate::entry::Entry;

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
}
