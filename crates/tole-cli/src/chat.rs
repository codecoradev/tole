//! `tole chat`: the line REPL.

use crate::{
    build_approver, build_registry, latest_session_id, new_session_id, session_path,
    valid_session_id, HostConfig,
};
use anyhow::{Context, Result};
use std::path::Path;
use tole_cli::session::assemble::{
    apply_skills, build_default_prompt, resolve_system_prompt, review_section,
};
use tole_cli::tool_roster;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::turn::TurnOutcome;

// ---- Chat (B1) ----

/// The B1 REPL: one durable session, many turns. Every user line becomes a
/// turn; mid-flight states (denial, provider failure) are resolved by
/// `resume_turn` on the next line, keeping the conversation alive without
/// losing durable context. Ctrl-C / EOF exit cleanly — every commit is
/// already durable, `tole chat --resume <id>` picks the thread back up.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_command(
    sessions_dir: &Path,
    system: Option<&str>,
    resume: Option<String>,
    last: bool,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
    // `--image`: attached to the first message (#212); `/image` queues more
    // for any later message (#559).
    mut pending_images: Vec<tole_core::image::ImageRef>,
) -> Result<()> {
    use std::io::{BufRead, Write};

    let cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;

    // Resolve the session: explicit id, --last, or fresh.
    let (session_id, fresh) = if let Some(id) = resume {
        if !valid_session_id(&id) {
            anyhow::bail!("invalid session id {id:?} (allowed: [a-z0-9-], max 64)");
        }
        let path = session_path(sessions_dir, &id);
        if !path.exists() {
            anyhow::bail!("session {id} not found at {}", path.display());
        }
        (id, false)
    } else if last {
        let Some(id) = latest_session_id(sessions_dir) else {
            anyhow::bail!("no sessions found in {}", sessions_dir.display());
        };
        (id, false)
    } else {
        (new_session_id(), true)
    };

    // Build everything that can fail BEFORE the fresh session file is
    // created, so a failed startup does not leave a stray empty session
    // polluting `sessions` / `--last` (CodeCora scan 2026-09-18).
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

    if fresh {
        // A broken router (#586) fails before the session file exists.
        tole_cli::routing::Routing::from_config(&host.delegation.cfg)
            .map_err(anyhow::Error::msg)?;
        std::fs::create_dir_all(sessions_dir)
            .with_context(|| format!("creating {}", sessions_dir.display()))?;
    }
    let path = session_path(sessions_dir, &session_id);
    let mut storage = if fresh {
        let system_prompt = system
            .map(str::to_string)
            .or_else(resolve_system_prompt)
            .or_else(|| Some(build_default_prompt(host.plan_mode)));
        // Skills (issue #161): sections append after the base prompt,
        // after the cora review flow when its tools are attached (#422).
        let review = review_section(&registry);
        let system_prompt = system_prompt.map(|p| format!("{p}{review}{skill_sections}"));
        tole_cli::session_host::create_session(
            sessions_dir,
            &session_id,
            system_prompt.as_deref(),
            None,
            Some("chat"),
        )
        .with_context(|| format!("creating session {session_id}"))?
    } else {
        JsonlStorage::open(&path).context("replaying session log")?
    };
    // Task-list tools (issues #198/#276): per-session state hydrated from
    // the replayed transcript so a resumed chat keeps its plan.
    {
        use tole_core::storage::Storage;
        tool_roster::register_todo(&mut registry, storage.entries(), host.plan_mode)?;
    }
    // Model routing (#586): a fresh chat is routed once; a resumed one
    // keeps its model.
    if fresh {
        host.route(
            &mut storage,
            tole_core::router::TaskKind::Chat,
            "",
            &registry,
            false,
            host.max_tokens,
        )?;
    }
    println!(
        "tole chat — session {session_id} (Ctrl-D exits, /image <path> attaches to the next message, resume: tole chat --resume {session_id})"
    );

    // B2: fresh sessions pin the resolved prompt; resumed sessions re-apply
    // the header-pinned one (see create_with above / JsonlStorage::open).
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
    // Server prompts (#213): probe-first, silent when no server offers any.
    #[cfg(feature = "mcp")]
    {
        let cmds: Vec<String> = host
            .mcp_server
            .iter()
            .filter_map(|spec| tole_core::mcp::McpServerConfig::parse(spec).ok())
            // #680: only a server this session admitted offers prompts.
            .filter(|cfg| registry.mcp_admitted(&cfg.name))
            .flat_map(|cfg| {
                tole_core::mcp::probe_server_extras(&cfg)
                    .prompts
                    .into_iter()
                    .map(move |p| format!("/mcp:{}:{}", cfg.name, p.name))
            })
            .collect();
        if !cmds.is_empty() {
            println!("mcp prompts: {}", cmds.join(" "));
        }
    }
    // One token for the whole chat session: sibling notices (#380) reach
    // a child running `tole chat` at each turn's next safe point.
    let (cancel, _sibling_watcher) = tole_cli::turn_runner::cli_turn_token();

    // Memory loop state: recall rides into the first message of a FRESH
    // session only (a resumed session already carries its context); the
    // session summary is stored when the REPL exits cleanly.
    #[cfg(feature = "shell-tools")]
    let (mut memory_injected, mut first_prompt, mut last_answer) = (!fresh, None, None);
    // Issue #143: did any turn of this chat execute a Write/Destructive tool?
    #[cfg(feature = "shell-tools")]
    let mut chat_wrote = false;
    // Set when the typed message could not run because the session was
    // stuck mid-flight and the bounded resolve retries ran out — the
    // message is NOT in the durable log, so the operator must resend it.
    // SCOPED PER MESSAGE (cora scan-3 #8): declared inside the loop —
    // an outer flag never reset, so one drop warned forever after.
    let stdin = std::io::stdin();
    loop {
        let mut dropped_message = false;
        print!("you>");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                break; // EOF: clean exit, session stays durable
            }
            Ok(_) => {}
            Err(e) => anyhow::bail!("reading stdin: {e}"),
        }
        let text = line.trim();
        // `/mcp:<server>:<prompt> [k=v ...]` (#213): the server renders the
        // prompt; the result is shown, then sent as this turn's user
        // message. Server-controlled text — same trust tier as a tool
        // description; tool calls it provokes still hit the approval gate.
        #[cfg(feature = "mcp")]
        let rendered: String;
        #[cfg(feature = "mcp")]
        let text = match tole_core::mcp::parse_prompt_command(text) {
            None => text,
            Some(parsed) => {
                let admitted = parsed.and_then(|(s, p, a)| {
                    if registry.mcp_admitted(&s) {
                        Ok((s, p, a))
                    } else {
                        Err(format!(
                            "mcp server {s:?} is not admitted by the plugin lock: it offers no prompts"
                        ))
                    }
                });
                match admitted.and_then(|(s, p, a)| {
                    tole_core::mcp::get_server_prompt_in(
                        &s,
                        host.workspace.as_deref().map(Path::new),
                        &p,
                        a,
                    )
                }) {
                    Ok(t) if !t.is_empty() => {
                        println!("tole> (mcp prompt)\n{t}");
                        rendered = t;
                        rendered.as_str()
                    }
                    Ok(_) => {
                        eprintln!("tole> mcp prompt rendered no text");
                        continue;
                    }
                    Err(e) => {
                        eprintln!("tole> {e}");
                        continue;
                    }
                }
            }
        };
        // Bound auto-resolve attempts per message so a wedged session
        // can't spin forever (each retry is a full provider round-trip).
        let mut retries_left: u8 = 2;
        match text {
            "" => continue,
            "/exit" | "/quit" => break,
            "/status" => {
                println!(
                    "pc: {:?}  seq: {}  entries: {}",
                    storage.state().pc,
                    storage.state().seq,
                    storage.entries().len()
                );
                continue;
            }
            // #559: `/image <path|data: URL>` queues an image for the NEXT
            // message (same jail, cap and sniffing as `--image`); a bare
            // `/image` lists the queue, `/image clear` empties it.
            t if t == "/image" || t.starts_with("/image ") => {
                match t["/image".len()..].trim() {
                    "" => {
                        println!(
                            "tole> {} image(s) queued for your next message",
                            pending_images.len()
                        );
                        for r in &pending_images {
                            println!("  {} ({}, {} bytes)", r.path, r.mime, r.bytes);
                        }
                    }
                    "clear" => {
                        let n = pending_images.len();
                        pending_images.clear();
                        println!("tole> {n} queued image(s) dropped");
                    }
                    spec => match tole_cli::turn_runner::resolve_image(spec) {
                        Ok(r) => {
                            println!(
                                "tole> image attached to your next message: {} ({}, {} bytes)",
                                r.path, r.mime, r.bytes
                            );
                            pending_images.push(r);
                        }
                        Err(e) => eprintln!("tole> /image: {e}"),
                    },
                }
                continue;
            }
            _ => {}
        }

        // Dispatch by durable state: a boundary (Idle/Final) starts a new
        // turn; anything mid-flight is resolved via resume FIRST (looping
        // until it lands on a boundary), and only then does the freshly
        // typed message run as its own turn — user input is never dropped.
        //
        // Memory loop, pre-turn: the first fresh-session message carries
        // the recalled-context block; the durable log records exactly
        // what the provider sees.
        #[cfg(feature = "shell-tools")]
        if first_prompt.is_none() {
            first_prompt = Some(text.to_string());
        }
        #[cfg(feature = "shell-tools")]
        let turn_prompt: String = if fresh && !memory_injected {
            memory_injected = true;
            host.inject_memory(text)
        } else {
            text.to_string()
        };
        #[cfg(not(feature = "shell-tools"))]
        let turn_prompt = text.to_string();
        let turn_images = std::mem::take(&mut pending_images);
        let outcome = loop {
            match storage.state().pc {
                tole_core::state::Pc::Idle | tole_core::state::Pc::Final => {
                    // Context compaction (#440): only here, between
                    // settled turns.
                    compactor.before_turn(&mut storage, &mut *provider);
                    let plan =
                        tole_cli::turn_runner::TurnPlan::new(turn_cfg.clone(), &registry, &cancel);
                    break tole_cli::turn_runner::run_turn_with_images(
                        &mut storage,
                        &mut provider,
                        &plan,
                        &turn_prompt,
                        &turn_images,
                    )
                    .map_err(tole_cli::turn_runner::TurnRunError::into_message);
                }
                _ => {
                    eprintln!("tole> (resolving the interrupted turn…)");
                    let plan =
                        tole_cli::turn_runner::TurnPlan::new(turn_cfg.clone(), &registry, &cancel);
                    match tole_cli::turn_runner::resume_turn_with(
                        &mut storage,
                        &mut provider,
                        &plan,
                    )
                    .map_err(tole_cli::turn_runner::TurnRunError::into_message)
                    {
                        // Landed on a boundary — dispatch the message now.
                        Ok(TurnOutcome::Final { .. }) => continue,
                        // Still stuck (re-denied, provider still down):
                        // surface it; the NEXT user message retries the
                        // resolve. The current message is preserved in
                        // this loop and will run once the session clears.
                        Ok(
                            other @ (TurnOutcome::ApprovalRequired { .. }
                            | TurnOutcome::ProviderFailed { .. }),
                        ) => {
                            if retries_left == 0 {
                                dropped_message = true;
                                break Ok(other);
                            }
                            retries_left -= 1;
                            continue;
                        }
                        Ok(other) => {
                            // UnknownTool / BudgetExhausted / LoopDetected
                            // / Storage: the typed message never reached
                            // the durable log (run_turn was never
                            // reached). Flag it so the operator gets the
                            // not-recorded note below (cora full-scan
                            // #7 — silently continuing would let them
                            // believe the input was recorded).
                            dropped_message = true;
                            break Ok(other);
                        }
                        Err(e) => {
                            // Storage-level failure resolving: do not lose
                            // the user's message — report and keep the
                            // input buffered for the next attempt.
                            dropped_message = true;
                            break Err(e);
                        }
                    }
                }
            }
        };

        match outcome {
            Ok(TurnOutcome::Final { text, wrote }) => {
                #[cfg(feature = "shell-tools")]
                {
                    last_answer = Some(text.clone());
                    chat_wrote |= wrote;
                }
                #[cfg(not(feature = "shell-tools"))]
                let _ = wrote;
                println!("tole> {text}");
            }
            Ok(TurnOutcome::ApprovalRequired { name }) => eprintln!(
                "tole> (approval denied for '{name}' — turn aborted; your next message resumes)"
            ),
            Ok(TurnOutcome::UnknownTool { name }) => {
                eprintln!("tole> (unknown tool '{name}' — recorded; your next message continues)")
            }
            Ok(TurnOutcome::ProviderFailed { message }) => {
                eprintln!(
                    "tole> (provider failed: {message}{}; next message retries via resume)",
                    tole_cli::compaction_host::overflow_hint(&message)
                )
            }
            Ok(TurnOutcome::BudgetExhausted) => {
                eprintln!("tole> (step budget exhausted — turn aborted; next message resumes)")
            }
            Ok(TurnOutcome::StopGateBlocked { reason }) => {
                eprintln!(
                    "tole> (stop gate blocked: {reason} — turn aborted; next message resumes)"
                )
            }
            Ok(TurnOutcome::Cancelled) => {
                eprintln!("tole> (turn cancelled — next message resumes)")
            }
            Ok(TurnOutcome::Detached) => {
                eprintln!("tole> (turn detached — next message resumes)")
            }
            Ok(TurnOutcome::LoopDetected { .. }) => eprintln!(
                "tole> (loop guard tripped — identical tool calls repeated; next message resumes)"
            ),
            Ok(TurnOutcome::Storage(e)) => anyhow::bail!("storage error: {e}"),
            Err(e) => {
                // Resolve-path failure: the typed message was NOT
                // recorded. Print the same not-recorded note the
                // dropped_message path uses, then surface the error
                // (previously this bailed silently on the note).
                eprintln!("tole> (note: the message you just typed was NOT recorded — resolve the session state, then resend it)");
                anyhow::bail!("turn failed: {e}");
            }
        }
        if dropped_message {
            // The typed message never reached the durable log — saying
            // "your next message resumes" alone would let the operator
            // believe it was recorded (CodeCora scan 2026-09-18).
            eprintln!("tole> (note: the message you just typed was NOT recorded — resolve the session state, then resend it)");
            // Its images were not recorded either: keep them queued for
            // the resend (#559).
            if !turn_images.is_empty() {
                eprintln!(
                    "tole> ({} image(s) stay queued for your next message)",
                    turn_images.len()
                );
                pending_images.splice(0..0, turn_images);
            }
        }
        // #143 (cora): a writing turn that ABORTED (provider failure,
        // loop guard, budget) still wrote to disk — the durable
        // session-scoped register survives the abort, so fold it in here
        // too, not only on the Final arm.
        #[cfg(feature = "shell-tools")]
        if storage
            .get_register("fact", "wrote_this_turn")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            chat_wrote = true;
        }
    }
    // Memory loop, post-session: a clean REPL exit with at least one
    // completed turn leaves a compact summary in the namespace.
    #[cfg(feature = "shell-tools")]
    if let (Some(fp), Some(la)) = (first_prompt.as_deref(), last_answer.as_deref()) {
        host.remember(&session_id, fp, la, chat_wrote);
    }
    println!(
        "session {session_id} closed — entries: {}",
        storage.entries().len()
    );
    Ok(())
}
