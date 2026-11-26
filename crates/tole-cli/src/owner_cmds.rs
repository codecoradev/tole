//! The owner-only commands `tole delegation approvals` and `tole discuss`.

use crate::cli_actions::{ApprovalsAction, DelegationAction, DiscussAction};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// `tole discuss` (#588): the owner-only entry point.
/// `tole delegation approvals ...` (#714): the owner answers a delegated
/// child's request. `allow` and `deny` refuse under an agent session first.
pub(crate) fn delegation_command(action: DelegationAction, root: &Path) -> Result<()> {
    use tole_cli::approvals_owner as ao;
    let DelegationAction::Approvals { action } = action;
    let data = tole_core::paths::tole_data_dir()
        .ok_or_else(|| anyhow!("cannot resolve the tole data dir (set CODECORA_HOME or HOME)"))?;
    let log = tole_cli::delegate::log_path(&data, root)
        .ok_or_else(|| anyhow!("cannot canonicalize the workspace {}", root.display()))?;
    let store = tole_cli::delegated_approver::Store::new(log);
    let secrets = tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
    let ctx = ao::Ctx {
        store: &store,
        secrets: &secrets,
        now: tole_cli::delegated_approver::real_now(),
        via: tole_core::delegation_approvals::Via::Cli,
    };
    let env = &tole_cli::config_apply::real_env;
    let allow = tole_core::delegation_approvals::Choice::Allow;
    let deny = tole_core::delegation_approvals::Choice::Deny;
    let out = match action {
        ApprovalsAction::List { all } => ao::list(&ctx, all),
        ApprovalsAction::Show { id } => ao::show(&ctx, &id),
        ApprovalsAction::Deny { id } => ao::decide(&ctx, env, &id, deny, None, ao::Confirm::Yes),
        ApprovalsAction::Allow {
            id,
            yes,
            fingerprint,
        } => {
            let mut ask = |text: &str| {
                use std::io::{BufRead, Write};
                print!("{text}\ntype the request id to allow it: ");
                // A failed flush shows up as an empty answer: not allowed.
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                let _ = std::io::stdin().lock().read_line(&mut line);
                line
            };
            let confirm = if yes {
                ao::Confirm::Yes
            } else {
                ao::Confirm::Typed(&mut ask)
            };
            ao::decide(&ctx, env, &id, allow, fingerprint.as_deref(), confirm)
        }
    }
    .map_err(anyhow::Error::msg)?;
    print!("{out}");
    Ok(())
}

pub(crate) fn discuss_command(
    action: DiscussAction,
    cfg: Option<&tole_cli::config::Config>,
    servers: &[String],
    delegation: &tole_cli::delegate::Setup,
) -> Result<()> {
    use tole_cli::discuss;
    let dir = discuss::dir().map_err(anyhow::Error::msg)?;
    let secrets = tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
    let out = match action {
        DiscussAction::List { json } => {
            discuss::show(&dir, None, json, &secrets).map_err(anyhow::Error::msg)?
        }
        DiscussAction::Show { id, json } => {
            discuss::show(&dir, Some(&id), json, &secrets).map_err(anyhow::Error::msg)?
        }
        action => {
            // Starting or resuming spends the owner's budget and writes a
            // discussion ledger: owner-only like `cron create`, `plugin
            // approve` and `config trust` (#684, #813). A dry run calls no
            // model and writes nothing, so it stays available.
            if !matches!(action, DiscussAction::Start { dry_run: true, .. }) {
                tole_cli::owner_only::refuse_under_session("tole discuss start|resume")
                    .map_err(anyhow::Error::msg)?;
            }
            let default = tole_cli::config::Config::default();
            let cfg = cfg.unwrap_or(&default);
            let base = tole_cli::config::user_config_path()
                .and_then(|p| p.parent().map(Path::to_path_buf));
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let dry = matches!(action, DiscussAction::Start { dry_run: true, .. });
            // Background files are named against the folder the owner runs
            // `tole discuss` in.
            let workdir = std::env::current_dir()?;
            // A dry run calls no model, so it needs no provider config.
            let make = if dry {
                tole_cli::discussion_host::factory(
                    tole_core::openai::OpenAiConfig::new("http://127.0.0.1:1/v1", "dry", "dry"),
                    secrets.clone(),
                    tole_cli::config_apply::discussion_timeout(),
                )
            } else {
                let pc = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;
                tole_cli::discussion_host::factory(
                    pc,
                    secrets.clone(),
                    tole_cli::config_apply::discussion_timeout(),
                )
            };
            // The host that starts helper children, for a discussion that
            // allows tools (#1039): one root goal in this folder, no allow
            // patterns of its own (a helper's authority is nothing but the
            // owner's allowance).
            let delegator = std::env::current_exe().ok().and_then(|bin| {
                tole_cli::delegate::Delegator::for_session(
                    bin,
                    workdir.clone(),
                    false,
                    Vec::new(),
                    delegation.clone(),
                    &tole_cli::config_apply::real_env,
                )
            });
            let env = discuss::Env {
                dir: &dir,
                cfg,
                paths: (base.as_deref(), home.as_deref()),
                secrets,
                make,
                workdir: &workdir,
                delegator: delegator.map(std::sync::Arc::new),
            };
            match action {
                DiscussAction::Start {
                    question,
                    criteria,
                    participants,
                    budget,
                    max_rounds,
                    id,
                    files,
                    tools,
                    dry_run,
                } => {
                    let args = discuss::StartArgs {
                        id,
                        question,
                        criteria,
                        participants,
                        budget_tokens: budget,
                        max_rounds,
                        files,
                        tools: {
                            let names = discuss::helper_tools::resolve(&tools, servers)
                                .map_err(anyhow::Error::msg)?;
                            // The exact registry name each spelling allows.
                            for (spec, name) in tools.iter().zip(&names) {
                                eprintln!("tole: helper tool {spec} is allowed as {name}");
                            }
                            names
                        },
                    };
                    for line in discuss::advice_for(&args, env.workdir, &env.secrets) {
                        eprintln!("tole: advice: {line}");
                    }
                    if dry_run {
                        discuss::plan(&args, &env)
                    } else {
                        discuss::start(&args, &env)
                    }
                }
                DiscussAction::Resume { id } => discuss::resume(&id, &env),
                _ => unreachable!("list and show are handled above"),
            }
            .map_err(anyhow::Error::msg)?
        }
    };
    print!("{out}");
    Ok(())
}
