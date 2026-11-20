//! `tole discuss` (#588, owner-only entry point, option (a) of
//! `docs/automation-design.md` "Entry point"): the owner starts, resumes and
//! reads multi-round discussions from a shell.
//!
//! The contract is typed by the owner: question, acceptance criteria,
//! participants, token budget and `max_rounds` are command-line values, never
//! model output. A discussion started here is at depth 0, its participants
//! are read-only inline voices (no tools, no process), and its budget is the
//! one the owner named. Nothing here starts a child, writes a file in the
//! workspace or widens a secret, allow pattern or profile: the host library
//! ([`crate::discussion_host`]) does the work and only the gate decides.

pub mod audit;
pub mod host;
pub mod tool;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tole_core::advisor::Budget;
use tole_core::discussion::{self, Contract, Gate};
use tole_core::discussion_log::{describe, DiscussionLog};
use tole_core::secrets::SecretSet;

use crate::config::Config;
use crate::discussion_host::{Driver, Factory, Outcome};

/// What the owner typed for a new discussion.
#[derive(Debug, Clone)]
pub struct StartArgs {
    /// `None`: a fresh id from the clock.
    pub id: Option<String>,
    pub question: String,
    pub criteria: Vec<String>,
    pub participants: Vec<String>,
    pub budget_tokens: u64,
    pub max_rounds: Option<u32>,
}

/// Where and with what a discussion runs.
pub struct Env<'a> {
    /// The ledger directory (`$CODECORA_HOME/tole/discussions`).
    pub dir: &'a Path,
    pub cfg: &'a Config,
    /// Profile persona roots: the user config's directory and `$HOME`.
    pub paths: (Option<&'a Path>, Option<&'a Path>),
    pub secrets: SecretSet,
    pub make: Factory,
}

/// A fresh discussion id from the clock (`d-<unix seconds>`).
pub fn fresh_id(now: u64) -> String {
    format!("d-{now}")
}

/// The validated contract for `args`.
pub fn contract(args: &StartArgs, now: u64, secrets: &SecretSet) -> Result<Contract, String> {
    let id = args.id.clone().unwrap_or_else(|| fresh_id(now));
    Contract::new(
        &id,
        &args.question,
        &args.criteria,
        &args.participants,
        args.budget_tokens,
        args.max_rounds,
        secrets,
    )
}

/// What the discussion may cost, before it runs: the budget is a hard cap
/// (every call is reserved in the ledger before it runs), the rounds and
/// calls are the most the contract allows, and the cost of a round grows
/// with the number of participants ([`host::round_floor`]).
pub fn cost_plan(c: &Contract) -> String {
    let n = c.participants.len();
    let per_round = n + 2;
    let floor = host::round_floor(c);
    let fit = match c.budget_tokens / floor.max(1) {
        0 => format!(
            "This budget does not fit one round of {n} participants (about {floor} tokens \
             reserved)."
        ),
        k => format!(
            "This budget covers about {} round(s) at that floor.",
            k.min(u64::from(c.max_rounds))
        ),
    };
    let q_chars = c.question.chars().count();
    format!(
        "Discussion `{}`: {} participants ({}), up to {} rounds, up to {} model calls \
         (each round: one per participant, a scribe and a claim-checker).\n\
         Question: {q_chars} characters (about {} tokens), sent with every call and counted \
         in the estimate below.\n\
         Estimate: a round reserves at least {floor} tokens ({n} participant calls and a \
         claim-checker, plus a scribe for a large round); {fit}\n\
         Budget: {} tokens, a hard cap: a call that does not fit stops the discussion \
         with an escalation.",
        c.id,
        n,
        c.participants.join(", "),
        c.max_rounds,
        per_round * c.max_rounds as usize,
        c.question.len() / 4,
        c.budget_tokens
    )
}

/// What to tell the owner (or the model) before the discussion spends
/// anything, as lines: the participant advice when there are more than
/// [`discussion::RECOMMENDED_PARTICIPANTS`], and a warning when the budget
/// cannot fit one round. Neither refuses the discussion.
pub fn advice(c: &Contract) -> Vec<String> {
    let mut out: Vec<String> = discussion::participants_advice(c.participants.len())
        .into_iter()
        .collect();
    let floor = host::round_floor(c);
    if c.budget_tokens < floor {
        out.push(format!(
            "the budget ({} tokens) does not fit one round of {} participants (about {floor} \
             reserved): the discussion will probably stop with a budget escalation in round 1",
            c.budget_tokens,
            c.participants.len()
        ));
    }
    out
}

