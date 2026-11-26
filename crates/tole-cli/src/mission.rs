//! `tole mission` (issue #199): budgeted turn-chaining toward a goal.
//!
//! A mission is NOT a new engine — it is the existing turn machinery run
//! in a loop: turn → check termination → (budget left?) resume with a
//! continuation prompt. Every chained turn is a normal durable turn, so
//! a crash mid-mission resumes exactly where it stopped (the E5
//! guarantee) and cancel works unchanged. Termination:
//!
//! 1. the model declares completion — a final message containing
//!    [`MISSION_COMPLETE_MARKER`] (the mission system prompt pins the
//!    contract), or
//! 2. `--verify <cmd>` exits 0 (the operator's ground truth; when set,
//!    the marker alone is not enough), or
//! 3. a budget trips — `--max-steps` (provider steps across turns, from
//!    the durable usage ledger) or `--max-minutes` (wall clock), or
//! 4. the verify gate has failed [`VERIFY_FAILURE_CAP`] times in total
//!    (the count is cumulative across the mission, never reset by a pass).
//!
//! Exhaustion settles RESUMABLY (durable summary + pc=Final): a mission never
//! leaves a dead session; `tole mission --resume <id>` (or `tole resume`)
//! continues it. Destructive tools stay un-auto-allowable; approvals and
//! gates apply per turn.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use tole_core::storage::{JsonlStorage, Storage};
use tole_core::subprocess::run_with_timeout;
use tole_core::tool::ToolRegistry;
use tole_core::turn::TurnOutcome;

/// The completion marker the mission system prompt pins: a final
/// message containing this line declares the goal fully achieved.
pub const MISSION_COMPLETE_MARKER: &str = "MISSION_COMPLETE";

/// Verify-gate patience (issue #199): this many failed `--verify` runs in
/// total (cumulative, not consecutive) settle the mission as `verify_failed` — the model gets each
/// failure's output in context before the cap trips.
pub const VERIFY_FAILURE_CAP: u32 = 3;

/// Turns in a row at the per-turn step cap (`MAX_STEPS`) before the mission
/// stops as `exhausted_turn_cap` (#1239); a finished turn resets the count.
pub const TURN_CAP_TRIPS: u32 = 3;

/// Budget tier (issue #201): conservative by default; a trusted
/// ecosystem context (`--trust internal`) earns headroom, and explicit
/// flags always win over any tier default.
#[derive(Clone, Copy, Debug)]
pub struct BudgetTier {
    pub max_steps: u64,
    pub max_minutes: u64,
    pub max_tokens: u64,
}

pub const DEFAULT_TIER: BudgetTier = BudgetTier {
    max_steps: 48,
    max_minutes: 15,
    max_tokens: 200_000,
};
pub const TRUSTED_TIER: BudgetTier = BudgetTier {
    max_steps: 96,
    max_minutes: 30,
    max_tokens: 500_000,
};

impl BudgetTier {
    /// Resolve the tier: explicit flags win per-field; the trusted tier
    /// applies only where the operator gave no explicit value.
    pub fn resolve(
        trusted: bool,
        max_steps: Option<u64>,
        max_minutes: Option<u64>,
        max_tokens: Option<u64>,
    ) -> Self {
        let base = if trusted { TRUSTED_TIER } else { DEFAULT_TIER };
        Self {
            max_steps: max_steps.unwrap_or(base.max_steps),
            max_minutes: max_minutes.unwrap_or(base.max_minutes),
            max_tokens: max_tokens.unwrap_or(base.max_tokens),
        }
    }
}

/// Resolved mission parameters.
pub struct MissionConfig {
    /// The session's cancel token, shared with a parked delegated approver (#714).
    pub cancel: tole_core::cancel::CancelToken,
    /// The goal, verbatim from the operator.
    pub goal: String,
    /// Total provider steps across all chained turns.
    pub max_steps: u64,
    /// Wall-clock cap in minutes.
    pub max_minutes: u64,
    /// Total token ceiling (prompt + completion, from the usage ledger).
    pub max_tokens: u64,
    /// Optional verification command (ground truth for completion).
    pub verify: Option<String>,
    /// Continue an interrupted mission instead of starting a new one.
    pub resume_id: Option<String>,
    /// The id of a FRESH session the caller must know before the loop runs
    /// (`tole serve`); `None` mints one, as the CLI does. Ignored with `resume_id`.
    pub fresh_id: Option<String>,
    /// Per-run timeout of the `--verify` command in seconds (default 300).
    pub verify_timeout_secs: u64,
    /// Context compaction between chained turns (#440).
    pub compaction: tole_cli::compaction_host::CompactionSettings,
    /// Images attached to the first turn only (#212).
    pub images: Vec<tole_core::image::ImageRef>,
    /// The profile's persona (#584): replaces the default base prompt of a
    /// fresh mission (the mission section is still appended).
    pub persona: Option<String>,
    /// The profile's advisor (#587): `(profile, model, max uses)`. `Some`
    /// registers `consult_advisor`, whose tokens come out of `max_tokens`.
    pub advisor: Option<(String, String, u32)>,
}

