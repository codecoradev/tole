//! Scripted, deterministic provider for Tier A tests (E3).
//!
//! No network, no key, no timing — the entire turn loop can be exercised
//! in CI in milliseconds.

use crate::entry::Entry;
use crate::provider::{Provider, ProviderError, ProviderOutput};
use std::collections::VecDeque;

/// Replays a scripted list of outputs in order, or repeats one forever.
pub struct MockProvider {
    script: VecDeque<Result<ProviderOutput, ProviderError>>,
    repeat: Option<ProviderOutput>,
}

impl MockProvider {
    /// Replays `script` in order; errors once the script is exhausted.
    pub fn scripted(script: Vec<ProviderOutput>) -> Self {
        Self {
            script: script.into_iter().map(Ok).collect(),
            repeat: None,
        }
    }

    /// Script provider steps including failures (issue #58 retry tests):
    /// `Err(msg)` surfaces as `ProviderError(msg)` on that call, exactly
    /// like a real provider error (e.g. a gateway timeout).
    pub fn scripted_with_failures(steps: Vec<Result<ProviderOutput, String>>) -> Self {
        Self {
            script: steps
                .into_iter()
                .map(|r| r.map_err(ProviderError))
                .collect(),
            repeat: None,
        }
    }

    /// Returns the same output on every call (loop-guard tests).
    pub fn always(output: ProviderOutput) -> Self {
        Self {
            script: VecDeque::new(),
            repeat: Some(output),
        }
    }
}

impl Provider for MockProvider {
    fn complete(&mut self, _transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        if let Some(r) = &self.repeat {
            return Ok(r.clone());
        }
        self.script
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError("script exhausted".into())))
    }
}
