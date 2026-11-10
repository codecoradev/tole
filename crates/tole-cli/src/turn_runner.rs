//! Running one turn for a session, shared by every face (#428): the CLI
//! (`run`, `resume`, `chat`, missions) and the server faces (`serve`,
//! ACP, the MCP session tools).
//!
//! Starts with the provider: one place decides which model a session
//! turn talks to, with which tools, under which system prompt.

use std::sync::Arc;
use tole_core::cancel::CancelToken;
use tole_core::memory::MemoryConfig;
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::Provider;
use tole_core::state::Pc;
use tole_core::storage::{Storage, StorageError};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{self, TurnObserver, TurnOutcome};

use crate::compaction_host::CompactionSettings;

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
        .get_register(
            crate::session_settings::MODEL.0,
            crate::session_settings::MODEL.1,
        )
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
        .get_register(
            crate::session_settings::EFFORT.0,
            crate::session_settings::EFFORT.1,
        )
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

/// Everything one turn needs besides the session's storage and the prompt
/// (#525): the one description of "how a turn is run" that the server faces
/// (`serve`, ACP, MCP) and the TUI share. What differs between faces is
/// explicit here, not copied code:
///
/// - the server faces start a turn straight away and never compact;
/// - the TUI settles a turn parked mid-flight first and compacts when due.
pub struct TurnPlan<'a> {
    /// Provider connection settings (the model register may override the
    /// model per session, see [`session_provider`]).
    pub cfg: OpenAiConfig,
    pub registry: &'a ToolRegistry,
    pub system_prompt: Option<&'a str>,
    pub cancel: &'a CancelToken,
    /// Live text/tool callbacks; attaching one turns provider streaming on.
    pub observer: Option<&'a Arc<dyn TurnObserver>>,
    /// Settle a turn parked mid-flight (a provider failure, ...) before a
    /// new prompt starts. When settling does not end `Final`, that outcome
    /// IS the answer and the new prompt is not run.
    pub resolve_parked: bool,
    /// Compact before the turn when due (`None` = never).
    pub compaction: Option<CompactionSettings>,
}

/// Why a turn could not run to an outcome.
#[derive(Debug)]
pub enum TurnRunError {
    /// The provider could not be built (bad config, no failover target, ...).
    Provider(String),
    /// The session's storage failed.
    Storage(StorageError),
}

impl TurnRunError {
    /// The plain message the server faces return (the TUI adds its own
    /// `storage error:` prefix).
    pub fn into_message(self) -> String {
        match self {
            TurnRunError::Provider(e) => e,
            TurnRunError::Storage(e) => e.to_string(),
        }
    }
}

impl TurnPlan<'_> {
    fn provider(&self, storage: &impl Storage) -> Result<Box<dyn Provider>, TurnRunError> {
        session_provider(
            self.cfg.clone(),
            storage,
            self.registry.specs(),
            self.system_prompt,
            self.observer,
        )
        .map_err(TurnRunError::Provider)
    }
}

/// Run one user turn on `storage`: build the session's provider, then
/// [`run_turn_with`].
pub fn run_turn_on<S: Storage>(
    storage: &mut S,
    plan: &TurnPlan<'_>,
    prompt: &str,
) -> Result<TurnOutcome, TurnRunError> {
    let mut provider = plan.provider(&*storage)?;
    run_turn_with(storage, &mut *provider, plan, prompt)
}

/// The turn itself, with a provider already built: settle a parked turn
/// (when the plan asks), compact (when the plan asks and it is due), then run
/// the prompt.
pub fn run_turn_with<S: Storage>(
    storage: &mut S,
    provider: &mut dyn Provider,
    plan: &TurnPlan<'_>,
    prompt: &str,
) -> Result<TurnOutcome, TurnRunError> {
    if plan.resolve_parked && !matches!(storage.state().pc, Pc::Idle | Pc::Final) {
        match turn::resume_turn_with_cancel(storage, provider, plan.registry, plan.cancel)
            .map_err(TurnRunError::Storage)?
        {
            TurnOutcome::Final { .. } => {}
            other => return Ok(other),
        }
    }
    if let Some(settings) = plan.compaction {
        crate::compaction_host::for_session(settings, &plan.cfg, &*storage)
            .before_turn(storage, provider);
    }
    turn::run_turn_with_observer(
        storage,
        provider,
        plan.registry,
        prompt,
        plan.cancel,
        plan.observer.map(|o| o.as_ref()),
    )
    .map_err(TurnRunError::Storage)
}