/// The mission addendum appended to the default system prompt.
fn mission_prompt(goal: &str, verify: Option<&str>) -> String {
    let mut p = format!(
        "\n\n## MISSION MODE\n\
         You are running an autonomous mission toward ONE goal:\n\
         <goal>\n{goal}\n</goal>\n\n\
         Rules:\n\
         - Maintain your plan with the todo_write/todo_read tools; exactly \
         one task in_progress at a time.\n\
         - Work in small, verifiable steps; every turn is durable and \
         auditable.\n\
         - When the goal is FULLY achieved, end your final message with a \
         line containing exactly {MISSION_COMPLETE_MARKER} and nothing \
         after it. Never claim completion while work remains.\n"
    );
    if let Some(cmd) = verify {
        p.push_str(&format!(
            "- A verification command (`{cmd}`) is run after each of your \
             turns; it must exit 0 for the mission to complete. If it \
             fails, its output is returned to you — fix the cause first.\n"
        ));
    }
    p
}

/// The cora agent-as-reviewer guidance for a mission (#536): a mission gets
/// the `mcp_cora_*` tools like `run`, but its pinned prompt never named the
/// review flow. Present only when both review tools are registered. Guidance,
/// not enforcement: a gate before `git commit` is a separate change.
fn mission_review_section(registry: &ToolRegistry) -> String {
    let review = crate::review_section(registry);
    if review.is_empty() {
        return String::new();
    }
    format!(
        "{review} Before you end the mission, review the change you made with this \
         flow, address what it blocks, and include the verdict in your final message."
    )
}

/// The system prompt pinned in the header of a FRESH mission session: the
/// default prompt, the mission addendum, then the review flow when attached.
fn fresh_mission_prompt(
    goal: &str,
    verify: Option<&str>,
    registry: &ToolRegistry,
    persona: Option<&str>,
) -> String {
    format!(
        "{}{}{}",
        persona.map_or_else(|| crate::default_prompt_for(false), str::to_string),
        mission_prompt(goal, verify),
        mission_review_section(registry)
    )
}

/// Upper bound for `--max-minutes` (10 years): a huge value must neither
/// wrap `minutes * 60` nor overflow the `Instant` arithmetic.
const MAX_MINUTES_CAP: u64 = 60 * 24 * 365 * 10;

fn max_minutes_duration(minutes: u64) -> std::time::Duration {
    std::time::Duration::from_secs(minutes.min(MAX_MINUTES_CAP) * 60)
}

/// A `--resume` id names a file inside the sessions dir; it must be a plain
/// file stem (the shape `new_session_id` produces), never a path.
fn validate_session_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        anyhow::bail!(
            "invalid mission session id {id:?}: expected letters, digits, '-' and '_' only"
        )
    }
}

/// Failure to run the verify command to completion, with the real cause
/// (spawn failure, wait error or timeout), not always "timed out".
fn verify_run_error(cause: &str, timeout: std::time::Duration) -> String {
    format!(
        "verify command did not complete: {cause} (--verify-timeout is {}s)",
        timeout.as_secs()
    )
}

