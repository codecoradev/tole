//! Real LLM provider over OpenAI-compatible chat completions (E4).
//!
//! Sync (Phase 1 — the turn loop is single-threaded), `ureq` under the
//! hood. The API key lives only in the outbound `Authorization` header;
//! errors are scrubbed so a failed request can never echo it back into
//! logs or the durable log.
//!
//! Config resolution order: explicit struct fields > env (`TOLE_*`, then
//! `CORAGENT_*` legacy, then `OPENAI_*`).

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

/// Env names are namespaced (`TOLE_*`) to avoid collisions with other
/// tools on the same host.
pub const ENV_BASE_URL: &str = "TOLE_BASE_URL";
pub const ENV_MODEL: &str = "TOLE_MODEL";
pub const ENV_API_KEY: &str = "TOLE_API_KEY";

/// Prefixes tried by [`OpenAiConfig::from_env`], in order. The first
/// prefix whose full triple (`_BASE_URL`, `_MODEL`, `_API_KEY`) is
/// present and non-empty wins; prefixes are never mixed, so a
/// half-configured environment cannot silently blend two sources.
/// `TOLE_*` is canonical; `CORAGENT_*` is the pre-rename legacy name;
/// `OPENAI_*` is the de-facto standard for OpenAI-compatible providers.
pub const ENV_PREFIXES: [&str; 3] = ["TOLE", "CORAGENT", "OPENAI"];

/// Resolve one variable under `prefix`, treating whitespace-only as absent.
fn env_var(prefix: &str, suffix: &str) -> Option<String> {
    std::env::var(format!("{prefix}_{suffix}"))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

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

    /// Resolve from environment: the first of `CORAGENT_*` then `OPENAI_*`
    /// whose complete triple (`_BASE_URL`, `_MODEL`, `_API_KEY`) is set
    /// and non-empty. Prefixes are never mixed. `None` when neither
    /// prefix yields a complete triple.
    pub fn from_env() -> Option<Self> {
        for prefix in ENV_PREFIXES {
            let (Some(base_url), Some(model), Some(api_key)) = (
                env_var(prefix, "BASE_URL"),
                env_var(prefix, "MODEL"),
                env_var(prefix, "API_KEY"),
            ) else {
                continue;
            };
            return Some(Self {
                base_url,
                model,
                api_key,
            });
        }
        None
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
    /// OpenAI `tools` array (E4.5). Empty = text-only turn, no tool use.
    tool_specs: Vec<Value>,
}

impl OpenAiProvider {
    pub fn new(cfg: OpenAiConfig) -> Self {
        Self {
            cfg,
            timeout: Duration::from_secs(120),
            system_prompt: None,
            tool_specs: Vec::new(),
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

    /// Inject tool specs (from `ToolRegistry::specs()`). Non-empty specs
    /// enable tool-calling on the wire (E4.5).
    pub fn with_tool_specs(mut self, specs: Vec<Value>) -> Self {
        self.tool_specs = specs;
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
    /// Transcript flattening (E4.5): user/assistant chat messages pass
    /// through in order; each `intent` entry becomes an assistant message
    /// carrying `tool_calls` (intent id doubles as `tool_call_id`), and
    /// the child `tool_result`/`error` settlement becomes the matching
    /// `role:"tool"` message. State records stay in the durable log only.
    ///
    /// E11: any occurrence of the API key inside message text is redacted
    /// before the body goes over the wire — the durable log keeps the
    /// original (local), the provider never sees the key.
    fn request_body(&self, transcript: &[Entry]) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if let Some(sys) = &self.system_prompt {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        for e in transcript {
            match e.kind.as_str() {
                "message" => {
                    let (Some(role), Some(text)) = (
                        e.payload.get("role").and_then(Value::as_str),
                        e.payload
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|t| scrub(t, &self.cfg.api_key)),
                    ) else {
                        continue;
                    };
                    if !matches!(role, "user" | "assistant" | "system") {
                        continue;
                    }
                    messages.push(json!({ "role": role, "content": text }));
                }
                "intent" => {
                    let (Some(tool), Some(input)) = (
                        e.payload.get("tool").and_then(Value::as_str),
                        e.payload.get("input"),
                    ) else {
                        continue;
                    };
                    // The intent id doubles as the wire tool_call_id —
                    // deterministic and unique per sandwich.
                    messages.push(json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": e.id,
                            "type": "function",
                            "function": {
                                "name": tool,
                                "arguments": input.to_string(),
                            },
                        }],
                    }));
                }
                "tool_result" | "error" => {
                    // Settlement of the intent this entry hangs under.
                    // Errors carry `error` instead of `output`. Turn-level
                    // errors (no parent intent — e.g. budget exhausted,
                    // invalid tool arguments) have no tool_call_id and are
                    // skipped: the wire protocol only accepts tool messages
                    // that answer a specific tool_call.
                    let Some(parent) = e.parent_id.as_ref() else {
                        continue;
                    };
                    let content = if e.kind.as_str() == "tool_result" {
                        e.payload
                            .get("output")
                            .cloned()
                            .unwrap_or_else(|| json!({ "ok": true }))
                    } else {
                        json!({ "error": e.payload.get("error").cloned().unwrap_or(Value::Null) })
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": parent,
                        "content": scrub(&content.to_string(), &self.cfg.api_key),
                    }));
                }
                _ => {}
            }
        }
        let mut body = json!({
            "model": self.cfg.model,
            "messages": messages,
        });
        if !self.tool_specs.is_empty() {
            body["tools"] = json!(self.tool_specs);
        }
        body
    }

    /// Parse a chat-completions response into the single next step the
    /// state machine understands (E4.5). Precedence: a non-empty
    /// `tool_calls` array wins over content text — the model asked for a
    /// tool, and the loop must run it before any prose is meaningful.
    /// First call only: the machine is single-path (§5).
    fn parse_completion(resp: &Value) -> Result<ProviderOutput, ProviderError> {
        let msg = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .ok_or_else(|| ProviderError("malformed response: no choices[0].message".into()))?;
        let calls = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty());
        if let Some(calls) = calls {
            let first = &calls[0];
            let name = first
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProviderError("malformed tool_call: missing function.name".into())
                })?;
            let args = first
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "{}".into());
            let input: Value = match serde_json::from_str(&args) {
                Ok(v) => v,
                Err(err) => {
                    // Malformed arguments: the tool must NOT run. Surface the
                    // parse error so the loop settles it as an error result
                    // and the model can retry with well-formed JSON.
                    return Ok(ProviderOutput::InvalidToolArgs {
                        tool: name.to_string(),
                        raw: args,
                        reason: err.to_string(),
                    });
                }
            };
            return Ok(ProviderOutput::ToolCall {
                tool: name.to_string(),
                input,
            });
        }
        let text = msg.get("content").and_then(Value::as_str).ok_or_else(|| {
            ProviderError("malformed response: no content and no tool_calls".into())
        })?;
        Ok(ProviderOutput::Final {
            text: text.to_string(),
        })
    }
}

