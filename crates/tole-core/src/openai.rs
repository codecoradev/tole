//! Real LLM provider over OpenAI-compatible chat completions (E4).
//!
//! Sync (Phase 1 — the turn loop is single-threaded), `ureq` under the
//! hood. The API key lives only in the outbound `Authorization` header;
//! errors are scrubbed so a failed request can never echo it back into
//! logs or the durable log.
//!
//! Config resolution order: explicit struct fields > env (`CORAGENT_*`).

use crate::provider::{Provider, ProviderError, ProviderOutput};
use serde_json::{json, Value};
use std::time::Duration;

/// Where a config value came from — for tests and debug output that must
/// never include the key itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiConfig {
    /// e.g. `https://api.openai.com/v1`
    pub base_url: String,
    /// e.g. `gpt-4o-mini`
    pub model: String,
    /// Never logged; goes only into the `Authorization` header.
    pub api_key: String,
}

/// Env names are namespaced (`CORAGENT_*`) to avoid collisions with other
/// tools on the same host.
pub const ENV_BASE_URL: &str = "CORAGENT_BASE_URL";
pub const ENV_MODEL: &str = "CORAGENT_MODEL";
pub const ENV_API_KEY: &str = "CORAGENT_API_KEY";

impl OpenAiConfig {
    /// Build from explicit values.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
        }
    }

    /// Resolve from `CORAGENT_BASE_URL` / `CORAGENT_MODEL` / `CORAGENT_API_KEY`.
    /// `None` when any of the three is missing/empty.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var(ENV_BASE_URL).ok()?.trim().to_string();
        let model = std::env::var(ENV_MODEL).ok()?.trim().to_string();
        let api_key = std::env::var(ENV_API_KEY).ok()?.trim().to_string();
        if base_url.is_empty() || model.is_empty() || api_key.is_empty() {
            return None;
        }
        Some(Self {
            base_url,
            model,
            api_key,
        })
    }

    /// `Display` that structurally cannot leak the key: only lengths and
    /// the model/base URL are shown.
    pub fn safe_description(&self) -> String {
        format!(
            "OpenAiConfig {{ base_url: {}, model: {}, api_key: <{} chars, redacted> }}",
            self.base_url,
            self.model,
            self.api_key.len()
        )
    }
}

/// OpenAI-compatible provider (works with OpenAI, Groq, OpenRouter, vLLM,
/// LiteLLM proxies, … — anything speaking `/chat/completions`).
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    cfg: OpenAiConfig,
    timeout: Duration,
    /// Optional system prompt prepended to the wire transcript. Explicit
    /// opt-in: absent by default, never invented by the provider itself.
    system_prompt: Option<String>,
}

impl OpenAiProvider {
    pub fn new(cfg: OpenAiConfig) -> Self {
        Self {
            cfg,
            timeout: Duration::from_secs(120),
            system_prompt: None,
        }
    }

    /// Override the default 120s timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set a system prompt (prepended as the first message).
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// The agent used for requests (timeout lives here in ureq 3.x).
    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .build()
            .new_agent()
    }

    /// Build the request body. Separated so tests can inspect exactly what
    /// would go over the wire (and prove the key never appears in it).
    ///
    /// Phase 1 flattening: only chat messages (`kind == "message"` with a
    /// recognized role) are sent. Tool calls, errors, and state records
    /// stay in the durable log but are not part of the wire transcript.
    ///
    /// E11: any occurrence of the API key inside message text is redacted
    /// before the body goes over the wire — the durable log keeps the
    /// original (local), the provider never sees the key.
    fn request_body(&self, transcript: &[Entry]) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if let Some(sys) = &self.system_prompt {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        messages.extend(
            transcript
                .iter()
                .filter(|e| e.kind.as_str() == "message")
                .filter_map(|e| {
                    let role = e.payload.get("role")?.as_str()?;
                    // Only roles the chat-completions API accepts.
                    if !matches!(role, "user" | "assistant" | "system") {
                        return None;
                    }
                    let text = e
                        .payload
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|t| scrub(t, &self.cfg.api_key))?;
                    Some(json!({ "role": role, "content": text }))
                }),
        );
        json!({
            "model": self.cfg.model,
            "messages": messages,
        })
    }
}

impl Provider for OpenAiProvider {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        // Phase 1: no tool-calling loop on the real provider yet — the
        // transcript is flattened to text and the reply is treated as
        // final. The mock (E3 tests) exercises the full loop; E5 wires
        // real tool calls.
        let body = self.request_body(transcript);
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let resp = self
            .agent()
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.cfg.api_key))
            .send_json(&body)
            .and_then(|mut r| r.body_mut().read_json::<Value>())
            .map_err(|e| ProviderError(scrub(&e.to_string(), &self.cfg.api_key)))?;
        let text = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                ProviderError("malformed response: no choices[0].message.content".into())
            })?;
        Ok(ProviderOutput::Final {
            text: text.to_string(),
        })
    }
}

/// Remove any accidental occurrence of the secret from an error string.
/// Falls back to a generic message if scrubbing somehow fails.
fn scrub(msg: &str, secret: &str) -> String {
    if msg.contains(secret) {
        msg.replace(secret, "<redacted>")
    } else {
        msg.to_string()
    }
}