/// Run the `--verify` command (sh -c, operator-budgeted timeout) and say what
/// it came to: passed (exit 0), failed (output tail for the model) or refused.
///
/// The command is the owner's, but what it runs is usually code the agent
/// just wrote (`cargo test` runs build scripts and tests), with no
/// approval: it gets the ENV-1 scrub as its own consumer, which receives no
/// secret unless a `[secrets]` entry grants one to `verify` (#676).
///
/// With `[sandbox.consumers] verify` on (#748) the same `sh -c` runs in the
/// `command` profile instead. A mission is unattended, so `optional` counts as
/// `required`, and a required but missing sandbox is a [`Verify::Refused`],
/// never a pass. So is one that fails to start the command (the launcher
/// failing is not the command failing; the model must not be told to fix it):
/// a launcher spawn error, or a non-zero exit with no exit code of the command.
fn run_verify(cmd: &str, timeout: std::time::Duration) -> Verify {
    use tole_core::sandbox::command::{command_in_cwd, Consumer, Launched};
    let Launched { cmd: mut c, report } =
        match command_in_cwd(Consumer::Verify, "sh", &["-c".to_string(), cmd.into()]) {
            Ok(l) => l,
            Err(why) => return Verify::Refused(why),
        };
    tole_core::subprocess::scrub_env_for(&mut c, tole_core::secret_release::VERIFY);
    let out = match run_with_timeout(&mut c, timeout) {
        Ok(o) => o,
        Err(e) if report.is_some() && e.starts_with(tole_core::subprocess::SPAWN_FAILED) => {
            return Verify::Refused(format!("verify: not run, the sandbox did not start ({e})"))
        }
        Err(e) => return Verify::Failed(verify_run_error(&e, timeout)),
    };
    if out.status.success() {
        return Verify::Passed;
    }
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let tail: String = {
        let chars: Vec<char> = text.chars().collect();
        let start = chars.len().saturating_sub(800);
        chars[start..].iter().collect()
    };
    if let (Some(report), Some(code)) = (report, out.status.code()) {
        if report.command_exit_code() != Some(code) {
            return Verify::Refused(format!(
                "verify: not run, the sandbox failed before or while starting the command \
                 (bwrap exit {code}): {}",
                tail.trim()
            ));
        }
    }
    Verify::Failed(format!(
        "verify FAILED (exit {}):\n{}",
        out.status.code().unwrap_or(-1),
        tail
    ))
}

/// What running the `--verify` command came to.
#[derive(Debug, PartialEq, Eq)]
enum Verify {
    /// Exit 0.
    Passed,
    /// Ran and failed, or did not complete: the tail goes back to the model.
    Failed(String),
    /// Not run: the owner's required sandbox is unavailable (#748). The model
    /// cannot fix that: never retried, never a pass (`verify_refused`).
    Refused(String),
}

/// `tole-core`'s poll budget (120 polls in one turn) also settles as
/// `BudgetExhausted`; it is a wait, not a step cap, and its error entry says so.
fn poll_budget_spent(s: &JsonlStorage) -> bool {
    let detail = s
        .entries()
        .last()
        .and_then(|e| e.payload["detail"].as_str());
    detail.is_some_and(|d| d.starts_with("poll budget"))
}

/// Snapshot the durable usage ledger (provider steps so far).
fn steps_used(storage: &JsonlStorage) -> u64 {
    // Wire-only rows (#211: request sizes without provider usage) are not
    // provider steps — budget semantics are exactly as before.
    tole_core::transcript::provider_steps(storage.usages())
}

/// Tokens (prompt + completion) recorded in the durable ledger.
fn tokens_used(storage: &JsonlStorage) -> u64 {
    tole_core::transcript::tokens_in_out(storage.usages())
}

/// Run one mission to completion/exhaustion. The CALLER builds the registry
/// (the `build_registry` path of `run`, so MCP / trust / workspace flags
/// behave identically); approval is the operator's flags, Destructive stays
/// structurally un-allowable.
pub fn run_mission(
    cfg: MissionConfig,
    registry: ToolRegistry,
    sessions_dir: &std::path::Path,
    // Model routing (#586) of a fresh mission, before its provider exists.
    route: &dyn Fn(&mut dyn tole_core::storage::Storage, &ToolRegistry) -> Result<()>,
) -> Result<()> {
    let done = run_mission_outcome(cfg, registry, sessions_dir, route)?;
    println!("mission: {}", done.session_id);
    println!("status: {}", done.status);
    println!(
        "turns: {}  steps: {}  verify failures: {}  wall: {}s",
        done.turns, done.steps, done.verify_failures, done.wall_secs
    );
    // Issue #253: --verify is a CI-style gate, so a mission that did NOT
    // complete exits NONZERO and only `complete` exits 0.
    if done.status != "complete" {
        anyhow::bail!("mission ended with status: {}", done.status);
    }
    Ok(())
}

