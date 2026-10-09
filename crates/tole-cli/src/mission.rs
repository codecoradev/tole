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
//! 4. the verify gate fails more than [`VERIFY_FAILURE_CAP`] times.
//!
//! Exhaustion settles RESUMABLY (durable summary + pc=Final): a mission
//! never leaves a dead session — `tole mission --resume <id>` (or a
//! plain `tole resume`) continues it. Destructive tools stay
//! un-auto-allowable; approvals (interactive) and gates apply per turn.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use tole_core::openai::OpenAiProvider;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::subprocess::run_with_timeout;
use tole_core::tool::ToolRegistry;
use tole_core::turn::{run_turn, TurnOutcome};

/// The completion marker the mission system prompt pins: a final
/// message containing this line declares the goal fully achieved.
pub const MISSION_COMPLETE_MARKER: &str = "MISSION_COMPLETE";

/// Verify-gate patience (issue #199): this many failed `--verify` runs
/// settle the mission as `verify_failed` — the model gets each
/// failure's output in context before the cap trips.
pub const VERIFY_FAILURE_CAP: u32 = 3;

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
    /// The goal, verbatim from the operator.
    pub goal: String,
    /// Total provider steps across all chained turns.
    pub max_steps: u64,
    /// Wall-clock cap in minutes.
    pub max_minutes: u64,
    /// Total token ceiling (prompt + completion, from the durable
    /// usage ledger).
    pub max_tokens: u64,
    /// Optional verification command (ground truth for completion).
    pub verify: Option<String>,
    /// Continue an interrupted mission instead of starting a new one.
    pub resume_id: Option<String>,
    /// Per-run timeout of the `--verify` command (seconds). Default 300
    /// — `cargo test`-class suites are the advertised use.
    pub verify_timeout_secs: u64,
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

/// Run the `--verify` command (sh -c, operator-budgeted timeout).
/// Returns Ok(()) on exit-0, Err(output tail) otherwise.
fn run_verify(cmd: &str, timeout: std::time::Duration) -> Result<(), String> {
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(cmd);
    let out = run_with_timeout(&mut c, timeout).map_err(|_| {
        format!(
            "verify command timed out after {}s (raise --verify-timeout)",
            timeout.as_secs()
        )
    })?;
    if out.status.success() {
        return Ok(());
    }
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let tail: String = {
        let chars: Vec<char> = text.chars().collect();
        let start = chars.len().saturating_sub(800);
        chars[start..].iter().collect()
    };
    Err(format!(
        "verify FAILED (exit {}):\n{}",
        out.status.code().unwrap_or(-1),
        tail
    ))
}

/// Snapshot the durable usage ledger (provider steps so far).
fn steps_used(storage: &JsonlStorage) -> u64 {
    storage.usages().len() as u64
}

/// Tokens (prompt + completion) recorded in the durable ledger.
fn tokens_used(storage: &JsonlStorage) -> u64 {
    storage
        .usages()
        .iter()
        .map(|u| {
            u.usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .saturating_add(
                    u.usage
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                )
        })
        .sum()
}