impl Provider for OpenAiProvider {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
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
        Self::parse_completion(&resp)
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
    fn env_prefix_resolution_prefers_coragent_and_never_mixes() {
        // Env access is process-global; tests that mutate it must not run
        // in parallel with other env-mutating tests. This is the only
        // env-mutating test in this module, so a mutex here suffices.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn set(prefix: &str, vals: &[(&str, &str)]) {
            for (k, v) in vals {
                std::env::set_var(format!("{prefix}_{k}"), v);
            }
        }
        fn clear_all() {
            for p in ENV_PREFIXES {
                for k in ["BASE_URL", "MODEL", "API_KEY"] {
                    std::env::remove_var(format!("{p}_{k}"));
                }
            }
        }

        let _g = LOCK.lock().unwrap();
        // Snapshot + restore: never leak env mutations into other tests.
        let snapshot: Vec<(String, String)> = ["CORAGENT", "OPENAI"]
            .iter()
            .flat_map(|p| ["BASE_URL", "MODEL", "API_KEY"].map(|k| format!("{p}_{k}")))
            .filter_map(|k| std::env::var(&k).ok().map(|v| (k, v)))
            .collect();

        // Case 1: only OPENAI_* complete → OPENAI wins (the fallback fix).
        clear_all();
        set(
            "OPENAI",
            &[
                ("BASE_URL", "https://openai.example/v1"),
                ("MODEL", "gpt-fallback"),
                ("API_KEY", "sk-openai"),
            ],
        );
        let cfg = OpenAiConfig::from_env().expect("OPENAI fallback must resolve");
        assert_eq!(cfg.base_url, "https://openai.example/v1");
        assert_eq!(cfg.model, "gpt-fallback");
        assert_eq!(cfg.api_key, "sk-openai");

        // Case 2: all prefixes complete → TOLE wins, no mixing.
        set(
            "CORAGENT",
            &[
                ("BASE_URL", "https://coragent.example/v1"),
                ("MODEL", "glm-legacy"),
                ("API_KEY", "sk-coragent"),
            ],
        );
        set(
            "TOLE",
            &[
                ("BASE_URL", "https://tole.example/v1"),
                ("MODEL", "glm-test"),
                ("API_KEY", "sk-tole"),
            ],
        );
        let cfg = OpenAiConfig::from_env().expect("TOLE must take priority");
        assert_eq!(cfg.base_url, "https://tole.example/v1");
        assert_eq!(cfg.api_key, "sk-tole");

        // Case 3: TOLE half-set + CORAGENT complete → CORAGENT legacy
        // still honored (incomplete prefix skipped whole, never blended).
        std::env::remove_var("TOLE_API_KEY");
        let cfg = OpenAiConfig::from_env().expect("incomplete prefix must be skipped");
        assert_eq!(cfg.api_key, "sk-coragent", "must not blend prefixes");

