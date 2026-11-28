//! `tole run` and `tole resume`.

use crate::{
    build_approver, build_registry, new_session_id, report_outcome, session_path, valid_session_id,
    HostConfig,
};
use anyhow::{Context, Result};
use std::path::Path;
use tole_cli::session::assemble::{
    apply_skills, build_default_prompt, resolve_system_prompt, review_section,
};
use tole_cli::tool_roster;
use tole_core::storage::{JsonlStorage, Storage};
#[cfg(feature = "shell-tools")]
use tole_core::turn::TurnOutcome;

// ---- Commands ----

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_command(
    sessions_dir: &Path,
    prompt: &str,
    system: Option<&str>,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
    name: Option<&str>,
    timeout_secs: Option<u64>,
    images: &[String],
) -> Result<()> {
    let cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;
    // A broken router (#586) fails before the session file exists.
    tole_cli::routing::Routing::from_config(&host.delegation.cfg).map_err(anyhow::Error::msg)?;
    // Resolve (jail-check) images before the session file exists.
    let images = tole_cli::turn_runner::resolve_images(images)?;
    // Build everything that can fail BEFORE the session file exists, so a
    // failed startup does not leave a stray empty session polluting
    // `sessions` / `--last` (CodeCora scan 2026-09-18).
    let mut registry = build_registry(build_approver(allow_patterns, yes), host, allow_patterns)?;
    // Plan mode (issue #109): the guarantee is ABSENCE on the wire, not
    // approval — filtered tools never appear in specs().
    if host.plan_mode {
        registry.retain_read_only();
    }
    // Skills (issue #161): --skill files + discovery; load_skill registers ReadOnly.
    let skill_sections = apply_skills(
        &host.skills,
        host.workspace.as_ref(),
        host.no_skills,
        &mut registry,
        Some(prompt),
    )?;
    // Opt-in tool-boundary hooks (issue #110): deny-only policy
    // injection for Write/Destructive calls, default OFF.
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

    let session_id = new_session_id();
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    let system_prompt = system
        .map(str::to_string)
        .or_else(resolve_system_prompt)
        .or_else(|| Some(build_default_prompt(host.plan_mode)));
    // Skills (issue #161): skill sections append after the base prompt,
    // after the cora review flow when its tools are attached (#422).
    let review = review_section(&registry);
    let system_prompt = system_prompt.map(|p| format!("{p}{review}{skill_sections}"));
    let mut storage = tole_cli::session_host::create_session(
        sessions_dir,
        &session_id,
        system_prompt.as_deref(),
        name,
        Some("run"),
    )
    .with_context(|| format!("creating session {session_id}"))?;
    println!("session: {session_id}");
    if let Some(alias) = name {
        println!("name: {alias}");
    }
    // Task-list tools (issue #198): a fresh session starts with an empty list.
    tool_roster::register_todo(&mut registry, storage.entries(), host.plan_mode)?;
    // Model routing (#586): once, before the provider reads `fact/model`.
    host.route(
        &mut storage,
        tole_core::router::TaskKind::Run,
        prompt,
        &registry,
        false,
        host.max_tokens,
    )?;

    let turn_cfg = cfg.clone();
    let mut provider = tole_cli::turn_runner::session_provider(
        cfg,
        &storage,
        registry.specs(),
        system_prompt.as_deref(),
        None,
    )
    .map_err(anyhow::Error::msg)?;
    // Memory loop, pre-turn: recalled context rides inside the first
    // user message — the durable log stores exactly what was sent. The
    // summary remembers the PRE-injection prompt: storing the injected
    // block would echo recalled memory back into the store (amplification).
    #[cfg(feature = "shell-tools")]
    let (raw_prompt, prompt) = (prompt.to_string(), host.inject_memory(prompt));
    #[cfg(not(feature = "shell-tools"))]
    let prompt = prompt.to_string();
    // Wall-clock cap (issue #216): a timer thread cancels the turn's
    // token at the deadline; the loop's checkpoints unwind it into a
    // durable Cancelled — resumable, never dead.
    // Child agents (#380): the watcher queues sibling change notices on
    // the same token. A no-op outside a child.
    let (cancel, _sibling_watcher) = tole_cli::turn_runner::cli_turn_token();
    let timer = timeout_secs.map(|secs| {
        let token = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            token.cancel();
        })
    });
    let plan = tole_cli::turn_runner::TurnPlan::new(turn_cfg, &registry, &cancel);
    let outcome = tole_cli::turn_runner::run_turn_with_images(
        &mut storage,
        &mut provider,
        &plan,
        &prompt,
        &images,
    )
    .map_err(|e| anyhow::anyhow!(e.into_message()))?;
    // Join ONLY when the timer fired (cora MAJOR): join otherwise blocks
    // for the remaining budget after the turn already finished. Detach —
    // the process exits right after run_command anyway.
    if timer.is_some() && cancel.is_cancelled() {
        if let Some(t) = timer {
            t.join().ok();
        }
    }
    // Memory loop, post-session: a settled Final turn leaves a compact
    // summary behind for the next session's recall.
    #[cfg(feature = "shell-tools")]
    if let TurnOutcome::Final { text, wrote } = &outcome {
        host.remember(&session_id, &raw_prompt, text, *wrote);
    }
    report_outcome(&session_id, outcome);
    Ok(())
}