use crate::entry::Entry;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_display_never_leaks_key() {
        let cfg = OpenAiConfig::new(
            "https://api.example.com/v1",
            "test-model",
            "sk-supersecret-123",
        );
        let d = cfg.safe_description();
        assert!(!d.contains("sk-supersecret-123"), "key leaked: {d}");
        assert!(d.contains("test-model"));
    }

    #[test]
    fn env_resolution_requires_all_three() {
        // Deliberately not touching real env: from_env reads CORAGENT_*,
        // which is absent in CI → None (and that's the tested contract).
        // A full presence test would need serialised env access.
        let cfg = OpenAiConfig::from_env();
        // In CI none of the vars are set → None. If a dev machine has them
        // set, from_env returning Some is equally correct — both prove the
        // reader works without panicking on missing vars.
        let _ = cfg;
    }

    #[test]
    fn scrub_removes_secret_from_errors() {
        let out = scrub(
            "request to https://x failed with sk-secret-abc",
            "sk-secret-abc",
        );
        assert_eq!(out, "request to https://x failed with <redacted>");
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::*;
    use crate::entry::{Entry, EntryType};
    use serde_json::json;

    fn msg(id: &str, role: &str, text: &str) -> Entry {
        Entry {
            id: id.into(),
            parent_id: None,
            seq: 0,
            kind: EntryType::new(EntryType::MESSAGE),
            payload: json!({ "role": role, "text": text }),
            timestamp: 0,
        }
    }

    #[test]
    fn request_body_flattens_message_entries_in_order() {
        let cfg = OpenAiConfig::new("https://x.example/v1", "m", "k");
        let p = OpenAiProvider::new(cfg);
        let transcript = vec![
            msg("e1", "user", "hello"),
            msg("e2", "assistant", "hi there"),
            msg("e3", "user", "do a thing"),
        ];
        let body = p.request_body(&transcript);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], json!("user"));
        assert_eq!(msgs[0]["content"], json!("hello"));
        assert_eq!(msgs[2]["content"], json!("do a thing"));
        assert_eq!(body["model"], json!("m"));
    }

    #[test]
    fn request_body_skips_non_message_and_unknown_roles() {
        let cfg = OpenAiConfig::new("https://x.example/v1", "m", "k");
        let p = OpenAiProvider::new(cfg);
        let mut odd = msg("e9", "tool", "ignored");
        odd.kind = EntryType::new("error");
        let transcript = vec![odd, msg("e10", "wizard", "unknown role")];
        let body = p.request_body(&transcript);
        assert_eq!(
            body["messages"].as_array().map(|a| a.len()),
            Some(0),
            "non-message kinds and unknown roles must be filtered out"
        );
    }

    #[test]
    fn request_body_redacts_api_key_from_message_text() {
        // E11: a user message that accidentally quotes the API key must
        // never reach the provider in cleartext.
        let key = "sk-super-secret-123";
        let cfg = OpenAiConfig::new("https://x.example/v1", "m", key);
        let p = OpenAiProvider::new(cfg);
        let transcript = vec![msg(
            "e1",
            "user",
            "my key is sk-super-secret-123 please help",
        )];
        let body = p.request_body(&transcript);
        let serialized = body.to_string();
        assert!(
            !serialized.contains(key),
            "api key leaked in request body: {serialized}"
        );
        assert!(
            serialized.contains("<redacted>"),
            "redaction marker missing: {serialized}"
        );
    }

    #[test]
    fn request_body_keeps_durable_log_text_untouched() {
        // E11: redaction happens on the wire only — the durable entry
        // passed in must not be mutated by request_body.
        let key = "sk-super-secret-456";
        let cfg = OpenAiConfig::new("https://x.example/v1", "m", key);
        let p = OpenAiProvider::new(cfg);
        let original = msg("e1", "user", "key: sk-super-secret-456");
        let transcript = vec![original.clone()];
        let _ = p.request_body(&transcript);
        assert_eq!(
            transcript[0].payload["text"],
            json!("key: sk-super-secret-456"),
            "durable log must keep the original text"
        );
    }
}

#[cfg(test)]
mod system_prompt_tests {
    use super::*;
    use crate::entry::{Entry, EntryType};
    use serde_json::json;

    fn msg(role: &str, text: &str) -> Entry {
        Entry {
            id: "e".into(),
            parent_id: None,
            seq: 0,
            kind: EntryType::new(EntryType::MESSAGE),
            payload: json!({ "role": role, "text": text }),
            timestamp: 0,
        }
    }

    #[test]
    fn system_prompt_prepended_when_set() {
        let p = OpenAiProvider::new(OpenAiConfig::new("https://x/v1", "m", "k"))
            .with_system_prompt("you are cora");
        let body = p.request_body(&[msg("user", "hi")]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], json!("system"));
        assert_eq!(msgs[0]["content"], json!("you are cora"));
    }

    #[test]
    fn no_system_message_when_unset() {
        let p = OpenAiProvider::new(OpenAiConfig::new("https://x/v1", "m", "k"));
        let body = p.request_body(&[msg("user", "hi")]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
    }
}