/// How a mission ended: what [`run_mission_outcome`] hands back instead of
/// printing and exiting, so a face other than the CLI (`tole serve`) can
/// report it. `summary` is the `fact/mission` register that was written.
#[derive(Debug, Clone)]
// `tokens` and `summary` are read by `tole serve`, which needs `shell-tools`.
#[cfg_attr(not(feature = "shell-tools"), allow(dead_code))]
pub struct MissionOutcome {
    pub session_id: String,
    pub status: &'static str,
    pub turns: u64,
    pub steps: u64,
    pub tokens: u64,
    pub verify_failures: u32,
    pub wall_secs: u64,
    pub summary: Value,
}

/// [`run_mission`] without the console report or the nonzero exit: runs the
/// loop, writes the durable summary and returns how it ended. An `Err` is a
/// failure before or while writing the session; a mission that did not
/// complete is an `Ok` outcome with its `status`.
pub fn run_mission_outcome(
    cfg: MissionConfig,
    mut registry: ToolRegistry,
    sessions_dir: &std::path::Path,
    route: &dyn Fn(&mut dyn tole_core::storage::Storage, &ToolRegistry) -> Result<()>,
) -> Result<MissionOutcome> {
    let started = std::time::Instant::now();
    let deadline = started + max_minutes_duration(cfg.max_minutes);
    // Resolve provider config BEFORE the session file exists: a config
    // failure must not leave an empty, unsummarized session.
    let provider_cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;

    // Session: fresh (goal pinned via first prompt) or resumed.
    let (mut storage, session_id, first_prompt) = match &cfg.resume_id {
        Some(id) => {
            validate_session_id(id)?;
            let path = sessions_dir.join(format!("{id}.jsonl"));
            let storage = JsonlStorage::open(&path)
                .with_context(|| format!("opening mission session {id}"))?;
            eprintln!("mission: resuming {id}");
            (storage, id.clone(), None)
        }
        None => {
            let session_id = match &cfg.fresh_id {
                Some(id) => {
                    validate_session_id(id)?;
                    id.clone()
                }
                None => tole_cli::session_host::new_session_id("mission"),
            };
            // The mission system prompt pins the completion-marker
            // contract and the todo discipline in the session header —
            // resume re-applies it exactly (same as run's pinned prompt).
            let system_prompt = fresh_mission_prompt(
                &cfg.goal,
                cfg.verify.as_deref(),
                &registry,
                cfg.persona.as_deref(),
            );
            let storage = tole_cli::session_host::create_session(
                sessions_dir,
                &session_id,
                Some(&system_prompt),
                None,
                Some("mission"),
            )
            .with_context(|| format!("creating mission session {session_id}"))?;
            eprintln!("mission: session {session_id}");
            (storage, session_id, Some(cfg.goal.clone()))
        }
    };
    // Task-list tools (#198), hydrated from the transcript on resume (a
    // mission refuses --plan-mode, so todo_write is always present).
    tole_cli::tool_roster::register_todo(&mut registry, storage.entries(), false)?;
    // A fresh mission is routed once; a resumed one keeps its model.
    if cfg.resume_id.is_none() {
        route(&mut storage, &registry)?;
    }

    // The advisor (#587): its tokens come out of this mission's budget,
    // which starts at what the session already spent (a resume included).
    let advisor = match &cfg.advisor {
        Some((name, model, max_uses)) => {
            let budget = std::sync::Arc::new(tole_core::advisor::Budget::new(cfg.max_tokens));
            budget.charge(tokens_used(&storage));
            let shared = tole_cli::advisor_host::Shared::new(budget);
            let secrets =
                tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
            let tool = tole_cli::advisor_host::ConsultAdvisor::new(
                std::sync::Arc::clone(&shared),
                *max_uses,
                format!("Profile: {name}\nMission goal: {}", cfg.goal),
                secrets.clone(),
                tole_cli::advisor_host::factory(provider_cfg.clone(), model, secrets),
            );
            registry
                .register(Box::new(tool))
                .map_err(anyhow::Error::msg)?;
            Some(shared)
        }
        None => None,
    };

    let system_prompt = storage.system_prompt().map(str::to_owned);
    let mut compactor =
        tole_cli::compaction_host::for_session(cfg.compaction, &provider_cfg, &storage);
    let turn_cfg = provider_cfg.clone();
    let mut provider = tole_cli::turn_runner::session_provider(
        provider_cfg,
        &storage,
        registry.specs(),
        system_prompt.as_deref(),
        None,
    )
    .map_err(anyhow::Error::msg)?;
    if let Some(shared) = &advisor {
        provider = Box::new(tole_cli::advisor_host::Tap::new(
            provider,
            std::sync::Arc::clone(shared),
            true,
        ));
    }

    let mut turns: u64 = 0;
    let mut verify_failures: u32 = 0;
    let mut cap_trips: u32 = 0;
    let mut continuation: Option<String> = match &first_prompt {
        Some(goal) => Some(format!(
            "MISSION GOAL: {goal}\n\nBegin the mission. Maintain the plan \
             with todo_write; end your final message with \
             {MISSION_COMPLETE_MARKER} only when the goal is fully achieved."
        )),
        None => Some(
            "Mission continues (interrupted). Re-read the plan with \
             todo_read and continue toward the goal."
                .to_string(),
        ),
    };

    let status;
    loop {
        // Budget checkpoints BEFORE each turn, like the cancel checks (#201).
        let used = steps_used(&storage);
        if used >= cfg.max_steps {
            eprintln!(
                "mission: step budget exhausted ({used}/{}) after {turns} turn(s)",
                cfg.max_steps
            );
            status = "exhausted_steps";
            break;
        }
        let tokens = tokens_used(&storage);
        if tokens >= cfg.max_tokens {
            eprintln!(
                "mission: token budget exhausted ({tokens}/{}) after {turns} turn(s)",
                cfg.max_tokens
            );
            status = "exhausted_tokens";
            break;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "mission: time budget exhausted ({} min) after {turns} turn(s)",
                cfg.max_minutes
            );
            status = "exhausted_time";
            break;
        }
        if cap_trips >= TURN_CAP_TRIPS {
            status = "exhausted_turn_cap";
            break;
        }
        // A verify the sandbox cannot run: stop before a turn is spent on it.
        if turns == 0 && cfg.verify.is_some() {
            if let Err(why) = tole_core::sandbox::command::preflight(
                tole_core::sandbox::command::Consumer::Verify,
            ) {
                eprintln!("mission: verify refused before the first turn: {why}");
                status = "verify_refused";
                break;
            }
        }
        let Some(prompt) = continuation.take() else {
            // No continuation queued and no completion declared: a logic
            // bug upstream — settle loudly, the summary records it.
            status = "error";
            break;
        };
        // Context compaction (#440) between turns; its tokens count too.
        compactor.before_turn(&mut storage, &mut *provider);
        let first_images = if turns == 0 {
            cfg.images.as_slice()
        } else {
            &[]
        };
        let plan = tole_cli::turn_runner::TurnPlan::new(turn_cfg.clone(), &registry, &cfg.cancel);
        let outcome = match tole_cli::turn_runner::run_turn_with_images(
            &mut storage,
            &mut provider,
            &plan,
            &prompt,
            first_images,
        ) {
            Ok(o) => o,
            Err(e) => {
                // Storage-class failure: still summarize (the "either
                // way" contract), then surface the error.
                eprintln!("mission: turn storage error: {}", e.into_message());
                status = "error";
                break;
            }
        };
        turns += 1;
        // The advisor's usage becomes session records: budgets and a resume count it.
        if let Some(shared) = &advisor {
            if let Err(e) = tole_cli::advisor_host::record_usage(shared, &mut storage) {
                eprintln!("mission: {e}");
                status = "error";
                break;
            }
        }
        match &outcome {
            TurnOutcome::Final { text, .. } => {
                cap_trips = 0;
                // Line-exact per the pinned contract ("a line containing
                // exactly MISSION_COMPLETE"): a mere MENTION of the word
                // (the model restating the rules) must not declare.
                let declares = text.lines().any(|l| l.trim() == MISSION_COMPLETE_MARKER);
                match &cfg.verify {
                    // Per-turn verification (#206): the gate is the ground
                    // truth EVERY turn — a pass without the marker keeps the
                    // mission going, a fail returns the output to the model.
                    Some(cmd) => match run_verify(
                        cmd,
                        std::time::Duration::from_secs(cfg.verify_timeout_secs),
                    ) {
                        Verify::Refused(why) => {
                            // Fail closed: no ground truth, so no completion
                            // and no turn spent on a problem the model
                            // cannot fix. The session stays resumable.
                            eprintln!("mission: verify refused: {why}");
                            status = "verify_refused";
                            break;
                        }
                        Verify::Passed => {
                            if declares {
                                status = "complete";
                                break;
                            }
                            continuation = Some(format!(
                                "Verification passed, but you have not declared \
                                 {MISSION_COMPLETE_MARKER} — finish any remaining \
                                 work toward the goal, then declare it."
                            ));
                        }
                        Verify::Failed(output) => {
                            verify_failures += 1;
                            eprintln!(
                                "mission: verify failed ({verify_failures}/{VERIFY_FAILURE_CAP})"
                            );
                            if verify_failures >= VERIFY_FAILURE_CAP {
                                status = "verify_failed";
                                break;
                            }
                            let claim = if declares {
                                format!(
                                    "You declared {MISSION_COMPLETE_MARKER} but \
                                     verification disagrees. "
                                )
                            } else {
                                String::new()
                            };
                            continuation = Some(format!(
                                "{claim}Verification FAILED:\n{output}\n\n\
                                 Fix the cause and continue."
                            ));
                        }
                    },
                    None => {
                        if declares {
                            status = "complete";
                            break;
                        }
                        let used_now = steps_used(&storage);
                        continuation = Some(format!(
                            "Mission continues. Steps used: {used_now}/{}; \
                             time left: {} min. Keep working the plan.",
                            cfg.max_steps,
                            cfg.max_minutes
                                .saturating_sub(started.elapsed().as_secs() / 60)
                        ));
                    }
                }
            }
            TurnOutcome::BudgetExhausted if !poll_budget_spent(&storage) => {
                // The per-turn step cap (#1239); the budgets are checked above.
                cap_trips += 1;
                let cap = tole_core::turn::MAX_STEPS;
                eprintln!("mission: turn hit the {cap}-step cap ({cap_trips}/{TURN_CAP_TRIPS})");
                continuation = Some(
                    "Your last turn used its whole step limit without finishing. Stop \
                     gathering information and produce the deliverable now, then verify \
                     it if a verify gate exists."
                        .into(),
                );
            }
            other => {
                // ApprovalRequired / loop guard / provider failure: every
                // path settles pc=Final (#84) — the mission stops and reports;
                // the session stays resumable for a later retry.
                eprintln!("mission: turn settled: {other:?}");
                status = "stopped";
                break;
            }
        }
    }

    // Durable cost report (issue #201): a session fact register of steps,
    // turns, tokens, wall time and tool-call counts by risk tier (from the
    // durable intents, risk from the live registry); `tole status` renders it.
    let wall_secs = started.elapsed().as_secs();
    let mut tool_calls = json!({"readonly": 0, "write": 0, "destructive": 0});
    use tole_core::{entry::EntryType, tool::Risk};
    let intents = storage
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == EntryType::INTENT);
    for tool in intents.filter_map(|e| e.payload.get("tool").and_then(Value::as_str)) {
        let key = match registry.get(tool).map_or(Risk::ReadOnly, |t| t.risk()) {
            Risk::ReadOnly => "readonly",
            Risk::Write => "write",
            Risk::Destructive => "destructive",
        };
        tool_calls[key] = json!(tool_calls[key].as_u64().unwrap_or(0) + 1);
    }
    let summary = json!({
        "goal": cfg.goal,
        "status": status,
        "turns": turns,
        "steps": steps_used(&storage),
        "tokens_in_out": tokens_used(&storage),
        // Issue #211: reported only — never counted against any budget.
        // `null` when no step reported cached tokens (unknown, not 0).
        "cached_tokens": tole_cli::usage_report::usage_report(storage.usages())
            .to_json()["cached_tokens"],
        "verify_failures": verify_failures,
        "wall_seconds": wall_secs,
        "budget": {"max_steps": cfg.max_steps, "max_minutes": cfg.max_minutes,
                   "max_tokens": cfg.max_tokens},
        "tool_calls": tool_calls,
    });
    {
        use tole_core::register::RegisterWrite;
        storage
            .commit(
                tole_core::storage::Commit::new().register(RegisterWrite::set(
                    "fact",
                    "mission",
                    summary.clone(),
                )),
            )
            .map_err(|e| anyhow::anyhow!("writing mission summary: {e}"))?;
    }
    // How the mission ended, for the skill curator (#593).
    tole_cli::skill_outcome::record_mission(status);
    Ok(MissionOutcome {
        session_id,
        status,
        turns,
        steps: steps_used(&storage),
        tokens: tokens_used(&storage),
        verify_failures,
        wall_secs,
        summary,
    })
}

