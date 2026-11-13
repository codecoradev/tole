//! `consult_advisor` (#587 part 2): the advisor tier on the host side.
//!
//! - **Where.** Registered only in runs whose token budget the advisor can
//!   share: a `tole mission` (and so a delegated child, which is a mission)
//!   and a cron agent run, when the run's profile declares
//!   `models.advisor`. `run`, `chat`, the TUI, `serve` and ACP have no
//!   token budget for the advisor to come out of, so the tool is not
//!   registered there. It is ReadOnly, so plan-mode runs keep it.
//! - **What it sees.** [`Tap`] wraps the run's provider and keeps the tail
//!   of the transcript the turn loop hands it, in memory. The advisor's
//!   window comes from there, never from a file the model or a child can
//!   write. [`tole_core::advisor::request`] redacts, neutralizes and caps
//!   it.
//! - **What it costs.** The run's [`Budget`] is shared: the run's meter
//!   charges its turns and the advisor charges its call. A consultation the
//!   budget cannot fit is refused before any call. Each consultation's
//!   usage is queued with `"role": "advisor"` and written to the session as
//!   a usage record by [`record_usage`] after the turn.
//! - **What it returns.** Only [`tole_core::advisor::parse_reply`]'s
//!   verdict, fenced as advisory data. The advisor call carries no tools
//!   and does not stream.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tole_core::advisor::{self, Budget, Uses};
use tole_core::entry::Entry;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::secrets::SecretSet;
use tole_core::tool::{Risk, Tool};

/// Tokens reserved for the advisor's reply when admitting a call.
pub const REPLY_TOKENS: u64 = 600;

/// What a run and its `consult_advisor` share.
#[derive(Debug)]
pub struct Shared {
    pub budget: Arc<Budget>,
    window: Mutex<Vec<Entry>>,
    pending: Mutex<Vec<Value>>,
}

