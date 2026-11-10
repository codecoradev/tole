//! Host side of the TUI (#460): owns storage, registry and provider config,
//! runs one turn at a time for `tole-tui`, and composes the footer from
//! `status::session_status` (the tui crate cannot depend on this one).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tole_core::approval::{ToolRequest, Verdict};
use tole_core::cancel::CancelToken;
use tole_core::openai::OpenAiConfig;
use tole_core::storage::{is_valid_session_id, JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{TurnObserver, TurnOutcome};
use tole_tui::{
    ApprovalRequest, Footer, HostCmd, HostReply, Prompter, SessionItem, TuiHost, TurnEnd,
};

use crate::approver::PromptFn;
use crate::compaction_host::{CompactionSettings, ModelsCatalog, WindowSource};
use crate::status::session_status;

/// The inline y/n prompt: the approver (allowlist, `--yes`, Destructive rules
/// stay in `InteractiveApprover`) asks through the TUI event loop.
pub struct TuiPrompt(pub Prompter);

impl PromptFn for TuiPrompt {
    fn prompt(&self, req: &ToolRequest<'_>) -> Verdict {
        let target = req.target.as_deref().map(|t| format!(" → {t}"));
        self.0.ask(ApprovalRequest::new(
            format!(
                "{} — {}{}",
                req.tool,
                req.description,
                target.unwrap_or_default()
            ),
            req.risk,
        ))
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
    compaction: CompactionSettings,
    folder: String,
    /// `/new` creates sessions here, with this pinned system prompt.
    sessions_dir: PathBuf,
    fresh_prompt: Option<String>,
    /// The global session index (`None` = no index: nothing recorded or looked up).
    index: Option<PathBuf>,
}

impl TuiSession {
    pub fn new(
        storage: JsonlStorage,
        registry: ToolRegistry,
        cfg: OpenAiConfig,
        compaction: CompactionSettings,
    ) -> Self {
        let folder = std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| ".".into());
        Self {
            inner: Mutex::new(Inner {
                storage,
                window: None,
            }),
            registry,
            cfg,
            compaction,
            folder,
            sessions_dir: PathBuf::from(".tole/sessions"),
            fresh_prompt: None,
            index: crate::session_index::index_path(),
        }
    }

    /// Replace the session index (tests pass `None` to stay off the user's).
    pub fn with_index(mut self, index: Option<PathBuf>) -> Self {
        self.index = index;
        self
    }

    /// Where `/new` creates sessions and the system prompt it pins.
    pub fn with_sessions(mut self, dir: PathBuf, fresh_prompt: Option<String>) -> Self {
        self.sessions_dir = dir;
        self.fresh_prompt = fresh_prompt;
        self
    }

    /// The id of the session turns currently run against (it changes on
    /// `/new` and `/resume`).
    pub fn session_id(&self) -> String {
        self.lock().storage.session_id().to_string()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Swap the session in place. The new handle is opened BEFORE the old
    /// one is dropped, and never for the id already open (that would
    /// self-lock, #475): one open handle per session at all times.
    fn swap(&self, storage: JsonlStorage) -> String {
        let id = storage.session_id().to_string();
        let mut g = self.lock();
        g.storage = storage;
        g.window = None;
        id
    }
}

/// Sessions for the `/resume` picker: the global index (same source as
/// `sessions --all`), newest first, without the open one. One page of 50.
pub fn sessions_from(index: &std::path::Path, current: &str) -> Vec<SessionItem> {
    crate::session_index::rows(index, None)
        .into_iter()
        .filter(|r| r.entry.id != current)
        .take(50)
        .map(|r| {
            let what = r.name.clone().unwrap_or_else(|| r.preview.clone());
            SessionItem {
                label: format!("{}  {what}  ({} turns)", r.entry.id, r.turns),
                id: r.entry.id,
            }
        })
        .collect()
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
        let mut g = self.lock();
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
        footer_from(&status, effort, &self.folder, storage.session_id())
    }

    fn run_turn(
        &self,
        prompt: &str,
        cancel: &CancelToken,
        observer: Arc<dyn TurnObserver>,
    ) -> TurnEnd {
        let mut g = self.lock();
        let storage = &mut g.storage;
        let system_prompt = storage.system_prompt().map(str::to_string);
        // The TUI's differences from the server faces are explicit in the
        // plan: settle a turn parked mid-flight (provider failure, ...) first,
        // and compact when due. The turn itself is the shared runner (#525).
        let plan = crate::turn_runner::TurnPlan {
            cfg: self.cfg.clone(),
            registry: &self.registry,
            system_prompt: system_prompt.as_deref(),
            cancel,
            observer: Some(&observer),
            resolve_parked: true,
            compaction: Some(self.compaction),
        };
        match crate::turn_runner::run_turn_on(storage, &plan, prompt) {
            Ok(o) => map(o),
            Err(crate::turn_runner::TurnRunError::Provider(e)) => TurnEnd::Stopped(e),
            Err(crate::turn_runner::TurnRunError::Storage(e)) => {
                TurnEnd::Stopped(format!("storage error: {e}"))
            }
        }
    }

    /// The TUI's single host call for slash commands and hotkeys (#526): each
    /// command is dispatched to the session operation below, and the reply is
    /// data only (the wording is the UI's).
    fn apply(&self, cmd: HostCmd) -> Result<HostReply, String> {
        match cmd {
            HostCmd::PickModel => self.models().map(HostReply::Models),
            HostCmd::SetModel(id) => self.set_model(&id).map(|()| HostReply::Done),
            HostCmd::SetEffort(level) => {
                self.set_effort(level.as_deref()).map(|()| HostReply::Done)
            }
            // `/compact` instructions are not supported yet (the UI says so).
            HostCmd::Compact(_) => self.request_compaction().map(|()| HostReply::Done),
            HostCmd::New => self.new_session().map(HostReply::Session),
            HostCmd::PickSession => self.sessions().map(HostReply::Sessions),
            HostCmd::Switch(id) => self.switch_session(&id).map(HostReply::Session),
            HostCmd::Status => Ok(HostReply::Lines(self.status_lines())),
            HostCmd::Name(n) => self.set_name(&n).map(|()| HostReply::Done),
        }
    }
}

/// The session operations behind [`TuiHost::apply`]: plain methods, so the
/// session can be driven (and tested) without going through a command.
impl TuiSession {
    pub fn models(&self) -> Result<Vec<String>, String> {
        tole_core::openai::fetch_model_ids(&self.cfg.base_url, &self.cfg.api_key)
    }

    pub fn set_model(&self, id: &str) -> Result<(), String> {
        let mut g = self.lock();
        crate::session_settings::set_model(&mut g.storage, id)?;
        // the context window belongs to the model: look it up again
        g.window = None;
        Ok(())
    }

    pub fn set_effort(&self, level: Option<&str>) -> Result<(), String> {
        crate::session_settings::set_effort(&mut self.lock().storage, level)
    }

    /// The forced pass of #481: the next prompt compacts first (when
    /// compaction is enabled). Not a mid-turn compaction.
    pub fn request_compaction(&self) -> Result<(), String> {
        crate::session_settings::request_compaction(&mut self.lock().storage)
    }

    pub fn new_session(&self) -> Result<String, String> {
        std::fs::create_dir_all(&self.sessions_dir).map_err(|e| e.to_string())?;
        let id = crate::session_host::new_session_id("s");
        let s = crate::session_host::create_session(
            &self.sessions_dir,
            &id,
            self.fresh_prompt.as_deref(),
            None,
            self.index.as_ref().map(|_| "tui"),
        )
        .map_err(|e| format!("creating session {id}: {e}"))?;
        Ok(self.swap(s))
    }

    pub fn sessions(&self) -> Result<Vec<SessionItem>, String> {
        let index = self.index.as_deref().ok_or("no session index")?;
        Ok(sessions_from(index, &self.session_id()))
    }

    pub fn switch_session(&self, id: &str) -> Result<String, String> {
        if !is_valid_session_id(id) {
            return Err(format!("invalid session id {id:?}"));
        }
        if self.session_id() == id {
            return Ok(id.to_string());
        }
        let file = self
            .index
            .as_deref()
            .and_then(|i| crate::session_index::find(i, id))
            .map(|e| e.file)
            .unwrap_or_else(|| self.sessions_dir.join(format!("{id}.jsonl")));
        if !file.is_file() {
            return Err(format!("session {id} not found"));
        }
        let s = JsonlStorage::open(&file).map_err(|e| format!("opening {id}: {e}"))?;
        Ok(self.swap(s))
    }

    pub fn status_lines(&self) -> Vec<String> {
        let mut g = self.lock();
        let Inner { storage, window } = &mut *g;
        let model = crate::turn_runner::session_model(&*storage, &self.cfg.model);
        let mut src = CachedWindow {
            catalog: ModelsCatalog::new(&self.cfg, &model),
            slot: window,
        };
        let mut out = vec![format!("session: {}", storage.session_id())];
        out.extend(
            session_status(&*storage, Some(&self.cfg.model), &self.compaction, &mut src).lines(),
        );
        out
    }

    pub fn set_name(&self, name: &str) -> Result<(), String> {
        crate::session_settings::set_name(&mut self.lock().storage, name)
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

    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("tole-tuihost-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn session(dir: &std::path::Path, id: &str) -> TuiSession {
        let storage = JsonlStorage::create_with(dir, id, None, Some("sys")).unwrap();
        let cfg = OpenAiConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "m0".into(),
            api_key: "k".into(),
        };
        TuiSession::new(
            storage,
            ToolRegistry::new(),
            cfg,
            CompactionSettings::default(),
        )
        .with_sessions(dir.to_path_buf(), Some("sys".into()))
        .with_index(None)
    }

    #[test]
    fn switches_write_the_registers_the_next_call_and_resume_read() {
        let dir = tmp("regs");
        let s = session(&dir, "s-a");
        assert_eq!(s.footer().model.as_deref(), Some("m0"));
        s.set_model("m2").unwrap();
        s.set_effort(Some("low")).unwrap();
        // the very next footer refresh reflects both
        let f = s.footer();
        assert_eq!(
            (f.model.as_deref(), f.effort.as_deref()),
            (Some("m2"), Some("low"))
        );
        assert!(s.set_effort(Some("loud")).is_err());
        assert!(s.set_model("bad\x1b[2Jid").is_err());
        assert!(s.set_model("  ").is_err());
        drop(s);
        // a resumed process sees both (what `tole resume` builds the provider from)
        let r = JsonlStorage::open(dir.join("s-a.jsonl")).unwrap();
        assert_eq!(crate::turn_runner::session_model(&r, "m0"), "m2");
        assert_eq!(
            crate::turn_runner::session_effort(&r, None).as_deref(),
            Some("low")
        );
        drop(r);
        let s = session_reopen(&dir, "s-a");
        s.set_effort(None).unwrap();
        assert_eq!(s.footer().effort, None, "unset falls back to the default");
    }

    fn session_reopen(dir: &std::path::Path, id: &str) -> TuiSession {
        let storage = JsonlStorage::open(dir.join(format!("{id}.jsonl"))).unwrap();
        let cfg = OpenAiConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "m0".into(),
            api_key: "k".into(),
        };
        TuiSession::new(
            storage,
            ToolRegistry::new(),
            cfg,
            CompactionSettings::default(),
        )
        .with_sessions(dir.to_path_buf(), Some("sys".into()))
        .with_index(None)
    }

    #[test]
    fn name_and_compact_request_are_durable_registers() {
        let dir = tmp("name");
        let s = session(&dir, "s-n");
        s.set_name("  nightly run ").unwrap();
        assert!(s.set_name("a\x1bb").is_err());
        s.request_compaction().unwrap();
        drop(s);
        let r = JsonlStorage::open(dir.join("s-n.jsonl")).unwrap();
        assert_eq!(r.session_name(), Some("nightly run"));
        assert!(tole_core::compaction::overflow_pending(&r));
    }

    #[test]
    fn new_and_switch_keep_one_open_handle_per_session() {
        let dir = tmp("swap");
        let s = session(&dir, "s-1");
        let two = s.new_session().unwrap();
        assert_ne!(two, "s-1");
        assert_eq!(s.footer().session, two);
        // s-1 is closed: a second handle opens without `Locked`
        drop(JsonlStorage::open(dir.join("s-1.jsonl")).expect("old handle released"));
        // switching back releases `two` and re-opens s-1
        assert_eq!(s.switch_session("s-1").unwrap(), "s-1");
        drop(JsonlStorage::open(dir.join(format!("{two}.jsonl"))).expect("released"));
        // the same session again is a no-op, not a self-lock
        assert_eq!(s.switch_session("s-1").unwrap(), "s-1");
        assert!(s.switch_session("s-missing").is_err());
        assert!(s.switch_session("../x").is_err());
        assert_eq!(
            s.footer().session,
            "s-1",
            "a failed switch keeps the session"
        );
    }

    /// The single host call the UI makes (#526): every command reaches the
    /// session operation it names, with a data-only reply.
    #[test]
    fn apply_dispatches_each_command_to_its_session_operation() {
        let dir = tmp("apply");
        let s = session(&dir, "s-1");

        assert_eq!(s.apply(HostCmd::SetModel("m2".into())), Ok(HostReply::Done));
        assert_eq!(
            s.apply(HostCmd::SetEffort(Some("low".into()))),
            Ok(HostReply::Done)
        );
        assert_eq!(
            s.apply(HostCmd::Name("nightly".into())),
            Ok(HostReply::Done)
        );
        assert_eq!(s.apply(HostCmd::Compact(None)), Ok(HostReply::Done));
        // what each one wrote, read back through the session's own readers
        let footer = s.footer();
        assert_eq!(footer.model.as_deref(), Some("m2"));
        assert_eq!(footer.effort.as_deref(), Some("low"));
        {
            let g = s.lock();
            assert_eq!(g.storage.session_name(), Some("nightly"));
            assert!(tole_core::compaction::overflow_pending(&g.storage));
        }
        // `/compact <instructions>` is accepted and the instructions ignored.
        assert_eq!(
            s.apply(HostCmd::Compact(Some("keep tests".into()))),
            Ok(HostReply::Done)
        );

        // clearing the effort goes back to the process default
        assert_eq!(s.apply(HostCmd::SetEffort(None)), Ok(HostReply::Done));

        // bad input is refused with the operation's own error, nothing replied
        assert!(s.apply(HostCmd::SetModel("bad\x1b[2Jid".into())).is_err());
        assert!(s.apply(HostCmd::SetEffort(Some("loud".into()))).is_err());
        assert!(s.apply(HostCmd::Name("a\x1bb".into())).is_err());

        // sessions: New and Switch answer with the session now in use
        let Ok(HostReply::Session(two)) = s.apply(HostCmd::New) else {
            panic!("New answers with the new session id");
        };
        assert_ne!(two, "s-1");
        assert_eq!(s.session_id(), two);
        assert_eq!(
            s.apply(HostCmd::Switch("s-1".into())),
            Ok(HostReply::Session("s-1".into()))
        );
        assert!(s.apply(HostCmd::Switch("s-missing".into())).is_err());
        assert_eq!(s.session_id(), "s-1", "a failed switch keeps the session");

        // status is lines; the picker needs the index, which this session was
        // built without
        let Ok(HostReply::Lines(lines)) = s.apply(HostCmd::Status) else {
            panic!("Status answers with lines");
        };
        assert!(lines.iter().any(|l| l == "session: s-1"), "{lines:?}");
        assert_eq!(
            s.apply(HostCmd::PickSession),
            Err("no session index".to_string())
        );
    }

    #[test]
    fn a_locked_target_is_refused_and_the_current_session_stays() {
        let dir = tmp("lock");
        let s = session(&dir, "s-1");
        let held = JsonlStorage::create_with(&dir, "s-2", None, Some("x")).unwrap();
        assert!(s.switch_session("s-2").is_err());
        assert_eq!(s.footer().session, "s-1");
        drop(held);
    }

    #[test]
    fn session_rows_skip_the_current_one_and_label_names() {
        let dir = tmp("rows");
        let index = dir.join("index.jsonl");
        for id in ["s-1", "s-2"] {
            let st = JsonlStorage::create_with(&dir, id, None, Some("x")).unwrap();
            drop(st);
            crate::session_index::append(
                &index,
                &crate::session_index::Entry {
                    id: id.into(),
                    file: dir.join(format!("{id}.jsonl")),
                    cwd: dir.clone(),
                    face: "tui".into(),
                    created_at: 1,
                },
            )
            .unwrap();
        }
        let rows = sessions_from(&index, "s-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "s-2");
    }

    #[test]
    fn status_lines_reuse_the_session_status_snapshot() {
        let dir = tmp("status");
        let s = session(&dir, "s-st");
        s.set_effort(Some("high")).unwrap();
        let lines = s.status_lines();
        assert!(lines.iter().any(|l| l == "effort:  high"), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("model:")));
    }
}