#[cfg(test)]
mod budget_tests {
    use super::{steps_used, tokens_used, BudgetTier};
    use serde_json::{json, Value};
    use tole_core::entry::Entry;
    use tole_core::provider::{Provider, ProviderError, ProviderOutput};
    use tole_core::storage::{JsonlStorage, Storage};
    use tole_core::tool::ToolRegistry;
    use tole_core::turn::run_turn;

    /// Reports request sizes (#211) and, optionally, provider usage.
    struct Wired {
        usage: Option<Value>,
    }
    impl Provider for Wired {
        fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
            Ok(ProviderOutput::Final { text: "ok".into() })
        }
        fn last_usage(&self) -> Option<Value> {
            self.usage.clone()
        }
        fn last_wire_stats(&self) -> Option<Value> {
            Some(json!({"system_chars": 1, "tools_chars": 2, "history_chars": 3, "messages": 1}))
        }
    }

    fn store(tag: &str) -> JsonlStorage {
        let d = crate::test_dir::TestDir::new(&format!("tole-budget-{tag}"));
        JsonlStorage::create(&d, "b", None).unwrap()
    }

    /// #211 invariant: a gateway that reports NO usage leaves wire-only
    /// ledger rows, which must not count as steps or tokens — the step
    /// and token budgets behave exactly as before.
    #[test]
    fn wire_only_rows_do_not_move_step_or_token_budgets() {
        let mut s = store("wire-only");
        let reg = ToolRegistry::new();
        let mut p = Wired { usage: None };
        for t in ["a", "b", "c"] {
            run_turn(&mut s, &mut p, &reg, t).unwrap();
        }
        assert_eq!(s.usages().len(), 3, "rows are stored (PR 2 needs them)");
        assert_eq!(steps_used(&s), 0);
        assert_eq!(tokens_used(&s), 0);
    }

    /// Mixed ledger: only rows with provider usage are steps.
    #[test]
    fn mixed_ledger_counts_only_provider_usage_steps() {
        let mut s = store("mixed");
        let reg = ToolRegistry::new();
        run_turn(&mut s, &mut Wired { usage: None }, &reg, "a").unwrap();
        let mut with = Wired {
            usage: Some(json!({"prompt_tokens": 10, "completion_tokens": 4})),
        };
        run_turn(&mut s, &mut with, &reg, "b").unwrap();
        run_turn(&mut s, &mut Wired { usage: None }, &reg, "c").unwrap();
        run_turn(&mut s, &mut with, &reg, "d").unwrap();
        assert_eq!(s.usages().len(), 4);
        assert_eq!(steps_used(&s), 2);
        assert_eq!(tokens_used(&s), 28);
    }

    /// Issue #201: explicit flags win per-field; the trusted tier only
    /// fills where the operator gave nothing.
    #[test]
    fn tier_resolution_explicit_wins_and_trusted_fills() {
        let d = BudgetTier::resolve(false, None, None, None);
        assert_eq!(
            (d.max_steps, d.max_minutes, d.max_tokens),
            (48, 15, 200_000)
        );
        let t = BudgetTier::resolve(true, None, None, None);
        assert_eq!(
            (t.max_steps, t.max_minutes, t.max_tokens),
            (96, 30, 500_000)
        );
        // Explicit values override BOTH tiers, per-field.
        let mixed = BudgetTier::resolve(true, Some(10), None, Some(1_000));
        assert_eq!(
            (mixed.max_steps, mixed.max_minutes, mixed.max_tokens),
            (10, 30, 1_000)
        );
    }
}