impl Shared {
    pub fn new(budget: Arc<Budget>) -> Arc<Shared> {
        Arc::new(Shared {
            budget,
            window: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Keeps the last [`advisor::WINDOW_ENTRIES`] entries the turn loop just
    /// handed the run's provider (an in-memory copy, never a file read).
    fn snapshot(&self, transcript: &[Entry]) {
        let start = transcript.len().saturating_sub(advisor::WINDOW_ENTRIES);
        *self.window.lock().unwrap_or_else(|p| p.into_inner()) = transcript[start..].to_vec();
    }

    fn window(&self) -> Vec<Entry> {
        self.window
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn queue(&self, usage: Value) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(usage);
    }

    /// Takes the advisor usages not yet written to the session.
    pub fn drain(&self) -> Vec<Value> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// Prompt + completion tokens of a provider usage object.
pub fn tokens(u: &Value) -> u64 {
    let at = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    at("prompt_tokens").saturating_add(at("completion_tokens"))
}

/// The run's provider, watched: keeps the transcript tail for the advisor
/// and, when `charge`, charges each step's usage to the shared budget (a
/// face whose own meter already charges it passes `false`).
pub struct Tap {
    inner: Box<dyn Provider>,
    shared: Arc<Shared>,
    charge: bool,
}

impl Tap {
    pub fn new(inner: Box<dyn Provider>, shared: Arc<Shared>, charge: bool) -> Tap {
        Tap {
            inner,
            shared,
            charge,
        }
    }

    fn after(&self) {
        if !self.charge {
            return;
        }
        if let Some(u) = self.inner.last_usage() {
            self.shared.budget.charge(tokens(&u));
        }
    }
}

impl Provider for Tap {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.shared.snapshot(transcript);
        let out = self.inner.complete(transcript);
        self.after();
        out
    }
    fn complete_with_cancel(
        &mut self,
        transcript: &[Entry],
        cancel: &tole_core::cancel::CancelToken,
    ) -> Result<ProviderOutput, ProviderError> {
        self.shared.snapshot(transcript);
        let out = self.inner.complete_with_cancel(transcript, cancel);
        self.after();
        out
    }
    fn last_usage(&self) -> Option<Value> {
        self.inner.last_usage()
    }
    fn last_reasoning(&self) -> Option<String> {
        self.inner.last_reasoning()
    }
    fn last_extra_tool_calls(&self) -> Vec<(String, Value)> {
        self.inner.last_extra_tool_calls()
    }
    fn last_wire_stats(&self) -> Option<Value> {
        self.inner.last_wire_stats()
    }
    fn last_degraded(&self) -> Option<Value> {
        self.inner.last_degraded()
    }
    fn last_route(&self) -> Option<Value> {
        self.inner.last_route()
    }
}

/// Builds the provider of one advisor call: no tools, no streaming, the
/// fixed advisor instructions.
pub type Factory = Box<dyn Fn() -> Box<dyn Provider> + Send + Sync>;

/// The production factory: the advisor model on the run's provider
/// connection, with no fallback chain (a fallback would be another model).
pub fn factory(
    mut cfg: tole_core::openai::OpenAiConfig,
    model: &str,
    secrets: SecretSet,
) -> Factory {
    cfg.model = model.to_string();
    Box::new(move || {
        Box::new(
            tole_core::openai::OpenAiProvider::new(cfg.clone())
                .with_secrets(&secrets)
                .with_streaming(false)
                .with_system_prompt(advisor::SYSTEM_PROMPT),
        )
    })
}

/// The `consult_advisor` tool.
pub struct ConsultAdvisor {
    shared: Arc<Shared>,
    uses: Uses,
    max_uses: u32,
    contract: String,
    secrets: SecretSet,
    make: Factory,
}

impl ConsultAdvisor {
    pub fn new(
        shared: Arc<Shared>,
        max_uses: u32,
        contract: String,
        secrets: SecretSet,
        make: Factory,
    ) -> ConsultAdvisor {
        ConsultAdvisor {
            shared,
            uses: Uses::new(max_uses),
            max_uses,
            contract,
            secrets,
            make,
        }
    }
}

impl Tool for ConsultAdvisor {
    fn name(&self) -> &str {
        "consult_advisor"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        format!(
            "Ask a stronger advisor model about a hard decision. It sees the task and the recent \
             transcript, never acts, and answers with a plan, a correction or a stop signal. At \
             most {} times per run; its tokens come out of the run's budget.",
            self.max_uses
        )
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {"question": {"type": "string",
                "description": "The decision you need advice on, in one or two sentences."}},
            "required": ["question"],
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let question = input
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or("consult_advisor needs a non-empty `question`")?;
        let window = self.shared.window();
        let user = advisor::request(&self.contract, &window, question, &self.secrets)
            .into_iter()
            .find(|(role, _)| *role == "user")
            .map(|(_, content)| content)
            .unwrap_or_default();
        // Refused before any call: a budget that cannot fit it, or the cap.
        let need = ((advisor::SYSTEM_PROMPT.len() + user.len()) / 4) as u64 + REPLY_TOKENS;
        self.shared.budget.admit(need)?;
        let n = self.uses.take()?;
        // One user message, no tools, no streaming (see `factory`).
        let sent = Entry {
            seq: 1,
            id: "e_1".into(),
            parent_id: None,
            kind: tole_core::entry::EntryType::new("message"),
            timestamp: 0,
            payload: json!({"role": "user", "text": user}),
        };
        let mut provider = (self.make)();
        let out = provider.complete(std::slice::from_ref(&sent));
        // What was spent is counted, whatever the outcome.
        let spent = provider.last_usage();
        let used = spent.as_ref().map(tokens).unwrap_or(0);
        if let Some(u) = &spent {
            self.shared.budget.charge(used);
            self.shared.queue(advisor::usage_value(u));
        }
        match out {
            Ok(ProviderOutput::Final { text }) => {
                let v = advisor::parse_reply(&text, &self.secrets);
                Ok(json!({
                    "advisor": v.fenced(),
                    "verdict": v.kind.as_str(),
                    "use": n,
                    "of": self.max_uses,
                    "tokens": used,
                }))
            }
            Ok(_) => Err(
                "the advisor answered with a tool call, which it may not make; nothing was run"
                    .into(),
            ),
            Err(e) => Err(format!(
                "the advisor could not answer: {}",
                advisor::clean(&e.0, &self.secrets, 300)
            )),
        }
    }
}

/// Writes the queued advisor usages to the session as usage records, so
/// the run's token count (and a resumed run's) includes them.
pub fn record_usage(
    shared: &Shared,
    storage: &mut dyn tole_core::storage::Storage,
) -> Result<(), String> {
    for usage in shared.drain() {
        let entry_id = storage
            .entries()
            .last()
            .map(|e| e.id.clone())
            .unwrap_or_default();
        storage
            .commit(
                tole_core::storage::Commit::new().usage(tole_core::storage::UsageRecord {
                    id: String::new(),
                    entry_id,
                    usage,
                    cost_usd: None,
                }),
            )
            .map_err(|e| format!("recording the advisor's usage: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "advisor_host_tests.rs"]
mod tests;
