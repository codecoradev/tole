//! `tole curator` (#593 part 2, design `skills-registry-design.md` §4.5): the
//! owner's side of keeping the skill library small.
//!
//! `run` is the deterministic sweep: it assesses every skill the curator
//! manages (agent-grown, that is promoted from a draft, or adopted by the
//! owner), and archives those unused past the thresholds. `--dry-run` only
//! reports. A paused curator changes nothing. Archiving moves the whole skill
//! package into the archive with a mutation-ledger entry, so `restore` puts it
//! back exactly. Pinned skills, skills a cron job names and skills the
//! curator does not manage are never touched. Underperformers (a skill whose
//! runs do worse than runs without one) are listed for the owner, never
//! acted on.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tole_core::skill_curator::{
    plan, underperforming, Archive, Assessment, CuratorState, Lifecycle, Mutation, SkillFacts,
    Thresholds, Underperformer, Usage, UsageLedger, UsageSummary,
};
use tole_core::skill_draft::{name_ok, DraftStatus, DraftStore};

/// A lock this old is a crashed sweep's, not a running one's.
const LOCK_STALE_SECS: u64 = 600;

/// `tole curator <action>`.
#[derive(Debug, clap::Subcommand)]
pub enum CuratorAction {
    /// What the curator manages, where each skill is in its life, and what
    /// was archived.
    Status {
        /// Discovery root for adopted skills (default: the current directory).
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Stop `run` from changing anything until `resume`.
    Pause,
    /// Let `run` change things again.
    Resume,
    /// Assess every managed skill and archive the ones unused past the
    /// thresholds (14 days stale, 30 days archived). `--dry-run` reports only.
    Run {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Put an archived skill back where it was.
    Restore {
        name: String,
    },
    /// Never archive this skill.
    Pin {
        name: String,
    },
    Unpin {
        name: String,
    },
    /// Mark this skill as not editable by an agent: `skill_propose` refuses
    /// a draft under its name.
    Lock {
        name: String,
    },
    Unlock {
        name: String,
    },
    /// Put a hand-written skill under curation (archiving included).
    Adopt {
        name: String,
    },
    Unadopt {
        name: String,
    },
    /// Run `tole curator run` every week on the built-in scheduler (a
    /// script job `curator-weekly`, zero tokens). Needs a scheduler tick:
    /// `tole serve --cron`, or cron(8) calling `tole cron tick`.
    #[cfg(feature = "shell-tools")]
    Schedule {
        /// 5-field cron in UTC [default: `0 4 * * 1`, Mondays 04:00].
        #[arg(long)]
        cron: Option<String>,
    },
    /// Remove the weekly job.
    #[cfg(feature = "shell-tools")]
    Unschedule,
}

/// A managed skill with the directory its package lives in.
#[derive(Debug, Clone, PartialEq)]
pub struct Managed {
    pub facts: SkillFacts,
    pub dir: PathBuf,
}

/// What a sweep needs, all injectable.
pub struct Env<'a> {
    pub drafts: &'a DraftStore,
    pub state_path: &'a Path,
    pub usage: &'a UsageLedger,
    pub archive: &'a Archive,
    pub lock_path: &'a Path,
    /// Skill names live cron jobs run with.
    pub cron_skills: BTreeSet<String>,
    /// Discovery root for adopted skills.
    pub ws: Option<&'a Path>,
    pub thresholds: Thresholds,
    /// `[curator] enabled` of the user config (default on).
    pub enabled: bool,
    pub now: u64,
}

/// The skills the curator manages: promoted drafts whose directory is still
/// there, and the adopted names it can find.
pub fn gather(env: &Env<'_>, state: &CuratorState, usage: &UsageSummary) -> Vec<Managed> {
    let mut by_name: std::collections::BTreeMap<String, Managed> = Default::default();
    let mut add = |name: &str, dir: PathBuf, created_at: u64| {
        by_name.insert(
            name.to_string(),
            Managed {
                facts: SkillFacts {
                    name: name.to_string(),
                    managed: true,
                    pinned: state.pinned.contains(name),
                    cron_protected: env.cron_skills.contains(name),
                    created_at,
                    usage: usage.skills.get(name).cloned().unwrap_or_default(),
                },
                dir,
            },
        );
    };
    for d in env.drafts.list() {
        if d.status != DraftStatus::Promoted {
            continue;
        }
        let Some(p) = &d.promotion else { continue };
        let dir = PathBuf::from(&p.dir);
        if dir.is_dir() && name_ok(&d.name).is_ok() {
            add(&d.name, dir, p.at);
        }
    }
    for name in &state.adopted {
        let Some(dir) = tole_core::skills::skill_file(env.ws, name)
            .and_then(|f| f.parent().map(Path::to_path_buf))
        else {
            continue;
        };
        let first = usage.skills.get(name).map_or(0, |u| u.first_seen);
        let created = if first > 0 { first } else { mtime_secs(&dir) };
        add(name, dir, created);
    }
    by_name.into_values().collect()
}

fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// What one sweep did (or would do).
#[derive(Debug, Default)]
pub struct Report {
    /// Why nothing was changed (paused), if so.
    pub held: Option<String>,
    pub assessments: Vec<Assessment>,
    pub archived: Vec<Mutation>,
    pub would_archive: Vec<String>,
    pub underperformers: Vec<Underperformer>,
    pub errors: Vec<String>,
}

/// A sweep-wide lock file, created exclusively; one stale for
/// [`LOCK_STALE_SECS`] is taken over.
struct Lock(PathBuf);

impl Lock {
    fn acquire(path: &Path, now: u64) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => return Ok(Self(path.to_path_buf())),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if now.saturating_sub(mtime_secs(path)) < LOCK_STALE_SECS {
                        return Err("another curator sweep is running".into());
                    }
                    let _ = std::fs::remove_file(path);
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Err("another curator sweep is running".into())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The sweep. `dry_run` and a paused curator change nothing.
pub fn sweep(env: &Env<'_>, dry_run: bool) -> Result<Report, String> {
    env.thresholds.check()?;
    let state = CuratorState::load(env.state_path)?;
    let usage = env.usage.summary();
    let managed = gather(env, &state, &usage);
    let facts: Vec<SkillFacts> = managed.iter().map(|m| m.facts.clone()).collect();
    let mut report = Report {
        assessments: plan(&facts, &env.thresholds, env.now),
        underperformers: underperforming(&facts, &usage.baseline),
        ..Report::default()
    };
    let due: Vec<&Assessment> = report
        .assessments
        .iter()
        .filter(|a| a.lifecycle == Lifecycle::Archived)
        .collect();
    if (state.paused || !env.enabled) && !due.is_empty() && !dry_run {
        report.held = Some(if state.paused {
            "the curator is paused: nothing was changed".into()
        } else {
            "the curator is switched off (curator.enabled = false in the user config): nothing was changed".into()
        });
        report.would_archive = due.iter().map(|a| a.name.clone()).collect();
        return Ok(report);
    }
    if dry_run {
        report.would_archive = due.iter().map(|a| a.name.clone()).collect();
        return Ok(report);
    }
    if due.is_empty() {
        return Ok(report);
    }
    let _lock = Lock::acquire(env.lock_path, env.now)?;
    for a in due {
        let Some(m) = managed.iter().find(|m| m.facts.name == a.name) else {
            continue;
        };
        match env.archive.archive(&a.name, &m.dir, &a.reason, env.now) {
            Ok(mutation) => report.archived.push(mutation),
            Err(e) => report.errors.push(format!("{}: {e}", a.name)),
        }
    }
    Ok(report)
}

fn printable(s: &str) -> String {
    crate::skill_drafts_cli::printable(s)
}

/// The sweep's report as text.
pub fn render_report(r: &Report, dry_run: bool) -> String {
    let mut out = Vec::new();
    if let Some(h) = &r.held {
        out.push(h.clone());
    }
    if r.assessments.is_empty() {
        out.push("no managed skills".into());
    }
    for a in &r.assessments {
        out.push(format!(
            "{}  {}  {}",
            printable(&a.name),
            lifecycle_word(a.lifecycle),
            printable(&a.reason)
        ));
    }
    let verb = if dry_run || r.held.is_some() {
        "would archive"
    } else {
        "archive"
    };
    for n in &r.would_archive {
        out.push(format!("{verb}: {}", printable(n)));
    }
    for m in &r.archived {
        out.push(format!(
            "archived {} (entry {}, restore with `tole curator restore {}`)",
            printable(&m.skill),
            m.id,
            printable(&m.skill)
        ));
    }
    for u in &r.underperformers {
        out.push(format!(
            "proposal: review {}: it succeeds {:.0}% of {} runs, runs without a skill {:.0}%",
            printable(&u.name),
            u.rate * 100.0,
            u.outcomes,
            u.baseline * 100.0
        ));
    }
    for e in &r.errors {
        out.push(format!("error: {}", printable(e)));
    }
    out.join("\n")
}

fn lifecycle_word(l: Lifecycle) -> &'static str {
    match l {
        Lifecycle::Active => "active",
        Lifecycle::Stale => "stale",
        Lifecycle::Archived => "due for archive",
    }
}

/// The `status` text.
pub fn render_status(env: &Env<'_>) -> Result<String, String> {
    let state = CuratorState::load(env.state_path)?;
    let usage = env.usage.summary();
    let managed = gather(env, &state, &usage);
    let facts: Vec<SkillFacts> = managed.iter().map(|m| m.facts.clone()).collect();
    let assessments = plan(&facts, &env.thresholds, env.now);
    let count = |l: Lifecycle| assessments.iter().filter(|a| a.lifecycle == l).count();
    let archived = env.archive.archived();
    let list = |s: &BTreeSet<String>| {
        if s.is_empty() {
            "none".to_string()
        } else {
            printable(&s.iter().cloned().collect::<Vec<_>>().join(", "))
        }
    };
    let mut out = vec![
        format!(
            "curator: {}",
            if !env.enabled {
                "off (curator.enabled = false in the user config)"
            } else if state.paused {
                "paused"
            } else {
                "on"
            }
        ),
        format!(
            "managed: {} ({} active, {} stale, {} due for archive; {} pinned or cron-named left alone)",
            managed.len(),
            count(Lifecycle::Active),
            count(Lifecycle::Stale),
            count(Lifecycle::Archived),
            facts.len() - assessments.len()
        ),
        format!(
            "thresholds: stale after {} days, archive after {} days, {} days of grace for a skill never used",
            env.thresholds.stale_days, env.thresholds.archive_days, env.thresholds.grace_days
        ),
        format!("pinned: {}", list(&state.pinned)),
        format!("locked: {}", list(&state.locked)),
        format!("adopted: {}", list(&state.adopted)),
    ];
    if archived.is_empty() {
        out.push("archived: none".into());
    }
    for m in &archived {
        out.push(format!(
            "archived: {} (entry {}, {})",
            printable(&m.skill),
            m.id,
            tole_core::skill_approval::format_time(m.t)
        ));
    }
    let base: &Usage = &usage.baseline;
    out.push(format!(
        "runs without a skill on record: {}",
        base.outcomes()
    ));
    Ok(out.join("\n"))
}

/// Change one name in one of the state's sets; returns the line to print.
pub fn edit_state(
    path: &Path,
    name: &str,
    set: fn(&mut CuratorState) -> &mut BTreeSet<String>,
    add: bool,
    what: &str,
) -> Result<String, String> {
    name_ok(name)?;
    let mut state = CuratorState::load(path)?;
    let changed = if add {
        set(&mut state).insert(name.to_string())
    } else {
        set(&mut state).remove(name)
    };
    if !changed {
        return Ok(format!(
            "{name} is already {}{what}",
            if add { "" } else { "not " }
        ));
    }
    state.save(path)?;
    Ok(format!(
        "{name} is {}{what}",
        if add { "" } else { "no longer " }
    ))
}

/// The user config's `[curator]` table. A user config that cannot be read
/// is reported and the defaults apply, as `tole cron` does.
fn curator_config() -> Option<crate::config::CuratorConfig> {
    let path = crate::config::user_config_path().filter(|p| p.exists())?;
    match crate::config::load(&path) {
        Ok(c) => c.curator,
        Err(e) => {
            eprintln!("tole curator: user config not applied: {e}");
            None
        }
    }
}

/// The weekly sweep as a job on the built-in scheduler (#593).
#[cfg(feature = "shell-tools")]
pub mod schedule {
    use super::*;

