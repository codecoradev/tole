//! `tole discuss` (#588, owner-only entry point, option (a) of
//! `docs/automation-design.md` "Entry point"): the owner starts, resumes and
//! reads multi-round discussions from a shell.
//!
//! The contract is typed by the owner: question, acceptance criteria,
//! participants, token budget and `max_rounds` are command-line values, never
//! model output. A discussion started here is at depth 0, its participants
//! are read-only inline voices (no tools, no process), and its budget is the
//! one the owner named. Only when the owner passes `--tool` (#1039) are the
//! participants read-only independent children instead, holding exactly the
//! connected tools the owner named, each asked once per discussion
//! ([`helper_tools`]). Nothing here writes a file in the workspace or widens a
//! secret, allow pattern or profile: the host library
//! ([`crate::discussion_host`]) does the work and only the gate decides.

pub mod audit;
pub mod files;
pub mod helper_tools;
pub mod host;
pub mod owner;
pub mod tool;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tole_core::advisor::Budget;
use tole_core::discussion::{self, Background, Contract, Gate};
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
    /// Background files (#1038): paths as the owner typed them, inside the
    /// working folder ([`Env::workdir`]).
    pub files: Vec<String>,
    /// The connected tools the owner allows the helpers (#1039), as registry
    /// names (`mcp_<server>_<tool>`, from `--tool <server>.<tool>` by
    /// [`helper_tools::resolve`]). Empty: inline voices with no tools, as ever.
    pub tools: Vec<String>,
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
    /// The working folder background files are named against and read from.
    pub workdir: &'a Path,
    /// The host that starts helper children, for a discussion that allows
    /// tools (#1039); `None` refuses such a discussion.
    pub delegator: Option<Arc<crate::delegate::Delegator>>,
}

/// A fresh discussion id from the clock (`d-<unix seconds>`).
pub fn fresh_id(now: u64) -> String {
    format!("d-{now}")
}

/// The validated contract for `args`. Background files are named against
/// `workdir`: each is confined to it and read once here, so a refused file
/// (outside it, a symlink, a FIFO, over the size cap) fails before anything
/// starts; the contract keeps only the names and digests.
pub fn contract(
    args: &StartArgs,
    workdir: &Path,
    now: u64,
    secrets: &SecretSet,
) -> Result<Contract, String> {
    contract_and_background(args, workdir, now, secrets).map(|(c, _)| c)
}

/// [`contract`] and the files it read, as the voices will see them: ONE read
/// of each file serves the digest in the contract, the estimate and the first
/// run, so an edit in between cannot leave a ledger that can never resume.
fn contract_and_background(
    args: &StartArgs,
    workdir: &Path,
    now: u64,
    secrets: &SecretSet,
) -> Result<(Contract, Vec<Background>), String> {
    let id = args.id.clone().unwrap_or_else(|| fresh_id(now));
    let c = Contract::new(
        &id,
        &args.question,
        &args.criteria,
        &args.participants,
        args.budget_tokens,
        args.max_rounds,
        secrets,
    )?;
    let c = c.with_tools(&args.tools)?;
    if args.files.is_empty() {
        return Ok((c, Vec::new()));
    }
    // The count and the duplicates first: nothing is read, hashed or
    // sanitized for a list that is refused anyway, and a per-file error
    // cannot hide them.
    if args.files.len() > discussion::MAX_BACKGROUND_FILES {
        return Err(format!(
            "{} background files: at most {} are accepted",
            args.files.len(),
            discussion::MAX_BACKGROUND_FILES
        ));
    }
    let names = files::relative(workdir, &args.files)?;
    if let Some(dup) = names
        .iter()
        .enumerate()
        .find_map(|(i, n)| names[..i].contains(n).then_some(n))
    {
        return Err(format!("background file {dup:?} is listed twice"));
    }
    // Read once now: a refused file fails here, and the contract records the
    // names with the digests of the bytes it saw.
    let read = files::load(workdir, &names, secrets)?;
    let digests: Vec<String> = read.iter().map(|b| b.sha256.clone()).collect();
    Ok((c.with_files(&names, &digests)?, read))
}

