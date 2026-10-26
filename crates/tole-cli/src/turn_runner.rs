//! Running one turn for a session, shared by every face (#428): the CLI
//! (`run`, `resume`, `chat`, missions) and the server faces (`serve`,
//! ACP, the MCP session tools).
//!
//! Starts with the provider: one place decides which model a session
//! turn talks to, with which tools, under which system prompt.

use std::sync::Arc;
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::Provider;
use tole_core::storage::Storage;
use tole_core::turn::TurnObserver;

/// The one wording of a missing provider configuration.
pub const MISSING_PROVIDER_CONFIG: &str = "missing provider config: set TOLE_BASE_URL / \
     TOLE_MODEL / TOLE_API_KEY (or the OPENAI_* equivalents)";

/// The env provider configuration, or [`MISSING_PROVIDER_CONFIG`]. Hosts
/// call this before creating a session file, so a misconfigured start
/// leaves no empty session behind.
pub fn provider_config() -> Result<OpenAiConfig, String> {
    crate::config_apply::provider_config().ok_or_else(|| MISSING_PROVIDER_CONFIG.to_string())
}

/// The model a session turn uses: the session's durable `fact/model`
/// register (set by ACP's model picker, #176) wins over the configured
/// default, so a session keeps its model across faces and resumes.
pub fn session_model(storage: &impl Storage, default: &str) -> String {
    storage
        .get_register("fact", "model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(default)
        .to_string()
}

/// Configure one provider of the chain for a session turn: the
/// registry's tool specs, the session's pinned system prompt (fresh and
/// resumed sessions alike read it from the header), and, when an
/// observer is wired (ACP), live text/reasoning deltas.
pub fn configure_provider(
    p: OpenAiProvider,
    specs: Vec<serde_json::Value>,
    system_prompt: Option<&str>,
    observer: Option<&Arc<dyn TurnObserver>>,
) -> OpenAiProvider {
    let mut p = p.with_tool_specs(specs);
    if let Some(sys) = system_prompt {
        p = p.with_system_prompt(sys);
    }
    if let Some(o) = observer {
        let (text, reasoning) = (Arc::clone(o), Arc::clone(o));
        p = p.with_delta_sinks(
            Some(Arc::new(move |t: &str| text.text_delta(t))),
            Some(Arc::new(move |r: &str| reasoning.reasoning(r))),
        );
    }
    p
}

/// The provider (with its failover chain, #209) for one turn of a
/// session: `cfg` with the session's model ([`session_model`]), each link
/// configured by [`configure_provider`].
pub fn session_provider(
    mut cfg: OpenAiConfig,
    storage: &impl Storage,
    specs: Vec<serde_json::Value>,
    system_prompt: Option<&str>,
    observer: Option<&Arc<dyn TurnObserver>>,
) -> Result<Box<dyn Provider>, String> {
    cfg.model = session_model(storage, &cfg.model);
    crate::provider_chain::build(cfg, &crate::config_apply::real_env, |p| {
        configure_provider(p, specs.clone(), system_prompt, observer)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::register::RegisterWrite;
    use tole_core::storage::{Commit, JsonlStorage};

    fn storage(tag: &str) -> (JsonlStorage, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("tole-turn-runner-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (JsonlStorage::create(&dir, "s", None).unwrap(), dir)
    }

    #[test]
    fn the_session_model_register_wins_over_the_default() {
        let (mut s, dir) = storage("model");
        assert_eq!(session_model(&s, "env-model"), "env-model");
        s.commit(Commit::new().register(RegisterWrite::set(
            "fact",
            "model",
            serde_json::json!("picked"),
        )))
        .unwrap();
        assert_eq!(session_model(&s, "env-model"), "picked");
        s.commit(Commit::new().register(RegisterWrite::set(
            "fact",
            "model",
            serde_json::json!("  "),
        )))
        .unwrap();
        assert_eq!(
            session_model(&s, "env-model"),
            "env-model",
            "blank falls back"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_config_message_is_one_clean_line() {
        assert!(
            !MISSING_PROVIDER_CONFIG.contains("  "),
            "{MISSING_PROVIDER_CONFIG}"
        );
        assert!(MISSING_PROVIDER_CONFIG.contains("TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY"));
    }
}