    /// The job's id.
    pub const JOB_ID: &str = "curator-weekly";
    /// Mondays 04:00 UTC.
    pub const DEFAULT_SCHEDULE: &str = "0 4 * * 1";

    /// The live job, if any.
    pub fn find(dir: &Path) -> Option<crate::cron::JobRow> {
        crate::cron::list(dir)
            .ok()?
            .into_iter()
            .find(|j| j.id == JOB_ID)
    }

    /// Save the weekly script job `tole curator run`. The scheduler runs a
    /// script as argv with no shell and no tokens, and refuses a
    /// path-qualified program, so `tole` is found on the scheduler's PATH.
    /// Refuses when the job exists.
    pub fn install(
        dir: &Path,
        schedule: Option<&str>,
        workdir: &Path,
        now: i64,
    ) -> anyhow::Result<String> {
        if let Some(j) = find(dir) {
            anyhow::bail!(
                "{JOB_ID} already exists ({}, {}); `tole curator unschedule` first to change it",
                j.schedule,
                j.state
            );
        }
        let schedule = schedule.unwrap_or(DEFAULT_SCHEDULE).to_string();
        let spec = crate::cron::CreateSpec {
            id: Some(JOB_ID.into()),
            name: Some("skill curator".into()),
            schedule: schedule.clone(),
            script: Some("tole curator run".into()),
            workdir: workdir.to_path_buf(),
            ..Default::default()
        };
        crate::cron::create(dir, spec, now)?;
        Ok(format!(
            "scheduled {JOB_ID}: `tole curator run` on `{schedule}` (UTC). It runs when the \
             scheduler ticks: `tole serve --cron`, or cron(8) calling `tole cron tick`."
        ))
    }