/// The contract for `args` and the background files it names, as the voices
/// will read them.
fn prepared(
    args: &StartArgs,
    workdir: &Path,
    now: u64,
    secrets: &SecretSet,
) -> Result<(Contract, Vec<Background>), String> {
    contract_and_background(args, workdir, now, secrets)
}

/// What the dry run and the start say about the content of background files,
/// whether or not anything was replaced: what is redacted is only what tole
/// can recognise.
const NOTE: &str = "Secrets: only known values and token shapes are redacted from the \
files (a `.env`-style file is redacted line by line); anything else in a file, such as a password in \
prose or in a config file, goes as written to the provider of every inline participant. Name only \
files you would paste into the question.";

/// What the discussion may cost, before it runs: the budget is a hard cap
/// (every call is reserved in the ledger before it runs), the rounds and
/// calls are the most the contract allows, and the cost of a round grows
/// with the number of participants ([`host::round_floor`]).
pub fn cost_plan(c: &Contract) -> String {
    cost_plan_with(c, &[])
}

/// [`cost_plan`] for a contract whose inline voices also read `background`:
/// the excerpts are in every participant's request, so they are in the
/// estimate and listed.
pub fn cost_plan_with(c: &Contract, background: &[Background]) -> String {
    let n = c.participants.len();
    let per_round = n + 2;
    let floor = host::round_floor_with(c, background);
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
    let tools = if c.tools.is_empty() {
        String::new()
    } else {
        format!(
            "Helper tools: {}. Every participant runs as an independent read-only child (at \
             least {} tokens reserved per turn), and the first call of each tool is asked of you \
             in the approvals queue (`tole approvals`): allow covers that tool for this \
             discussion only.\n",
            c.tools.join(", "),
            host::INDEPENDENT_MIN_TOKENS
        )
    };
    let files = if background.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = background
            .iter()
            .map(|b| {
                format!(
                    "{} ({} characters shown{}{})",
                    b.path,
                    b.text.chars().count(),
                    if b.truncated {
                        ", cut to the excerpt cap"
                    } else {
                        ""
                    },
                    if b.redacted { ", values redacted" } else { "" },
                )
            })
            .collect();
        format!(
            "Background files, read by every inline participant each round and counted in the \
             estimate below: {}.\n\
             {NOTE}\n",
            list.join("; ")
        )
    };
    format!(
        "Discussion `{}`: {} participants ({}), up to {} rounds, up to {} model calls \
         (each round: one per participant, a scribe and a claim-checker).\n\
         Question: {q_chars} characters (about {} tokens), sent with every call and counted \
         in the estimate below.\n\
         {files}{tools}\
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
    advice_with(c, &[])
}

