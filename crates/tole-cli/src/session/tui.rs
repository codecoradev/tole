//! Host side of the TUI (#460): owns storage, registry and provider config,
//! runs one turn at a time for `tole-tui`, and composes the footer from
//! `status::session_status` (the tui crate cannot depend on this one).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tole_core::approval::{ToolRequest, Verdict};
use tole_core::cancel::CancelToken;
use tole_core::image::ImageRef;
use tole_core::openai::OpenAiConfig;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{TurnObserver, TurnOutcome};
use tole_tui::{
    ApprovalRequest, Footer, HostCmd, HostReply, Prompter, SessionItem, TuiHost, TurnEnd,
};

use crate::approver::PromptFn;
use crate::compaction_host::{CompactionSettings, ModelsCatalog, WindowSource};
use crate::session::problem::{describe_outcome, Face};
use crate::status::session_status;

/// The inline y/n prompt: the approver (allowlist, `--yes`, Destructive rules
/// stay in `InteractiveApprover`) asks through the TUI event loop.
pub struct TuiPrompt(pub Prompter);

impl PromptFn for TuiPrompt {
    fn prompt(&self, req: &ToolRequest<'_>) -> Verdict {
        // The plain-language summary (#1041), the same one `GET /approvals`
        // carries, then the tool's whole text as this prompt always showed it
        // (not cut: the structured summary is bounded, this is not); the tier
        // stays the request's own.
        let target = req.target.as_deref().map(|t| format!(" → {t}"));
        let detail = format!(
            "{} — {}{}",
            req.tool,
            req.description,
            target.unwrap_or_default()
        );
        self.0.ask(ApprovalRequest::from_summary(
            req.action_summary(),
            &detail,
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
    /// Images queued for the NEXT user message (#559); taken by `run_turn`.
    pending_images: Vec<ImageRef>,
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
    /// The workspace `/export` is jailed to (#463).
    workspace: PathBuf,
    /// Colors and the note about a refused theme (#463).
    theme: tole_tui::Theme,
    theme_note: Option<String>,
    /// Key bindings and the note about a refused keymap (#463).
    keymap: tole_tui::Keymap,
    keymap_note: Option<String>,
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
                pending_images: Vec::new(),
            }),
            registry,
            cfg,
            compaction,
            folder,
            sessions_dir: PathBuf::from(".tole/sessions"),
            fresh_prompt: None,
            index: crate::session_index::index_path(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            theme: tole_tui::Theme::default(),
            theme_note: None,
            keymap: tole_tui::Keymap::default(),
            keymap_note: None,
        }
    }

    /// The theme the UI draws with, and a note to show at start (#463).
    pub fn with_theme(mut self, theme: tole_tui::Theme, note: Option<String>) -> Self {
        self.theme = theme;
        self.theme_note = note;
        self
    }

    /// The key bindings the UI uses, and a note to show at start (#463).
    pub fn with_keymap(mut self, keymap: tole_tui::Keymap, note: Option<String>) -> Self {
        self.keymap = keymap;
        self.keymap_note = note;
        self
    }

    /// The workspace root `/export` writes under (default: the cwd).
    pub fn with_workspace(mut self, root: PathBuf) -> Self {
        self.workspace = root;
        self
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

/// Picker rows for the family of `current`: `└─ ` nesting by depth, the cut
/// entry as `(@entry)`, the open session marked `(current)`.
pub fn tree_items(nodes: Vec<crate::session_index::TreeNode>, current: &str) -> Vec<SessionItem> {
    crate::session_index::family(nodes, current)
        .into_iter()
        .map(|(depth, n)| {
            let pad = if depth == 0 {
                String::new()
            } else {
                format!("{}└─ ", "   ".repeat(depth - 1))
            };
            let at =
                n.at.as_ref()
                    .map(|a| format!(" (@{a})"))
                    .unwrap_or_default();
            let mark = if n.id == current { "  (current)" } else { "" };
            SessionItem {
                label: format!("{pad}{}{at}  {} turns{mark}", n.id, n.turns),
                id: n.id,
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
    fn theme(&self) -> tole_tui::Theme {
        self.theme.clone()
    }

    fn keymap(&self) -> tole_tui::Keymap {
        self.keymap.clone()
    }

    fn startup_note(&self) -> Option<String> {
        let notes: Vec<&str> = [&self.theme_note, &self.keymap_note]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        (!notes.is_empty()).then(|| notes.join(" | "))
    }

    fn footer(&self) -> Footer {
        let mut g = self.lock();
        let Inner {
            storage, window, ..
        } = &mut *g;
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
        // #559: images queued since the last message ride on this one.
        let images = std::mem::take(&mut g.pending_images);
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
        let before = storage.entries().len();
        let result = crate::turn_runner::run_turn_on_with_images(storage, &plan, prompt, &images);
        // The message did not reach the log (a parked turn that did not
        // settle, a provider that could not be built, ...): keep its images
        // queued for the next message instead of losing them.
        if !images.is_empty() && !records_images(&storage.entries()[before..]) {
            let later = std::mem::replace(&mut g.pending_images, images);
            g.pending_images.extend(later);
        }
        match result {
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
            HostCmd::Fork(at) => self.fork_session(at.as_deref()).map(HostReply::Session),
            HostCmd::PickTree => self.tree().map(HostReply::Sessions),
            HostCmd::Status => Ok(HostReply::Lines(self.status_lines())),
            HostCmd::Name(n) => self.set_name(&n).map(|()| HostReply::Done),
            // #560: `/image`, a pasted image path / data URL. Same resolution
            // as `--image` and chat's `/image`; the reply is the whole queue.
            HostCmd::AttachImage(spec) => self
                .attach_image(&spec)
                .map(|_| HostReply::Images(self.image_items())),
            HostCmd::ClearImages => {
                self.clear_pending_images();
                Ok(HostReply::Images(Vec::new()))
            }
            HostCmd::Images => Ok(HostReply::Images(self.image_items())),
            HostCmd::LastAnswer => Ok(HostReply::Answer(self.last_answer())),
            HostCmd::Export(path) => self.export(path.as_deref()).map(HostReply::Path),
        }
    }
}

/// Whether one of `entries` (those a turn just appended) is a user message
/// carrying images.
fn records_images(entries: &[tole_core::entry::Entry]) -> bool {
    entries.iter().any(|e| {
        e.payload["role"] == "user"
            && e.payload
                .get("images")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|a| !a.is_empty())
    })
}

fn map(o: TurnOutcome) -> TurnEnd {
    match o {
        TurnOutcome::Final { text, .. } => TurnEnd::Final(text),
        TurnOutcome::Cancelled => TurnEnd::Cancelled,
        TurnOutcome::Detached => TurnEnd::Stopped("turn detached".into()),
        other => TurnEnd::Stopped(
            describe_outcome(&other, Face::Short).unwrap_or_else(|| "turn stopped".into()),
        ),
    }
}

mod ops;
#[cfg(test)]
mod tests;