    /// Remove the weekly job.
    pub fn remove(dir: &Path, now: i64) -> anyhow::Result<String> {
        if find(dir).is_none() {
            anyhow::bail!("{JOB_ID} is not scheduled");
        }
        crate::cron::update(dir, JOB_ID, now, |j| j.removed = true)?;
        Ok(format!("removed {JOB_ID}"))
    }

    /// One status line.
    pub fn line(dir: &Path) -> String {
        match find(dir) {
            Some(j) => format!("schedule: {JOB_ID} on `{}` ({})", j.schedule, j.state),
            None => "schedule: not scheduled (`tole curator schedule` adds a weekly sweep)".into(),
        }
    }

    pub fn status_line() -> String {
        match crate::cron::default_dir() {
            Ok(d) => line(&d),
            Err(e) => format!("schedule: unknown ({e})"),
        }
    }

    pub fn install_cli(cron: Option<&str>) -> anyhow::Result<String> {
        use anyhow::Context as _;
        let dir = crate::cron::default_dir()?;
        let workdir = tole_core::paths::tole_data_dir()
            .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
        std::fs::create_dir_all(&workdir)
            .with_context(|| format!("creating {}", workdir.display()))?;
        let mut msg = install(&dir, cron, &workdir, crate::cron::now())?;
        if !crate::tool_roster::binary_available("tole") {
            msg.push_str(
                "\nWarning: `tole` is not on this PATH. The job runs `tole curator run`, so the \
                 scheduler's PATH must hold it or the run fails (see `tole cron runs`).",
            );
        }
        Ok(msg)
    }

