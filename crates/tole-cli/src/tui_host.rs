//! Host side of the TUI (#460): owns storage, registry and provider config,
//! runs one turn at a time for `tole-tui`, and composes the footer from
//! `status::session_status` (the tui crate cannot depend on this one).

use std::sync::{Arc, Mutex};

use tole_core::approval::{ToolRequest, Verdict};
use tole_core::cancel::CancelToken;
use tole_core::openai::OpenAiConfig;
use tole_core::state::Pc;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, ToolRegistry};
use tole_core::turn::{self, TurnObserver, TurnOutcome};
use tole_tui::{Footer, Prompter, TuiHost, TurnEnd};

use crate::approver::PromptFn;
use crate::compaction_host::{CompactionSettings, ModelsCatalog, WindowSource};
use crate::status::session_status;

/// The inline y/n prompt: the approver (allowlist, `--yes`, Destructive rules
/// stay in `InteractiveApprover`) asks through the TUI event loop.
pub struct TuiPrompt(pub Prompter);

impl PromptFn for TuiPrompt {
    fn prompt(&self, req: &ToolRequest<'_>) -> Verdict {
        let target = req.target.as_deref().map(|t| format!(" → {t}"));
        self.0.ask(
            format!(
                "{} — {}{}",
                req.tool,
                req.description,
                target.unwrap_or_default()
            ),
            req.risk == Risk::Destructive,
        )
    }
}

/// `/models` is asked at most once per session (the footer refreshes every turn).
struct CachedWindow<'a> {
    catalog: ModelsCatalog,
    slot: &'a mut Option<Option<u64>>,
}

impl WindowSource for CachedWindow<'_> {
    fn window(&mut self) -> Option<u64> {
        *self.slot.get_or_insert_with(|| self.catalog.window())
    }
}

struct Inner {
    storage: JsonlStorage,
    window: Option<Option<u64>>,
}

pub struct TuiSession {
    inner: Mutex<Inner>,
    registry: ToolRegistry,
    cfg: OpenAiConfig,
    system_prompt: Option<String>,
    compaction: CompactionSettings,
    folder: String,
    session_id: String,
}

impl TuiSession {
    pub fn new(
        storage: JsonlStorage,
        registry: ToolRegistry,
        cfg: OpenAiConfig,
        compaction: CompactionSettings,
    ) -> Self {
        let session_id = storage.session_id().to_string();
        let folder = std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| ".".into());
        let system_prompt = storage.system_prompt().map(str::to_string);
        Self {
            inner: Mutex::new(Inner {
                storage,
                window: None,
            }),
            registry,
            cfg,
            system_prompt,
            compaction,
            folder,
            session_id,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Footer fields from the status snapshot; `effort` is the effective value
/// (register, else the process default).
pub fn footer_from(
    status: &crate::status::SessionStatus,
    effective_effort: Option<String>,
    folder: &str,
    session: &str,
) -> Footer {
    Footer {
        folder: folder.to_string(),
        session: session.to_string(),
        model: status.model.clone(),
        effort: effective_effort,
        context: status.context_line(),
        tokens: format!(
            "tokens: {} in / {} out",
            status.input_tokens, status.output_tokens
        ),
    }
}

impl TuiHost for TuiSession {
    fn footer(&self) -> Footer {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Inner { storage, window } = &mut *g;
        let model = crate::turn_runner::session_model(&*storage, &self.cfg.model);
        let mut src = CachedWindow {
            catalog: ModelsCatalog::new(&self.cfg, &model),
            slot: window,
        };
        let status = session_status(&*storage, Some(&self.cfg.model), &self.compaction, &mut src);
        let effort = crate::turn_runner::session_effort(
            &*storage,
            crate::config_apply::default_effort().as_deref(),
        );
        footer_from(&status, effort, &self.folder, &self.session_id)
    }

    fn run_turn(
        &self,
        prompt: &str,
        cancel: &CancelToken,
        observer: Arc<dyn TurnObserver>,
    ) -> TurnEnd {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let storage = &mut g.storage;
        let mut provider = match crate::turn_runner::session_provider(
            self.cfg.clone(),
            &*storage,
            self.registry.specs(),
            self.system_prompt.as_deref(),
            Some(&observer),
        ) {
            Ok(p) => p,
            Err(e) => return TurnEnd::Stopped(e),
        };
        // A turn parked mid-flight (provider failure, ...) is resolved first.
        if !matches!(storage.state().pc, Pc::Idle | Pc::Final) {
            match turn::resume_turn_with_cancel(storage, &mut *provider, &self.registry, cancel) {
                Ok(TurnOutcome::Final { .. }) => {}
                Ok(other) => return map(other),
                Err(e) => return TurnEnd::Stopped(format!("storage error: {e}")),
            }
        }
        crate::compaction_host::for_session(self.compaction, &self.cfg, &*storage)
            .before_turn(storage, &mut *provider);
        match turn::run_turn_with_observer(
            storage,
            &mut *provider,
            &self.registry,
            prompt,
            cancel,
            Some(&*observer),
        ) {
            Ok(o) => map(o),
            Err(e) => TurnEnd::Stopped(format!("storage error: {e}")),
        }
    }
}

fn map(o: TurnOutcome) -> TurnEnd {
    match o {
        TurnOutcome::Final { text, .. } => TurnEnd::Final(text),
        TurnOutcome::Cancelled => TurnEnd::Cancelled,
        TurnOutcome::ApprovalRequired { name } => TurnEnd::Stopped(format!(
            "approval denied for '{name}'; your next message resumes"
        )),
        TurnOutcome::ProviderFailed { message } => TurnEnd::Stopped(format!(
            "provider failed: {message}{}",
            crate::compaction_host::overflow_hint(&message)
        )),
        TurnOutcome::UnknownTool { name } => TurnEnd::Stopped(format!("unknown tool '{name}'")),
        TurnOutcome::BudgetExhausted => TurnEnd::Stopped("step budget exhausted".into()),
        TurnOutcome::StopGateBlocked { reason } => {
            TurnEnd::Stopped(format!("stop gate blocked: {reason}"))
        }
        TurnOutcome::Detached => TurnEnd::Stopped("turn detached".into()),
        TurnOutcome::LoopDetected { tool, .. } => {
            TurnEnd::Stopped(format!("loop guard tripped on '{tool}'"))
        }
        TurnOutcome::Storage(e) => TurnEnd::Stopped(format!("storage error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::SessionStatus;

    #[test]
    fn footer_carries_status_fields_and_effective_effort() {
        let st = SessionStatus {
            model: Some("m1".into()),
            effort: None,
            last_prompt_tokens: 41_000,
            window: Some(128_000),
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
            compactions: 0,
        };
        let f = footer_from(&st, Some("high".into()), "proj", "s-1");
        assert_eq!(f.model.as_deref(), Some("m1"));
        assert_eq!(f.effort.as_deref(), Some("high"));
        assert_eq!(f.context, "context: 41k / 128k (32%)");
        assert_eq!(f.tokens, "tokens: 10 in / 5 out");
        assert_eq!((f.folder.as_str(), f.session.as_str()), ("proj", "s-1"));
    }
}
