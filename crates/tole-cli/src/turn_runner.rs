//! Running one turn for a session, shared by every face (#428): the CLI
//! (`run`, `resume`, `chat`, missions) and the server faces (`serve`,
//! ACP, the MCP session tools).
//!
//! Starts with the provider: one place decides which model a session
//! turn talks to, with which tools, under which system prompt.

use std::sync::Arc;
use tole_core::memory::MemoryConfig;
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

static IMAGE_ROOTS: std::sync::OnceLock<Vec<std::path::PathBuf>> = std::sync::OnceLock::new();

/// Record the roots attached images may be read from (#212): the
/// workspace plus the inline-image spill dir. Set once at startup, before
/// any provider is built — a resumed session needs them to re-load the
/// images recorded in its log.
pub fn set_image_roots(workspace: Option<std::path::PathBuf>) {
    let roots = workspace.into_iter().chain(spill_dir()).collect();
    let _ = IMAGE_ROOTS.set(roots);
}

fn image_roots() -> Vec<std::path::PathBuf> {
    IMAGE_ROOTS.get().cloned().unwrap_or_default()
}

fn spill_dir() -> Option<std::path::PathBuf> {
    tole_core::paths::tole_data_dir().map(|d| d.join("images"))
}

/// Resolve `--image` specs (workspace-jailed paths or `data:` URLs) to
/// durable references. Fails loudly on the first bad one.
pub fn resolve_images(specs: &[String]) -> anyhow::Result<Vec<tole_core::image::ImageRef>> {
    let roots = image_roots();
    specs
        .iter()
        .map(|s| {
            tole_core::image::resolve(s, &roots, spill_dir().as_deref())
                .map_err(|e| anyhow::anyhow!("--image: {e}"))
        })
        .collect()
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

/// The reasoning effort a session turn sends (#457): the session's durable
/// `fact/reasoning_effort` register wins over the process default (flag >
/// env > config), exactly as `fact/model` does for the model. An empty or
/// invalid register value counts as unset, so it can clear an override
/// back to the default.
pub fn session_effort(storage: &impl Storage, default: Option<&str>) -> Option<String> {
    storage
        .get_register("fact", "reasoning_effort")
        .and_then(serde_json::Value::as_str)
        .and_then(|v| tole_core::openai::parse_reasoning_effort(v).ok())
        .or_else(|| default.map(str::to_string))
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
    let mut p = p.with_tool_specs(specs).with_image_roots(image_roots());
    // #212: TOLE_VISION=0|1 declares the model's capability; unset = the
    // server decides (an image rejection still becomes a capability error).
    match std::env::var("TOLE_VISION").ok().as_deref().map(str::trim) {
        Some("0" | "false" | "no") => p = p.with_vision(false),
        Some("1" | "true" | "yes") => p = p.with_vision(true),
        _ => {}
    }
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
    let effort = session_effort(storage, crate::config_apply::default_effort().as_deref());
    crate::provider_chain::build(cfg, &crate::config_apply::real_env, |p| {
        configure_provider(p, specs.clone(), system_prompt, observer)
            .with_reasoning_effort(effort.clone())
    })
}

/// Pre-turn recall (memory loop): the prompt with the recalled block
/// appended — what the provider sees and the durable log records. No
/// memory configured, nothing recalled, or a failed recall (reported on
/// stderr, #384) all yield the bare prompt: memory is an enhancement,
/// never a dependency.
pub fn with_recalled_memory(memory: Option<&MemoryConfig>, prompt: &str) -> String {
    let Some(mem) = memory else {
        return prompt.to_string();
    };
    match tole_core::memory::recall_block(mem, prompt) {
        Ok(block) if !block.is_empty() => {
            eprintln!("tole: memory: recalled context injected ({})", mem.room);
            format!("{prompt}{block}")
        }
        Ok(_) => prompt.to_string(),
        Err(e) => {
            eprintln!("tole: memory recall failed (continuing without): {e}");
            prompt.to_string()
        }
    }
}

/// Store a session summary (memory loop): best effort, reported on
/// stderr either way; the session itself is never affected. `first_prompt`
/// is the PRE-injection prompt, so recalled memory is never echoed back
/// into the store.
pub fn remember(
    memory: Option<&MemoryConfig>,
    session_id: &str,
    first_prompt: &str,
    last_answer: &str,
    wrote: bool,
) {
    let Some(mem) = memory else {
        return;
    };
    match tole_core::memory::remember_session(mem, session_id, first_prompt, last_answer, wrote) {
        Ok(_) => eprintln!("tole: memory: session summary stored in room {}", mem.room),
        Err(e) => eprintln!("tole: memory remember failed (session unaffected): {e}"),
    }
}

/// The cancellation token for a CLI-face session plus, in a child agent,
/// the sibling watcher that folds the shared touch log and queues change
/// notices on that token for the turn loop's next safe point (#380). A
/// no-op watcher (`None`) outside a child. Keep the watcher alive while
/// turns run: dropping it stops the thread.
pub fn cli_turn_token() -> (
    tole_core::cancel::CancelToken,
    Option<crate::sibling::Watcher>,
) {
    let cancel = tole_core::cancel::CancelToken::default();
    let watcher = crate::sibling::tracking_from_env(&crate::config_apply::real_env)
        .map(|t| crate::sibling::start_watcher(&t, &cancel, crate::sibling::POLL_INTERVAL));
    (cancel, watcher)
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
    /// A fake `uteke` that prints `body` and logs its argv.
    #[cfg(unix)]
    fn fake_uteke(tag: &str, body: &str) -> (MemoryConfig, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tole-turn-mem-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("uteke");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}/argv.log\n{body}\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = MemoryConfig {
            bin: bin.display().to_string(),
            room: "repo-x".into(),
            limit: 3,
        };
        (cfg, dir)
    }

    #[cfg(unix)]
    #[test]
    fn recall_appends_hits_and_degrades_to_the_bare_prompt() {
        assert_eq!(with_recalled_memory(None, "p"), "p");
        let (hit, d1) = fake_uteke("hit", "echo '[{\"score\":0.9,\"content\":\"a fact\"}]'");
        let out = with_recalled_memory(Some(&hit), "p");
        assert!(out.starts_with('p') && out.contains("a fact"), "{out}");
        let (empty, d2) = fake_uteke("empty", "echo '[]'");
        assert_eq!(with_recalled_memory(Some(&empty), "p"), "p");
        let (broken, d3) = fake_uteke("broken", "exit 3");
        assert_eq!(with_recalled_memory(Some(&broken), "p"), "p");
        for d in [d1, d2, d3] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[cfg(unix)]
    #[test]
    fn remember_stores_the_pre_injection_prompt_in_the_room() {
        remember(None, "s", "p", "a", false); // no memory: a no-op
        let (cfg, dir) = fake_uteke("remember", "echo id");
        remember(Some(&cfg), "s-1", "bare prompt", "the answer", true);
        let argv = std::fs::read_to_string(dir.join("argv.log")).unwrap();
        assert!(
            argv.starts_with("remember [tole session s-1] bare prompt → the answer"),
            "{argv}"
        );
        assert!(
            argv.contains("--room repo-x") && argv.contains("--type decision"),
            "{argv}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_effort_register_wins_over_the_default() {
        let dir = std::env::temp_dir().join(format!("tole-effort-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = tole_core::storage::JsonlStorage::create(&dir, "e1", None).unwrap();
        assert_eq!(session_effort(&st, Some("low")).as_deref(), Some("low"));
        assert_eq!(session_effort(&st, None), None);
        let set = |st: &mut tole_core::storage::JsonlStorage, v: serde_json::Value| {
            st.commit(tole_core::storage::Commit::new().register(
                tole_core::register::RegisterWrite::set("fact", "reasoning_effort", v),
            ))
            .unwrap();
        };
        set(&mut st, serde_json::json!(" High "));
        assert_eq!(session_effort(&st, Some("low")).as_deref(), Some("high"));
        set(&mut st, serde_json::json!("turbo"));
        assert_eq!(
            session_effort(&st, Some("low")).as_deref(),
            Some("low"),
            "an invalid register value falls back to the default"
        );
        set(&mut st, serde_json::json!(""));
        assert_eq!(session_effort(&st, Some("low")).as_deref(), Some("low"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