pub(crate) fn resume_command(
    sessions_dir: &Path,
    id: &str,
    prompt: Option<&str>,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
) -> Result<()> {
    // Resolve by NAME first (issue #216): an alias from `run --name`
    // is accepted anywhere an id is; exact ids still win when the file
    // exists directly.
    let id = if valid_session_id(id) && session_path(sessions_dir, id).exists() {
        id.to_string()
    } else {
        let mut best: Option<(std::time::SystemTime, String)> = None;
        // No sessions dir here is no local match (the index may still know
        // the id, #439); any other listing error is real.
        let listing = match std::fs::read_dir(sessions_dir) {
            Ok(rd) => Some(rd),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("listing {}", sessions_dir.display()))
                )
            }
        };
        for entry in listing.into_iter().flatten().flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = file_name.strip_suffix(".jsonl") else {
                continue;
            };
            if !valid_session_id(stem) {
                continue;
            }
            let Ok(s) = JsonlStorage::open_read_only(entry.path()) else {
                continue;
            };
            if s.session_name() == Some(id) {
                let mtime = entry.metadata().ok().and_then(|mm| mm.modified().ok());
                match &best {
                    Some((t, _)) if mtime.map(|mt| mt >= *t) != Some(true) => {}
                    _ => best = Some((mtime.unwrap_or(std::time::UNIX_EPOCH), stem.to_string())),
                }
            }
        }
        match best {
            Some((_, resolved)) => resolved,
            None => {
                // Not here: the global index (#439) knows which project the
                // session belongs to. It is NOT resumed from here: this
                // process's workspace jail, project config and trust were
                // resolved for THIS directory, and the turn would act on
                // the wrong tree. Name the exact command instead, so the
                // owning project's config and trust gate apply.
                let indexed = valid_session_id(id)
                    .then(tole_cli::session_index::index_path)
                    .flatten()
                    .and_then(|idx| tole_cli::session_index::find(&idx, id));
                if let Some(e) = indexed {
                    anyhow::bail!(
                        "session {id} belongs to another project; resume it there so its \
                         workspace, config and trust apply:\n  {}",
                        tole_cli::session_index::resume_hint(&e)
                    );
                }
                anyhow::bail!(
                    "no session with id or name {id:?} in {} (nor in the session index)",
                    sessions_dir.display()
                )
            }
        }
    };
    let path = session_path(sessions_dir, &id);
    if !path.exists() {
        anyhow::bail!("session {id} not found at {}", path.display());
    }
    let cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;
    let mut storage = JsonlStorage::open(&path).context("replaying session log")?;

    let mut registry = build_registry(build_approver(allow_patterns, yes), host, allow_patterns)?;
    // Task-list tools (issue #198): state hydrated from the replayed
    // transcript so a resumed session keeps its plan; todo_write is
    // absent under --plan-mode.
    tool_roster::register_todo(&mut registry, storage.entries(), host.plan_mode)?;
    // Plan mode (issue #109): the guarantee is ABSENCE on the wire, not
    // approval — filtered tools never appear in specs().
    if host.plan_mode {
        registry.retain_read_only();
    }
    // Skills (issue #161): --skill files + discovery; load_skill registers ReadOnly.
    let _skill_sections = apply_skills(
        &host.skills,
        host.workspace.as_ref(),
        host.no_skills,
        &mut registry,
        None,
    )?;
    // Opt-in tool-boundary hooks (issue #110): deny-only policy
    // injection for Write/Destructive calls, default OFF.
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
    // B2: the system prompt is pinned in the session header — resume
    // re-applies exactly what the session was created with (never the
    // ambient env, which may have changed since).
    let pinned = storage.system_prompt().map(str::to_string);
    let mut compactor = tole_cli::compaction_host::for_session(host.compaction, &cfg, &storage);
    let turn_cfg = cfg.clone();
    let mut provider = tole_cli::turn_runner::session_provider(
        cfg,
        &storage,
        registry.specs(),
        pinned.as_deref(),
        None,
    )
    .map_err(anyhow::Error::msg)?;
    // The session token: sibling notices (#380) reach a resumed child too.
    let (cancel, _sibling_watcher) = tole_cli::turn_runner::cli_turn_token();
    let outcome = match prompt {
        Some(text) if !text.trim().is_empty() => {
            // New instructions on a settled session (issue #55): the
            // machine accepts a user message at a turn boundary, so
            // reuse run_turn on the resumed storage instead of the
            // approvals-only resume protocol.
            // Context compaction (#440) between the settled turn and the
            // new instruction.
            compactor.before_turn(&mut storage, &mut *provider);
            let plan = tole_cli::turn_runner::TurnPlan::new(turn_cfg, &registry, &cancel);
            let outcome =
                tole_cli::turn_runner::run_turn_with(&mut storage, &mut provider, &plan, text)
                    .map_err(|e| anyhow::anyhow!(e.into_message()))?;
            // Memory loop (CodeCora scan 2026-09-18): a settled resumed
            // turn leaves a summary like `run` does — the continuation is
            // its own durable event. No recall injection here: the resumed
            // session already carries its context.
            #[cfg(feature = "shell-tools")]
            if let TurnOutcome::Final {
                text: answer,
                wrote,
            } = &outcome
            {
                host.remember(&id, text, answer, *wrote);
            }
            outcome
        }
        _ => {
            let plan = tole_cli::turn_runner::TurnPlan::new(turn_cfg, &registry, &cancel);
            tole_cli::turn_runner::resume_turn_with(&mut storage, &mut provider, &plan)
                .map_err(|e| anyhow::anyhow!(e.into_message()))?
        }
    };
    report_outcome(&id, outcome);
    Ok(())
}