    pub fn remove_cli() -> anyhow::Result<String> {
        remove(&crate::cron::default_dir()?, crate::cron::now())
    }
}

/// The doctor line: whether the curator is on and scheduled.
pub fn doctor_line() -> String {
    let cfg = curator_config();
    if cfg.as_ref().and_then(|c| c.enabled) == Some(false) {
        return "curator: off (curator.enabled = false)".into();
    }
    #[cfg(feature = "shell-tools")]
    {
        let sched = match crate::cron::default_dir() {
            Ok(d) => schedule::find(&d)
                .map(|j| format!("scheduled on `{}`", j.schedule))
                .unwrap_or_else(|| "not scheduled (`tole curator schedule`)".into()),
            Err(_) => "schedule unknown".into(),
        };
        format!("curator: on, {sched}")
    }
    #[cfg(not(feature = "shell-tools"))]
    "curator: on".to_string()
}

/// The owned pieces an [`Env`] borrows from.
struct Parts<'p> {
    drafts: &'p DraftStore,
    state_path: &'p Path,
    usage: &'p UsageLedger,
    archive: &'p Archive,
    lock_path: &'p Path,
    cron_skills: BTreeSet<String>,
    thresholds: Thresholds,
    enabled: bool,
}

impl<'p> Parts<'p> {
    fn env<'a>(&'a self, ws: Option<&'a Path>) -> Env<'a> {
        Env {
            drafts: self.drafts,
            state_path: self.state_path,
            usage: self.usage,
            archive: self.archive,
            lock_path: self.lock_path,
            cron_skills: self.cron_skills.clone(),
            ws,
            thresholds: self.thresholds,
            enabled: self.enabled,
            now: now_secs(),
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `tole curator <action>`.
pub fn cli(action: CuratorAction) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let data = tole_core::paths::tole_data_dir()
        .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
    let state_path = CuratorState::default_path().context("the CodeCora home is unresolvable")?;
    let drafts = DraftStore::default_store().context("the CodeCora home is unresolvable")?;
    let usage = UsageLedger::default_ledger().context("the CodeCora home is unresolvable")?;
    let archive = Archive::default_archive().context("the CodeCora home is unresolvable")?;
    let lock_path = data.join("skill-curator.lock");
    let ws_of = |w: Option<String>| -> anyhow::Result<PathBuf> {
        match w {
            Some(p) => Ok(PathBuf::from(p)),
            None => std::env::current_dir().context("reading the current directory"),
        }
    };
    #[cfg(feature = "shell-tools")]
    let cron_skills = crate::cron::default_dir()
        .map(|d| crate::cron::skill_names(&d))
        .unwrap_or_default();
    #[cfg(not(feature = "shell-tools"))]
    let cron_skills = BTreeSet::new();
    let cfg = curator_config();
    let parts = Parts {
        drafts: &drafts,
        state_path: &state_path,
        usage: &usage,
        archive: &archive,
        lock_path: &lock_path,
        cron_skills,
        thresholds: crate::config::curator_thresholds(cfg.as_ref()),
        enabled: cfg.as_ref().and_then(|c| c.enabled).unwrap_or(true),
    };
    let msg = |r: Result<String, String>| r.map_err(anyhow::Error::msg);
    match action {
        CuratorAction::Status { workspace } => {
            let ws = ws_of(workspace)?;
            println!("{}", msg(render_status(&parts.env(Some(&ws))))?);
            #[cfg(feature = "shell-tools")]
            println!("{}", schedule::status_line());
        }
        CuratorAction::Run { dry_run, workspace } => {
            let ws = ws_of(workspace)?;
            let report = sweep(&parts.env(Some(&ws)), dry_run).map_err(anyhow::Error::msg)?;
            println!("{}", render_report(&report, dry_run));
            if !report.errors.is_empty() {
                anyhow::bail!("{} skill(s) could not be archived", report.errors.len());
            }
        }
        CuratorAction::Pause | CuratorAction::Resume => {
            let pause = matches!(action, CuratorAction::Pause);
            let mut state = CuratorState::load(&state_path).map_err(anyhow::Error::msg)?;
            state.paused = pause;
            state.save(&state_path).map_err(anyhow::Error::msg)?;
            println!("curator {}", if pause { "paused" } else { "resumed" });
        }
        CuratorAction::Restore { name } => {
            let m = archive
                .restore(&name, now_secs())
                .map_err(anyhow::Error::msg)?;
            println!("restored {} to {}", printable(&m.skill), printable(&m.to));
        }
        CuratorAction::Pin { name } => {
            println!(
                "{}",
                msg(edit_state(
                    &state_path,
                    &name,
                    |s| &mut s.pinned,
                    true,
                    "pinned"
                ))?
            )
        }
        CuratorAction::Unpin { name } => {
            println!(
                "{}",
                msg(edit_state(
                    &state_path,
                    &name,
                    |s| &mut s.pinned,
                    false,
                    "pinned"
                ))?
            )
        }
        CuratorAction::Lock { name } => {
            println!(
                "{}",
                msg(edit_state(
                    &state_path,
                    &name,
                    |s| &mut s.locked,
                    true,
                    "locked"
                ))?
            )
        }
        CuratorAction::Unlock { name } => {
            println!(
                "{}",
                msg(edit_state(
                    &state_path,
                    &name,
                    |s| &mut s.locked,
                    false,
                    "locked"
                ))?
            )
        }
        CuratorAction::Adopt { name } => {
            println!(
                "{}",
                msg(edit_state(
                    &state_path,
                    &name,
                    |s| &mut s.adopted,
                    true,
                    "adopted"
                ))?
            )
        }
        #[cfg(feature = "shell-tools")]
        CuratorAction::Schedule { cron } => {
            println!("{}", schedule::install_cli(cron.as_deref())?);
        }
        #[cfg(feature = "shell-tools")]
        CuratorAction::Unschedule => {
            println!("{}", schedule::remove_cli()?);
        }
        CuratorAction::Unadopt { name } => {
            println!(
                "{}",
                msg(edit_state(
                    &state_path,
                    &name,
                    |s| &mut s.adopted,
                    false,
                    "adopted"
                ))?
            )
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "curator_cmd_tests.rs"]
mod tests;