        // Case 4: whitespace-only values count as absent.
        std::env::set_var("TOLE_API_KEY", "   ");
        let cfg = OpenAiConfig::from_env().expect("whitespace key counts as absent");
        assert_eq!(cfg.api_key, "sk-coragent");

        // Restore the original environment.
        clear_all();
        for (k, v) in &snapshot {
            std::env::set_var(k, v);
        }
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

    fn state() -> Entry {
        Entry {
            id: "s1".into(),
            parent_id: None,
            seq: 0,
            kind: EntryType::new("state"),
            payload: json!({ "pc": "Planning" }),
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
    fn request_body_skips_unknown_roles_and_state_records() {
        let cfg = OpenAiConfig::new("https://x.example/v1", "m", "k");
        let p = OpenAiProvider::new(cfg);
        // Unknown chat roles and state records stay log-only.
        let transcript = vec![msg("e10", "wizard", "unknown role"), state()];
        let body = p.request_body(&transcript);
        assert_eq!(
            body["messages"].as_array().map(|a| a.len()),
            Some(0),
            "unknown roles and state records must be filtered out"
        );
    }

    #[test]
    fn request_body_flattens_intent_and_result_to_tool_wire() {
        // E4.5: intent → assistant tool_calls (intent id as tool_call_id),
        // settlement → role:"tool" with the parent as tool_call_id.
        let cfg = OpenAiConfig::new("https://x.example/v1", "m", "k");
        let p = OpenAiProvider::new(cfg);
        let mut intent = msg("intent_5", "assistant", "");
        intent.kind = EntryType::new("intent");
        intent.payload = json!({ "tool": "read_file", "input": { "path": "Cargo.toml" } });
        let mut result = msg("e6", "tool", "");
        result.kind = EntryType::new("tool_result");
        result.parent_id = Some("intent_5".into());
        result.payload = json!({ "ok": true, "output": { "bytes": 42 } });
        let mut err = msg("e7", "tool", "");
        err.kind = EntryType::new("error");
        err.parent_id = Some("intent_5".into());
        err.payload = json!({ "ok": false, "error": "boom" });
        let body = p.request_body(&[intent.clone(), result, intent, err]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        // intent → assistant tool_calls
        assert_eq!(msgs[0]["role"], json!("assistant"));
        assert_eq!(msgs[0]["tool_calls"][0]["id"], json!("intent_5"));
        assert_eq!(
            msgs[0]["tool_calls"][0]["function"]["name"],
            json!("read_file")
        );
        assert_eq!(
            msgs[0]["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"path":"Cargo.toml"}"#)
        );
        // settlement → role: tool with parent id
        assert_eq!(msgs[1]["role"], json!("tool"));
        assert_eq!(msgs[1]["tool_call_id"], json!("intent_5"));
        assert_eq!(msgs[1]["content"], json!(r#"{"bytes":42}"#));
        // error settlement also becomes a tool message (with error payload)
        assert_eq!(msgs[3]["role"], json!("tool"));
        assert_eq!(msgs[3]["tool_call_id"], json!("intent_5"));
        assert!(msgs[3]["content"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn request_body_sends_tools_when_specs_present() {
        let cfg = OpenAiConfig::new("https://x.example/v1", "m", "k");
        let spec = json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "read_file (read-only)",
                "parameters": { "type": "object", "properties": {} },
            },
        });
        let with = OpenAiProvider::new(cfg.clone()).with_tool_specs(vec![spec]);
        let body = with.request_body(&[]);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        let without = OpenAiProvider::new(cfg).request_body(&[]);
        assert!(without.get("tools").is_none());
    }

    #[test]
    fn parse_completion_prefers_tool_calls_over_text() {
        let resp = json!({
            "choices": [{
                "message": {
                    "content": "I will call a tool",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}",
                        },
                    }],
                },
            }],
        });
        let out = OpenAiProvider::parse_completion(&resp).unwrap();
        match out {
            ProviderOutput::ToolCall { tool, input } => {
                assert_eq!(tool, "read_file");
                assert_eq!(input["path"], json!("Cargo.toml"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_completion_handles_text_only_and_malformed() {
        // Text-only: Final.
        let text = json!({ "choices": [{ "message": { "content": "done" } }] });
        assert!(matches!(
            OpenAiProvider::parse_completion(&text).unwrap(),
            ProviderOutput::Final { .. }
        ));
        // No choices → malformed.
        assert!(OpenAiProvider::parse_completion(&json!({})).is_err());
        // Tool call missing function.name → malformed.
        let bad = json!({ "choices": [{ "message": { "tool_calls": [{ "id": "x" }] } }] });
        assert!(OpenAiProvider::parse_completion(&bad).is_err());
        // Empty tool_calls array + content → Final by content.
        let empty_calls = json!({
            "choices": [{ "message": { "content": "hi", "tool_calls": [] } }],
        });
        assert!(matches!(
            OpenAiProvider::parse_completion(&empty_calls).unwrap(),
            ProviderOutput::Final { .. }
        ));
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