/// [`advice`] with the background files the voices read counted in the floor.
pub fn advice_with(c: &Contract, background: &[Background]) -> Vec<String> {
    let mut out: Vec<String> = discussion::participants_advice(c.participants.len())
        .into_iter()
        .collect();
    // A helper child is reserved at least this many tokens a turn (#1039), a
    // floor the inline estimate below does not see.
    let helpers = c.participants.len() as u64 * host::INDEPENDENT_MIN_TOKENS;
    if !c.tools.is_empty() && c.budget_tokens < helpers {
        out.push(format!(
            "helpers with tools are independent children, each reserved at least {} tokens a \
             turn: the budget ({} tokens) does not fit one round of {} of them ({helpers} \
             reserved), so the discussion will stop with a budget escalation in round 1",
            host::INDEPENDENT_MIN_TOKENS,
            c.budget_tokens,
            c.participants.len()
        ));
    }
    let floor = host::round_floor_with(c, background);
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
pub fn advice_for(args: &StartArgs, workdir: &Path, secrets: &SecretSet) -> Vec<String> {
    prepared(args, workdir, now_secs(), secrets)
        .map(|(c, bg)| advice_with(&c, &bg))
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
    let (c, bg) = prepared(args, env.workdir, now_secs(), &env.secrets)?;
    let log = DiscussionLog::at(env.dir, &c.id)?;
    check_participants(&c, env, log)?;
    if !c.tools.is_empty() {
        helper_tools::need_approvals(env.cfg)?;
    }
    Ok(format!("{}\n", cost_plan_with(&c, &bg)))
}

/// [`plan`] for a client that draws its own screen: the same dry run (the
/// contract, its participants and background files are validated, nothing is
/// written, no provider is called) as numbers next to the text. The cost plan
/// is redacted like every other text the owner reads.
pub fn estimate(args: &StartArgs, env: &Env<'_>) -> Result<serde_json::Value, String> {
    let (c, bg) = prepared(args, env.workdir, now_secs(), &env.secrets)?;
    check_participants(&c, env, DiscussionLog::at(env.dir, &c.id)?)?;
    let calls = (c.participants.len() + 2) * c.max_rounds as usize;
    Ok(env.secrets.redact_value(&serde_json::json!({
        "plan": cost_plan_with(&c, &bg),
        "advice": advice_with(&c, &bg),
        "participants": c.participants,
        "max_rounds": c.max_rounds,
        "max_calls": calls,
        "budget_tokens": c.budget_tokens,
        "round_floor_tokens": host::round_floor_with(&c, &bg),
        "files": c.files,
    })))
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

/// A discussion that is recorded and ready to run: what [`begin`] and
/// [`prepare_resume`] hand to whoever drives it (the terminal blocks on it,
/// `tole serve` runs it on a thread).
pub struct Ready {
    pub driver: Driver,
    pub id: String,
    /// The cost plan shown to the owner before it ran.
    pub plan: String,
    /// What the owner should know before it spends anything ([`advice_with`]).
    pub advice: Vec<String>,
}

/// Validates the owner's contract and records it, without running a round.
/// Everything that can be refused is refused here, before anything is spent;
/// `tole discuss start` and the serve route both go through this one path.
pub fn begin(args: &StartArgs, env: &Env<'_>) -> Result<Ready, String> {
    let (c, bg) = prepared(args, env.workdir, now_secs(), &env.secrets)?;
    let plan = cost_plan_with(&c, &bg);
    let advice = advice_with(&c, &bg);
    let driver = Driver::from_config(
        env.cfg,
        env.paths,
        &c.participants,
        DiscussionLog::at(env.dir, &c.id)?,
        env.secrets.clone(),
        Arc::clone(&env.make),
        Arc::new(Budget::new(c.budget_tokens)),
    )?
    .with_workdir(env.workdir)
    .with_background(bg);
    let driver = helper_tools::seat(driver, &c, env.cfg, env.delegator.as_ref())?;
    // Before anything is written: the files the contract was built from are
    // the ones the driver will show, so the ledger can always be resumed.
    driver.check_files(&c)?;
    let id = c.id.clone();
    driver.start(c)?;
    Ok(Ready {
        driver,
        id,
        plan,
        advice,
    })
}

/// Starts a discussion from the owner's contract and runs it to its end.
pub fn start(args: &StartArgs, env: &Env<'_>) -> Result<String, String> {
    let Ready {
        driver, id, plan, ..
    } = begin(args, env)?;
    let out = driver.run().map_err(|e| resumable(&id, &env.secrets, &e))?;
    Ok(format!("{plan}\n\n{}", finish(&out, &env.secrets)))
}

/// The driver that continues discussion `id` where its ledger stands.
pub fn prepare_resume(id: &str, env: &Env<'_>) -> Result<Ready, String> {
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
    )?
    .with_workdir(env.workdir);
    // Helpers only when a step can still start one: a stopped or finished
    // discussion is closed or shown without a delegation host (#1039).
    let driver = if helper_tools::needs_helpers(&st.next()) {
        helper_tools::seat(driver, &c, env.cfg, env.delegator.as_ref())?
    } else {
        driver
    };
    // A file that is gone or changed is not a failure a resume can cure, so it
    // is reported as it is, without the "continue it with resume" advice.
    driver.check_files(&c)?;
    Ok(Ready {
        driver,
        id: id.to_string(),
        plan: String::new(),
        advice: Vec::new(),
    })
}

/// Continues discussion `id` where its ledger stands (after a crash or a
/// failed call); a finished one just prints its outcome again.
pub fn resume(id: &str, env: &Env<'_>) -> Result<String, String> {
    let ready = prepare_resume(id, env)?;
    let out = ready
        .driver
        .run()
        .map_err(|e| resumable(id, &env.secrets, &e))?;
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