/// Run one mission to completion/exhaustion. The CALLER builds the
/// registry (the same dual-cfg `build_registry` path as `run`, so MCP /
/// trust / workspace flags behave identically); mission approval is the
/// operator's flags — Destructive stays structurally un-allowable.
pub fn run_mission(
    cfg: MissionConfig,
    mut registry: ToolRegistry,
    sessions_dir: &std::path::Path,
) -> Result<()> {
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(cfg.max_minutes * 60);
    // Resolve provider config BEFORE the session file exists — a config
    // failure must not leave an empty, unsummarized session (cora CI).
    let provider_cfg = tole_cli::config_apply::provider_config().context(
        "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY \
         (or the OPENAI_* equivalents)",
    )?;

    // Session: fresh (goal pinned via first prompt) or resumed.
    let (mut storage, session_id, first_prompt) = match &cfg.resume_id {
        Some(id) => {
            let path = sessions_dir.join(format!("{id}.jsonl"));
            let storage = JsonlStorage::open(&path)
                .with_context(|| format!("opening mission session {id}"))?;
            eprintln!("mission: resuming {id}");
            (storage, id.clone(), None)
        }
        None => {
            let session_id = tole_cli::session_host::new_session_id("mission");
            // The mission system prompt pins the completion-marker
            // contract and the todo discipline in the session header —
            // resume re-applies it exactly (same as run's pinned prompt).
            let system_prompt = format!(
                "{}{}",
                crate::default_prompt_for(false),
                mission_prompt(&cfg.goal, cfg.verify.as_deref())
            );
            let storage =
                JsonlStorage::create_with(sessions_dir, &session_id, None, Some(&system_prompt))
                    .with_context(|| format!("creating mission session {session_id}"))?;
            eprintln!("mission: session {session_id}");
            (storage, session_id, Some(cfg.goal.clone()))
        }
    };
    // Task-list tools (#198): hydrated from the transcript on resume.
    {
        let todo_state = tole_core::todo::TodoState::new();
        todo_state.hydrate(storage.entries());
        registry
            .register(Box::new(tole_core::todo::TodoReadTool::new(
                std::sync::Arc::clone(&todo_state),
            )))
            .map_err(|e| anyhow::anyhow!("registering todo_read: {e}"))?;
        registry
            .register(Box::new(tole_core::todo::TodoWriteTool::new(
                std::sync::Arc::clone(&todo_state),
            )))
            .map_err(|e| anyhow::anyhow!("registering todo_write: {e}"))?;
    }

    let mut provider = OpenAiProvider::new(provider_cfg).with_tool_specs(registry.specs());

    let mut turns: u64 = 0;
    let mut verify_failures: u32 = 0;
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
        // Budget checkpoints — BEFORE each turn, same discipline as the
        // cancel token's between-steps checks (#201 extends these).
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
        let Some(prompt) = continuation.take() else {
            // No continuation queued and no completion declared: a logic
            // bug upstream — settle loudly, the summary records it.
            status = "error";
            break;
        };
        let outcome = match run_turn(&mut storage, &mut provider, &registry, &prompt) {
            Ok(o) => o,
            Err(e) => {
                // Storage-class failure: still summarize (the "either
                // way" contract), then surface the error.
                eprintln!("mission: turn storage error: {e}");
                status = "error";
                break;
            }
        };
        turns += 1;
        match &outcome {
            TurnOutcome::Final { text, .. } => {
                // Line-exact per the pinned contract ("a line containing
                // exactly MISSION_COMPLETE"): a mere MENTION of the word
                // (the model restating the rules) must not declare.
                let declares = text.lines().any(|l| l.trim() == MISSION_COMPLETE_MARKER);
                match &cfg.verify {
                    // Per-turn verification (cora CI on #206): the gate
                    // is the ground truth EVERY turn, not just on marker
                    // turns — a pass without the marker keeps the mission
                    // going (the model finishes remaining work), a fail
                    // always returns the output to the model.
                    Some(cmd) => match run_verify(
                        cmd,
                        std::time::Duration::from_secs(cfg.verify_timeout_secs),
                    ) {
                        Ok(()) => {
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
                        Err(output) => {
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
            other => {
                // ApprovalRequired / BudgetExhausted(turn) / loop guard /
                // provider failure: every path settles pc=Final (#84) —
                // the mission stops and reports; the session stays
                // resumable for a later retry.
                eprintln!("mission: turn settled: {other:?}");
                status = "stopped";
                break;
            }
        }
    }

    // Durable cost report (issue #201): a fact register on the session
    // — steps, turns, tokens, wall time, and tool-call counts by risk
    // tier (folded from the durable intents; risk from the live
    // registry). `tole status` renders it.
    let wall_secs = started.elapsed().as_secs();
    let mut tool_calls = json!({"readonly": 0, "write": 0, "destructive": 0});
    use tole_core::entry::EntryType;
    for e in storage.entries() {
        if e.kind.as_str() == EntryType::INTENT {
            if let Some(tool) = e.payload.get("tool").and_then(Value::as_str) {
                let risk = registry
                    .get(tool)
                    .map(|t| t.risk())
                    .unwrap_or(tole_core::tool::Risk::ReadOnly);
                let key = match risk {
                    tole_core::tool::Risk::ReadOnly => "readonly",
                    tole_core::tool::Risk::Write => "write",
                    tole_core::tool::Risk::Destructive => "destructive",
                };
                tool_calls[key] = json!(tool_calls[key].as_u64().unwrap_or(0) + 1);
            }
        }
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
    println!("mission: {session_id}");
    println!("status: {status}");
    println!(
        "turns: {turns}  steps: {}  verify failures: {verify_failures}  wall: {wall_secs}s",
        steps_used(&storage)
    );
    // Issue #253 (rescan #25): --verify is a CI-style gate — a mission
    // that did NOT complete successfully must exit NONZERO so scripts
    // and the agent loop can branch on it. `complete` (and only a
    // verified completion) exits 0. (CI cora round on #253: the first
    // cut compared against a nonexistent "done" — every mission,
    // including successful ones, exited nonzero. Caught by the
    // mission_e2e suite.)
    if status != "complete" {
        anyhow::bail!("mission ended with status: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod budget_tests {
    use super::BudgetTier;

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