/// [`advice`] for what the owner typed, for the terminal before a start or
/// a dry run; empty when the contract is invalid (the start reports that).
pub fn advice_for(args: &StartArgs, secrets: &SecretSet) -> Vec<String> {
    contract(args, now_secs(), secrets)
        .map(|c| advice(&c))
        .unwrap_or_default()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Dry run: validates the contract and its participants against the config
/// and returns the cost plan. Writes nothing and calls no provider.
pub fn plan(args: &StartArgs, env: &Env<'_>) -> Result<String, String> {
    let c = contract(args, now_secs(), &env.secrets)?;
    let log = DiscussionLog::at(env.dir, &c.id)?;
    check_participants(&c, env, log)?;
    Ok(format!("{}\n", cost_plan(&c)))
}

fn check_participants(c: &Contract, env: &Env<'_>, log: DiscussionLog) -> Result<(), String> {
    Driver::from_config(
        env.cfg,
        env.paths,
        &c.participants,
        log,
        env.secrets.clone(),
        Arc::clone(&env.make),
        Arc::new(Budget::new(c.budget_tokens)),
    )
    .map(|_| ())
}

/// Starts a discussion from the owner's contract and runs it to its end.
pub fn start(args: &StartArgs, env: &Env<'_>) -> Result<String, String> {
    let c = contract(args, now_secs(), &env.secrets)?;
    let plan = cost_plan(&c);
    let driver = Driver::from_config(
        env.cfg,
        env.paths,
        &c.participants,
        DiscussionLog::at(env.dir, &c.id)?,
        env.secrets.clone(),
        Arc::clone(&env.make),
        Arc::new(Budget::new(c.budget_tokens)),
    )?;
    let id = c.id.clone();
    driver.start(c)?;
    let out = driver.run().map_err(|e| resumable(&id, &env.secrets, &e))?;
    Ok(format!("{plan}\n\n{}", finish(&out, &env.secrets)))
}

/// Continues discussion `id` where its ledger stands (after a crash or a
/// failed call); a finished one just prints its outcome again.
pub fn resume(id: &str, env: &Env<'_>) -> Result<String, String> {
    let log = DiscussionLog::at(env.dir, id)?;
    let st = log.state()?;
    let c = st
        .contract
        .clone()
        .ok_or_else(|| format!("no discussion `{id}` in {}", env.dir.display()))?;
    // The pot is the discussion's own budget; the ledger counts what was
    // spent and caps every call, so a resumed discussion never gets a fresh
    // allowance.
    let pot = Arc::new(Budget::new(c.budget_tokens));
    let driver = Driver::from_config(
        env.cfg,
        env.paths,
        &c.participants,
        log,
        env.secrets.clone(),
        Arc::clone(&env.make),
        pot,
    )?;
    let out = driver.run().map_err(|e| resumable(id, &env.secrets, &e))?;
    Ok(finish(&out, &env.secrets))
}

fn resumable(id: &str, secrets: &SecretSet, e: &str) -> String {
    format!(
        "{}\nThe discussion is saved: continue it with `tole discuss resume {id}`.",
        secrets.redact(e)
    )
}

/// The outcome as the owner sees it: the host-built text and the cost.
fn finish(out: &Outcome, secrets: &SecretSet) -> String {
    let mut s = format!(
        "{}\nOutcome: {}. Rounds: {}. Tokens used: {}.\n",
        out.text,
        describe(out.gate),
        out.rounds,
        out.tokens_used
    );
    for n in &out.notes {
        s.push_str(&format!("note: {n}\n"));
    }
    if out.gate != Gate::Report {
        s.push_str("This needs the owner: read the escalation above and decide.\n");
    }
    secrets.redact(&s).into_owned()
}

/// The ledger directory, or why there is none.
pub fn dir() -> Result<PathBuf, String> {
    DiscussionLog::default_dir()
        .ok_or_else(|| "cannot resolve the tole data dir (set CODECORA_HOME or HOME)".into())
}

/// Lists the discussions (`id` `None`) or shows one: status, rounds, gate
/// decisions and tokens, never a raw contribution (the audit view).
pub fn show(
    dir: &Path,
    id: Option<&str>,
    as_json: bool,
    secrets: &SecretSet,
) -> Result<String, String> {
    if let Some(id) = id {
        if !discussion::id_ok(id) {
            return Err(format!("invalid discussion id {id:?}"));
        }
    }
    crate::audit_discussion::discussions(dir, id, as_json, secrets)
}

#[cfg(test)]
#[path = "tests.rs"]
pub(crate) mod tests;
