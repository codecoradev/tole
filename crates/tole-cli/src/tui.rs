//! `tole` / `tole tui`: the terminal UI over one durable session.

use crate::{build_registry, new_session_id, session_path, valid_session_id, HostConfig};
use anyhow::{Context, Result};
use std::path::Path;
use tole_cli::approver::InteractiveApprover;
use tole_cli::session::assemble::{
    apply_skills, build_default_prompt, resolve_system_prompt, resolve_workspace_root,
    review_section,
};
use tole_cli::tool_roster;
use tole_core::storage::{JsonlStorage, Storage};

/// `tole` / `tole tui` (#460): the TUI over one ordinary durable session
/// (same JSONL as `chat`, so `tole resume` / `sessions --all` see it).
/// Memory loop, MCP prompt commands and sibling notices are not wired here yet.
#[cfg(feature = "tui")]
pub(crate) fn tui_command(
    sessions_dir: &Path,
    system: Option<&str>,
    resume: Option<String>,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
    settings: &tole_cli::config_apply::Settings,
) -> Result<()> {
    let (theme, theme_note) = settings.theme();
    let (keymap, keymap_note) = settings.keymap();
    let cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;
    if let Some(id) = &resume {
        if !valid_session_id(id) || !session_path(sessions_dir, id).exists() {
            anyhow::bail!("session {id} not found in {}", sessions_dir.display());
        }
    }
    let (prompter, approvals) = tole_tui::approval_channel();
    let approver = InteractiveApprover::new(tole_cli::tui_host::TuiPrompt(prompter))
        .with_allow_patterns(allow_patterns.to_vec())
        .with_auto_write(yes);
    let mut registry = build_registry(approver, host, allow_patterns)?;
    if host.plan_mode {
        registry.retain_read_only();
    }
    let skill_sections = apply_skills(
        &host.skills,
        host.workspace.as_ref(),
        host.no_skills,
        &mut registry,
        None,
    )?;
    // Opt-in tool-boundary hooks (issue #110): same wiring as `chat`.
    #[cfg(feature = "shell-tools")]
    if !host.on_pretool.is_empty() || !host.on_posttool.is_empty() || !host.on_turnend.is_empty() {
        let mut hooks = tole_core::hooks::ToolHooks::from_cli(&host.on_pretool, &host.on_posttool);
        hooks.turnend = host
            .on_turnend
            .iter()
            .map(|c| tole_core::hooks::turnend_hook(c))
            .collect();
        registry.set_hooks(hooks);
    }
    // The pinned prompt of a fresh session; `/new` inside the TUI reuses it.
    let prompt = system
        .map(str::to_string)
        .or_else(resolve_system_prompt)
        .unwrap_or_else(|| build_default_prompt(host.plan_mode));
    let prompt = format!("{prompt}{}{skill_sections}", review_section(&registry));
    let mut storage = match &resume {
        Some(id) => {
            JsonlStorage::open(session_path(sessions_dir, id)).context("replaying session log")?
        }
        None => {
            // A broken router (#586) fails before the session file exists.
            tole_cli::routing::Routing::from_config(&host.delegation.cfg)
                .map_err(anyhow::Error::msg)?;
            std::fs::create_dir_all(sessions_dir)
                .with_context(|| format!("creating {}", sessions_dir.display()))?;
            let id = new_session_id();
            tole_cli::session_host::create_session(
                sessions_dir,
                &id,
                Some(&prompt),
                None,
                Some("tui"),
            )
            .with_context(|| format!("creating session {id}"))?
        }
    };
    tool_roster::register_todo(&mut registry, storage.entries(), host.plan_mode)?;
    // Model routing (#586): a fresh session is routed once, quietly (the
    // decision is in its `fact/route` register).
    if resume.is_none() {
        host.route(
            &mut storage,
            tole_core::router::TaskKind::Chat,
            "",
            &registry,
            true,
            host.max_tokens,
        )?;
    }
    let session = tole_cli::tui_host::TuiSession::new(storage, registry, cfg, host.compaction)
        .with_sessions(sessions_dir.to_path_buf(), Some(prompt))
        .with_workspace(resolve_workspace_root(host.workspace.as_ref())?)
        .with_theme(theme, theme_note)
        .with_keymap(keymap, keymap_note);
    tole_tui::run(&session, approvals).context("terminal ui")?;
    let id = session.session_id();
    println!("session {id} closed (resume: tole resume {id} \"…\")");
    Ok(())
}
