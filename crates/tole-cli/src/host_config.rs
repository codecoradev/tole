//! The host knobs the command fns share (`HostConfig`).

use anyhow::Result;
use std::path::PathBuf;

// ---- Wiring ----

/// Host knobs shared by the command fns: the file-tools jail root, the
/// MCP server list, the memory backend. Bundled so per-command
/// signatures stop growing with every feature.
pub(crate) struct HostConfig {
    pub(crate) workspace: Option<String>,
    /// Issue #161: explicit --skill files (loaded into the system prompt)
    pub(crate) skills: Vec<PathBuf>,
    /// Issue #161: --no-skills disables skills discovery + load_skill.
    pub(crate) no_skills: bool,
    #[cfg(feature = "mcp")]
    pub(crate) mcp_server: Vec<String>,
    /// Plan mode (issue #109): registry filtered to ReadOnly tools.
    pub(crate) plan_mode: bool,
    /// Child agents (#171): PARENT-ONLY operator mode — children spawn
    /// into per-child git worktrees. Default false; the model cannot
    /// flip this per call (storage boomerang, owner decision 2026-10-05).
    pub(crate) agents_worktree: bool,
    /// Delegation (#585): profiles and `[delegation]` for the delegate tool.
    pub(crate) delegation: tole_cli::delegate::Setup,
    /// The selected profile's name (#586): the model router's brief.
    pub(crate) profile_name: Option<String>,
    /// The run's token budget (`--max-tokens` / `mission.max_tokens`), for
    /// the router's `budget_below` (#586).
    pub(crate) max_tokens: Option<u64>,

    /// Tool-boundary hook command lines (issue #110), default empty.
    pub(crate) on_pretool: Vec<String>,
    pub(crate) on_turnend: Vec<String>,
    pub(crate) on_posttool: Vec<String>,
    /// shell-tools-only host knob. `not(feature = "shell-tools")` builds
    /// still assign `memory: None` in dispatch — the field stays so the
    /// assignments and helper signatures never fork per profile.
    #[cfg(not(feature = "shell-tools"))]
    #[allow(dead_code)] // placeholder field, never read in the bare profile (#330)
    pub(crate) memory: (),
    #[cfg(feature = "shell-tools")]
    pub(crate) memory: Option<tole_core::memory::MemoryConfig>,
    /// Context compaction between turns (#440).
    pub(crate) compaction: tole_cli::compaction_host::CompactionSettings,
}

impl HostConfig {
    /// Routes a NEW session once (#586), before its provider is built: off
    /// unless `[router] enabled = true`; a session that already has a model
    /// is left alone. A broken router fails the start with its remedy. The
    /// one-line decision goes to stderr unless `quiet` (the TUI owns the
    /// screen).
    pub(crate) fn route(
        &self,
        storage: &mut dyn tole_core::storage::Storage,
        kind: tole_core::router::TaskKind,
        prompt: &str,
        registry: &tole_core::tool::ToolRegistry,
        quiet: bool,
        budget_left: Option<u64>,
    ) -> Result<()> {
        tole_cli::session::assemble::route_session(
            storage,
            &self.delegation,
            self.profile_name.as_deref(),
            kind,
            prompt,
            registry,
            quiet,
            budget_left,
        )
    }

    pub(crate) fn on_pretool_non_empty(&self) -> bool {
        !self.on_pretool.is_empty()
    }
    pub(crate) fn on_posttool_non_empty(&self) -> bool {
        !self.on_posttool.is_empty()
    }
}

#[cfg(feature = "shell-tools")]
impl HostConfig {
    /// Pre-turn recall injection (memory loop), see
    /// [`tole_cli::turn_runner::with_recalled_memory`].
    pub(crate) fn inject_memory(&self, prompt: &str) -> String {
        tole_cli::turn_runner::with_recalled_memory(self.memory.as_ref(), prompt)
    }

    /// Post-session summary (memory loop), see
    /// [`tole_cli::turn_runner::remember`].
    pub(crate) fn remember(
        &self,
        session_id: &str,
        first_prompt: &str,
        last_answer: &str,
        wrote: bool,
    ) {
        tole_cli::turn_runner::remember(
            self.memory.as_ref(),
            session_id,
            first_prompt,
            last_answer,
            wrote,
        );
    }
}