#[cfg(test)]
mod verify_error_tests {
    /// #536: the mission prompt names the cora review flow exactly when both
    /// review tools are registered, and keeps the completion contract.
    #[test]
    fn fresh_mission_prompt_names_the_review_flow_only_when_the_tools_are_attached() {
        use tole_core::tool::{Risk, Tool};
        struct Named(&'static str);
        impl Tool for Named {
            fn name(&self) -> &str {
                self.0
            }
            fn risk(&self) -> Risk {
                Risk::ReadOnly
            }
            fn execute(&self, _input: serde_json::Value) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
        }
        let mut reg =
            ToolRegistry::with_approver(tole_core::approval::AllowlistApprover::allow_only(vec![]));
        let none = fresh_mission_prompt("ship it", Some("cargo test"), &reg, None);
        assert!(!none.contains("Code review"), "{none}");
        assert!(none.contains(MISSION_COMPLETE_MARKER), "{none}");

        reg.register(Box::new(Named("mcp_cora_cora_review_prepare")))
            .unwrap();
        let half = fresh_mission_prompt("ship it", None, &reg, None);
        assert!(!half.contains("Code review"), "finalize missing: {half}");

        reg.register(Box::new(Named("mcp_cora_cora_review_finalize")))
            .unwrap();
        let both = fresh_mission_prompt("ship it", Some("cargo test"), &reg, None);
        assert!(both.contains("mcp_cora_cora_review_prepare"), "{both}");
        assert!(both.contains("should_block"), "{both}");
        assert!(both.contains("review the change you made"), "{both}");
        // The review text comes AFTER the contract, so it cannot displace it.
        assert!(both.contains("ship it") && both.contains(MISSION_COMPLETE_MARKER));
        assert!(both.rfind("# Code review").unwrap() > both.find("MISSION MODE").unwrap());

        // #584: a persona replaces the default base prompt; the contract follows.
        let p = fresh_mission_prompt("ship it", None, &reg, Some("You are the coder."));
        assert!(p.starts_with("You are the coder."), "{p}");
        assert!(!p.contains(&crate::default_prompt_for(false)));
        assert!(p.contains("MISSION MODE") && p.contains(MISSION_COMPLETE_MARKER));
    }

    use super::*;
    use std::time::Duration;

    #[test]
    fn a_spawn_failure_is_not_reported_as_a_timeout() {
        let e = verify_run_error(
            "failed to spawn subprocess (is it on PATH?): No such file or directory",
            Duration::from_secs(300),
        );
        assert!(e.contains("failed to spawn subprocess"), "{e}");
        assert!(!e.contains("timed out"), "{e}");
        assert!(e.contains("--verify-timeout is 300s"), "{e}");
    }

    /// cora 0.19 scan: the mission prompt pinned in the session header must
    /// reach the wire, on a fresh run and after reopening on `--resume`.
    #[test]
    fn mission_provider_sends_the_pinned_system_prompt_on_resume() {
        use tole_core::openai::{OpenAiConfig, OpenAiProvider};
        let d = crate::test_dir::TestDir::new("tole-mission-sys");
        let sys = format!("base{}", mission_prompt("ship it", None));
        let cfg = || OpenAiConfig::new("https://x/v1", "m", "k");
        // One handle at a time: a writable open holds the session lock, so
        // drop it before the next iteration reopens the file (#488).
        for reopen in [false, true] {
            let body = {
                let storage = if reopen {
                    JsonlStorage::open(d.join("m1.jsonl")).unwrap()
                } else {
                    JsonlStorage::create_with(&d, "m1", None, Some(&sys)).unwrap()
                };
                tole_cli::turn_runner::configure_provider(
                    OpenAiProvider::new(cfg()),
                    vec![],
                    storage.system_prompt(),
                    None,
                )
                .request_body(&[])
            };
            let first = &body["messages"][0];
            assert_eq!(first["role"], json!("system"), "{body}");
            assert!(
                first["content"]
                    .as_str()
                    .unwrap()
                    .contains(MISSION_COMPLETE_MARKER),
                "{body}"
            );
        }
    }

    #[test]
    fn resume_id_must_be_a_plain_session_stem() {
        let id = tole_cli::session_host::new_session_id("mission");
        assert!(validate_session_id(&id).is_ok(), "{id}");
        for bad in ["", "../x", "a/b", "a\\b", "..", "x.jsonl", "/etc/passwd"] {
            assert!(validate_session_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn huge_max_minutes_is_capped_not_wrapped() {
        let d = max_minutes_duration(u64::MAX);
        assert_eq!(d.as_secs(), MAX_MINUTES_CAP * 60);
        // Instant arithmetic must not overflow either.
        let _ = std::time::Instant::now() + d;
        assert_eq!(max_minutes_duration(2).as_secs(), 120);
    }

    #[test]
    fn a_real_timeout_still_says_so() {
        let Verify::Failed(e) = run_verify("sleep 5", Duration::from_secs(1)) else {
            panic!("a timeout is a failure the model sees, not a pass or a refusal");
        };
        assert!(e.contains("timed out after 1s"), "{e}");
    }

    #[test]
    fn exit_status_decides_pass_or_fail_with_the_output_tail() {
        let t = Duration::from_secs(20);
        assert_eq!(run_verify("exit 0", t), Verify::Passed);
        let Verify::Failed(e) = run_verify("echo boom; exit 3", t) else {
            panic!("a non-zero exit is a failure");
        };
        assert!(
            e.starts_with("verify FAILED (exit 3):") && e.contains("boom"),
            "{e}"
        );
    }
}