/// Approvals-only recovery (#200): no new prompt, settle the parked guarded
/// effect and replan to completion.
pub fn resume_turn_on<S: Storage>(
    storage: &mut S,
    plan: &TurnPlan<'_>,
) -> Result<TurnOutcome, TurnRunError> {
    let mut provider = plan.provider(&*storage)?;
    turn::resume_turn_with_cancel(storage, &mut *provider, plan.registry, plan.cancel)
        .map_err(TurnRunError::Storage)
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

    // ---- the shared turn runner (#525) ----

    use tole_core::mock::MockProvider;
    use tole_core::provider::ProviderOutput;

    fn final_out(t: &str) -> ProviderOutput {
        ProviderOutput::Final { text: t.into() }
    }

    fn plan<'a>(
        registry: &'a ToolRegistry,
        cancel: &'a CancelToken,
        resolve_parked: bool,
    ) -> TurnPlan<'a> {
        TurnPlan {
            cfg: OpenAiConfig::new("http://127.0.0.1:1/v1", "m", "k"),
            registry,
            system_prompt: None,
            cancel,
            observer: None,
            resolve_parked,
            compaction: None,
        }
    }

    fn text_of(o: &TurnOutcome) -> &str {
        match o {
            TurnOutcome::Final { text, .. } => text,
            other => panic!("expected Final, got {other:?}"),
        }
    }

    /// Park a turn the way a crash does: the user message is durable and the
    /// machine is mid-flight (`Planning`), as in the core's resume tests.
    fn park(storage: &mut JsonlStorage) {
        use tole_core::entry::{EntryType, NewEntry};
        use tole_core::state::StateTransition;
        let seq = storage.state().seq;
        storage
            .commit(
                Commit::new()
                    .entry(NewEntry::root(
                        EntryType::new(EntryType::MESSAGE),
                        serde_json::json!({"role": "user", "text": "parked prompt"}),
                    ))
                    .transition(StateTransition::from(seq, Pc::Planning)),
            )
            .unwrap();
        assert_eq!(storage.state().pc, Pc::Planning);
    }

    #[test]
    fn a_plain_turn_runs_the_prompt_and_appends_to_the_log() {
        let (mut s, dir) = storage("plain");
        let reg = ToolRegistry::new();
        let cancel = CancelToken::default();
        let mut p = MockProvider::scripted(vec![final_out("hello")]);
        let out = run_turn_with(&mut s, &mut p, &plan(&reg, &cancel, false), "hi").unwrap();
        assert_eq!(text_of(&out), "hello");
        assert!(
            s.entries().len() >= 2,
            "user message and answer are durable"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_parked_turn_is_settled_first_only_when_the_plan_asks() {
        let reg = ToolRegistry::new();
        let cancel = CancelToken::default();

        // TUI-style: the parked turn is resolved (consuming one provider
        // output), THEN the new prompt runs (consuming the next).
        let (mut s, dir) = storage("parked-yes");
        park(&mut s);
        let mut p = MockProvider::scripted(vec![final_out("settled"), final_out("answer")]);
        let out = run_turn_with(&mut s, &mut p, &plan(&reg, &cancel, true), "second").unwrap();
        assert_eq!(text_of(&out), "answer");
        assert!(
            s.entries()
                .iter()
                .any(|e| e.payload.to_string().contains("settled")),
            "the parked turn finished before the new prompt"
        );
        let _ = std::fs::remove_dir_all(dir);

        // Server-style (the plan does not ask): the core refuses to start a
        // prompt on a parked session ("resolve the session first"). That is
        // what the server faces always did, and it is why the TUI asks for
        // settling: the refusal surfaces as a storage error, nothing runs.
        let (mut s, dir) = storage("parked-no");
        park(&mut s);
        let before = s.entries().len();
        let mut p = MockProvider::scripted(vec![final_out("answer")]);
        match run_turn_with(&mut s, &mut p, &plan(&reg, &cancel, false), "second") {
            Err(TurnRunError::Storage(e)) => {
                assert!(e.to_string().contains("resolve the session first"), "{e}")
            }
            other => panic!("expected the core's refusal, got {other:?}"),
        }
        assert_eq!(s.entries().len(), before, "nothing was written");
        let _ = std::fs::remove_dir_all(dir);

        // When settling does NOT end Final, that outcome is the answer and
        // the new prompt is not run.
        let (mut s, dir) = storage("parked-stop");
        park(&mut s);
        let mut p = MockProvider::scripted_with_failures(vec![]);
        let out = run_turn_with(&mut s, &mut p, &plan(&reg, &cancel, true), "second").unwrap();
        assert!(matches!(out, TurnOutcome::ProviderFailed { .. }), "{out:?}");
        assert!(
            !s.entries()
                .iter()
                .any(|e| e.payload.to_string().contains("second")),
            "the new prompt was not started"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compaction_runs_only_when_the_plan_asks() {
        use crate::compaction_host::CompactionSettings;
        let reg = ToolRegistry::new();
        let cancel = CancelToken::default();
        let seeded = |tag: &str| {
            let (mut s, dir) = storage(tag);
            for n in 0..4 {
                let mut p = MockProvider::scripted(vec![final_out(&format!("a{n}"))]);
                run_turn_with(
                    &mut s,
                    &mut p,
                    &plan(&reg, &cancel, false),
                    &format!("q{n}"),
                )
                .unwrap();
            }
            // The forced pass of #481: the next prompt compacts first.
            tole_core::compaction::mark_overflow(&mut s).unwrap();
            assert!(tole_core::compaction::overflow_pending(&s));
            (s, dir)
        };

        // The plan asks: the pending pass runs first (consuming the summary
        // output) and then the prompt (consuming the answer).
        let (mut s, dir) = seeded("compact-on");
        let mut on = plan(&reg, &cancel, false);
        on.compaction = Some(CompactionSettings::default());
        let mut p = MockProvider::scripted(vec![final_out("SUMMARY"), final_out("two")]);
        let out = run_turn_with(&mut s, &mut p, &on, "second").unwrap();
        assert_eq!(text_of(&out), "two");
        assert!(!tole_core::compaction::overflow_pending(&s), "the pass ran");
        let _ = std::fs::remove_dir_all(dir);

        // The plan does not ask (the server faces): nothing is compacted and
        // the first output answers the prompt.
        let (mut s, dir) = seeded("compact-off");
        let mut p = MockProvider::scripted(vec![final_out("two")]);
        let out = run_turn_with(&mut s, &mut p, &plan(&reg, &cancel, false), "second").unwrap();
        assert_eq!(text_of(&out), "two");
        assert!(tole_core::compaction::overflow_pending(&s), "no pass ran");
        let _ = std::fs::remove_dir_all(dir);

        // Enabled = false in the settings is off even when the plan asks.
        let (mut s, dir) = seeded("compact-disabled");
        let mut off = plan(&reg, &cancel, false);
        off.compaction = Some(CompactionSettings {
            enabled: false,
            ..CompactionSettings::default()
        });
        let mut p = MockProvider::scripted(vec![final_out("two")]);
        let out = run_turn_with(&mut s, &mut p, &off, "second").unwrap();
        assert_eq!(text_of(&out), "two");
        assert!(tole_core::compaction::overflow_pending(&s));
        let _ = std::fs::remove_dir_all(dir);
    }
}
