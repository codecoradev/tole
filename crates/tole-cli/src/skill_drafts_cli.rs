//! `tole skills drafts` (#592 part 2): the owner's side of skill growth.
//!
//! A draft written by `skill_propose` waits here. The owner lists, reads,
//! rejects and records a reviewer's verdict. Only `promote` makes a draft a
//! skill, and only for the exact bytes the owner confirmed; `revoke` takes
//! back what grew from a source that turned out poisoned.
//!
//! Everything shown came from a model, so it is printed through
//! [`printable`]: a control or invisible character is shown as `\u{..}`
//! instead of reaching the terminal.

use std::path::{Path, PathBuf};

use tole_core::skill_approval::{format_time, read_package, Approvals};
use tole_core::skill_draft::{
    is_hidden, Band, Draft, DraftStatus, DraftStore, Promotion, Severity, Verdict,
};
use tole_core::skill_index::content_hash;

/// `tole skills drafts [action]`; without one, the pending drafts.
#[derive(Debug, clap::Subcommand)]
pub enum DraftsAction {
    /// List drafts (the pending ones unless --all).
    List {
        #[arg(long)]
        all: bool,
    },
    /// Show one draft in full: provenance, taint, warnings, the nearest
    /// skill and the proposed SKILL.md.
    Show {
        id: String,
        /// Discovery root for the nearest skill (default: the current directory).
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Reject a pending draft. It stays on disk, out of the pending list.
    Reject { id: String },
    /// Record a reviewer's call on the draft's nearest skill. `conflict`
    /// means similar but contradictory: never merged.
    Verdict { id: String, verdict: VerdictArg },
    /// Turn a pending draft into a skill and approve its exact bytes. Shows
    /// the draft first, then asks; without a terminal, --hash (the content
    /// hash `show` printed) is required. A tainted draft needs
    /// --accept-taint; a draft close to an existing skill needs the verdict
    /// `new`.
    Promote {
        id: String,
        /// Promote only if the draft is still this content hash.
        #[arg(long)]
        hash: Option<String>,
        /// Write to `<workspace>/skills` instead of the user-global skills dir.
        #[arg(long)]
        workspace: Option<String>,
        /// Confirm you read a draft whose session consumed untrusted content.
        #[arg(long)]
        accept_taint: bool,
    },
    /// Take back what grew from a source: SOURCE is a session id (or a draft
    /// id). Its pending drafts are rejected and the approval of each skill
    /// promoted from it is withdrawn; the skill stays on disk.
    Revoke { source: String },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum VerdictArg {
    Patch,
    Reference,
    New,
    Conflict,
}

impl From<VerdictArg> for Verdict {
    fn from(v: VerdictArg) -> Self {
        match v {
            VerdictArg::Patch => Verdict::Patch,
            VerdictArg::Reference => Verdict::Reference,
            VerdictArg::New => Verdict::New,
            VerdictArg::Conflict => Verdict::Conflict,
        }
    }
}

/// `s` with every control or invisible character (newline and tab kept)
/// replaced by its `\u{..}` escape, so model text cannot move the cursor,
/// recolor the terminal or hide characters from the owner.
pub fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\n' || c == '\t' || !(c.is_control() || is_hidden(c)) {
            out.push(c);
        } else {
            out.push_str(&format!("\\u{{{:04x}}}", c as u32));
        }
    }
    out
}

fn status_word(s: DraftStatus) -> &'static str {
    match s {
        DraftStatus::Pending => "pending",
        DraftStatus::Rejected => "rejected",
        DraftStatus::Promoted => "promoted",
        DraftStatus::Revoked => "revoked",
    }
}

/// One line per draft.
pub fn format_list(drafts: &[Draft], all: bool) -> String {
    let rows: Vec<String> = drafts
        .iter()
        .filter(|d| all || d.status == DraftStatus::Pending)
        .map(|d| {
            format!(
                "{}  {}  {}{}  {:?}  {}",
                printable(&d.id),
                status_word(d.status),
                if d.tainted() { "TAINTED  " } else { "" },
                printable(&d.name),
                d.dedup.band,
                format_time(d.created_at)
            )
        })
        .collect();
    if rows.is_empty() {
        return if all {
            "no drafts".into()
        } else {
            "no pending drafts".into()
        };
    }
    rows.join("\n")
}

/// A draft in full. `nearest_md` is the nearest skill's SKILL.md, when it
/// could be read, shown after the draft so the two can be compared.
pub fn format_show(d: &Draft, skill_md: &str, nearest_md: Option<&str>) -> String {
    let mut out = format!(
        "draft {}  {}\nname: {}\ndescription: {}\ncreated: {}\ncontent: {}\n",
        printable(&d.id),
        status_word(d.status),
        printable(&d.name),
        printable(&d.description),
        format_time(d.created_at),
        d.content_hash
    );
    let p = &d.provenance;
    out.push_str(&format!(
        "trigger: {:?}\nsessions: {}\nmodel: {}\n",
        p.trigger,
        if p.sessions.is_empty() {
            "unknown".to_string()
        } else {
            printable(&p.sessions.join(", "))
        },
        printable(p.model.as_deref().unwrap_or("unknown"))
    ));
    if d.tainted() {
        out.push_str(&format!(
            "TAINTED: this session consumed untrusted content ({}). Read every line below.\n",
            printable(&d.taint.join(", "))
        ));
    }
    for w in &d.warnings {
        let sev = match w.severity {
            Severity::Block => "block",
            Severity::Warn => "warning",
        };
        out.push_str(&format!(
            "{sev}: {}: {}\n",
            printable(&w.rule),
            printable(&w.detail)
        ));
    }
    match &d.dedup.nearest {
        Some(n) => out.push_str(&format!(
            "nearest skill: {} (score {:.2}, proposed {:?}{})\n",
            printable(n),
            d.dedup.score,
            d.dedup.band,
            match d.dedup.verdict {
                Some(v) => format!(", reviewer says {v:?}"),
                None => String::new(),
            }
        )),
        None => out.push_str("nearest skill: none found\n"),
    }
    out.push_str("\n--- proposed SKILL.md ---\n");
    out.push_str(&printable(skill_md));
    if let (Some(n), Some(md)) = (&d.dedup.nearest, nearest_md) {
        out.push_str(&format!(
            "\n--- nearest skill {} (as it is now) ---\n",
            printable(n)
        ));
        out.push_str(&printable(md));
    }
    out
}

fn nearest_md(ws: &Path, d: &Draft) -> Option<String> {
    let name = d.dedup.nearest.as_deref()?;
    let pkg = read_package(Some(ws), name).ok()?;
    String::from_utf8(pkg.skill_md().to_vec()).ok()
}

/// Where a promotion writes and which approvals it updates.
pub struct PromoteEnv<'a> {
    pub store: &'a DraftStore,
    /// Discovery root: the skill lands in `<ws>/skills/<name>`; `None` is
    /// the user-global skills dir.
    pub ws: Option<&'a Path>,
    pub approvals_path: &'a Path,
    pub now: u64,
}

fn skills_root(ws: Option<&Path>) -> Result<PathBuf, String> {
    match ws {
        Some(w) => Ok(w.join("skills")),
        None => tole_core::paths::tole_data_dir()
            .map(|d| d.join("skills"))
            .ok_or_else(|| "the CodeCora home is unresolvable (set CODECORA_HOME or HOME)".into()),
    }
}

/// Promote a pending draft: check it, get the owner's consent to its exact
/// bytes, write the skill, approve its package hash and record where it
/// went. Returns the line to print.
pub fn promote(
    env: &PromoteEnv<'_>,
    id: &str,
    hash: Option<&str>,
    accept_taint: bool,
    interactive: bool,
    ask: impl FnOnce() -> Option<String>,
) -> Result<String, String> {
    let d = env.store.get(id)?;
    if d.status != DraftStatus::Pending {
        return Err(format!("draft {id} is already {}", status_word(d.status)));
    }
    // The record is a file on disk: its name becomes a path, so it is
    // checked again here, not trusted from the gate that wrote it.
    tole_core::skill_draft::name_ok(&d.name)?;
    let md = env.store.skill_md(id)?;
    if content_hash(md.as_bytes()) != d.content_hash {
        return Err(format!(
            "draft {id}: SKILL.md no longer matches the proposed hash {}; it was changed on disk",
            d.content_hash
        ));
    }
    match (d.dedup.verdict, &d.dedup.nearest, &d.dedup.band) {
        (Some(Verdict::Conflict), Some(n), _) => {
            return Err(format!(
                "draft {id} conflicts with {n}: it is never merged automatically; settle which                  one is right, then reject this draft"
            ))
        }
        (Some(Verdict::Patch | Verdict::Reference), Some(n), _) => {
            return Err(format!(
                "the verdict is to fold this into {n}: edit that skill by hand, then reject                  this draft"
            ))
        }
        (None, Some(n), band) if *band != Band::New => {
            return Err(format!(
                "draft {id} is close to {n} (score {:.2}, proposed {band:?}): record a verdict                  (`verdict {id} new` to keep it as its own skill) or fold it in by hand",
                d.dedup.score
            ))
        }
        _ => {}
    }
    if d.tainted() && !accept_taint {
        return Err(format!(
            "draft {id} is tainted ({}): read it with `show`, then pass --accept-taint",
            d.taint.join(", ")
        ));
    }
    if tole_core::skills::skill_file(env.ws, &d.name).is_some() {
        return Err(format!(
            "a skill named {:?} already exists: fold the draft into it by hand, then reject it",
            d.name
        ));
    }
    crate::skill_approve::consent(&d.content_hash, hash, interactive, ask)?;
    // Read before anything is written: a malformed approvals file must not
    // leave an unapproved skill behind.
    let mut approvals = Approvals::load(env.approvals_path)?;

    let root = skills_root(env.ws)?;
    // A skills dir that is a link could send the write anywhere.
    if std::fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!(
            "{} is a symlink; refusing to write there",
            root.display()
        ));
    }
    let dir = root.join(&d.name);
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    {
        use std::io::Write as _;
        let path = dir.join("SKILL.md");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(md.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // What gets approved is exactly the one file the owner confirmed: the
    // package on disk must hash like `[SKILL.md]` and nothing else, so a
    // file that appeared in the new directory meanwhile is never approved.
    let pkg = read_package(env.ws, &d.name)?;
    let expected = tole_core::skill_approval::package_hash(&[(
        "SKILL.md".to_string(),
        md.as_bytes().to_vec(),
    )]);
    if pkg.hash != expected {
        return Err(format!(
            "{}: the directory holds more than the confirmed SKILL.md (or other bytes); \
             nothing was approved, review it and run `tole skills approve {}`",
            dir.display(),
            d.name
        ));
    }
    approvals.approve(&pkg, env.now);
    approvals.save(env.approvals_path)?;
    env.store.promote(
        id,
        Promotion {
            package_hash: pkg.hash.clone(),
            dir: dir.display().to_string(),
            at: env.now,
        },
    )?;
    Ok(format!(
        "promoted {id} as skill {} in {} (approved {})\nIf that directory is git-tracked, commit it.",
        printable(&d.name),
        dir.display(),
        pkg.hash
    ))
}

/// Take back what grew from `source` (a session id, or a draft id): reject
/// its pending drafts and withdraw the approval of each promoted skill whose
/// package is still the promoted one. Returns one line per draft touched.
pub fn revoke_source(
    store: &DraftStore,
    approvals_path: &Path,
    source: &str,
) -> Result<Vec<String>, String> {
    let mut hits = store.by_session(source);
    if hits.is_empty() {
        if let Ok(d) = store.get(source) {
            hits.push(d);
        }
    }
    if hits.is_empty() {
        return Err(format!("no draft names {source:?} as a session or id"));
    }
    let mut approvals = Approvals::load(approvals_path)?;
    let mut changed = false;
    let mut lines = Vec::new();
    for d in hits {
        let id = printable(&d.id);
        match d.status {
            DraftStatus::Pending => {
                store.decide(&d.id, DraftStatus::Rejected)?;
                lines.push(format!("{id}: pending draft rejected"));
            }
            DraftStatus::Promoted => {
                let promoted = d.promotion.as_ref();
                let current = approvals.skills.get(&d.name).map(|a| a.hash.as_str());
                if promoted.is_some_and(|p| Some(p.package_hash.as_str()) == current) {
                    approvals.revoke(&d.name);
                    changed = true;
                    store.revoke(&d.id)?;
                    lines.push(format!(
                        "{id}: approval of skill {} withdrawn; it stays on disk",
                        printable(&d.name)
                    ));
                } else {
                    lines.push(format!(
                        "{id}: skill {} changed or was re-approved since promotion; left alone, \
                         review it by hand (`tole skills revoke {}`)",
                        printable(&d.name),
                        printable(&d.name)
                    ));
                }
            }
            DraftStatus::Rejected | DraftStatus::Revoked => {}
        }
    }
    if changed {
        approvals.save(approvals_path)?;
    }
    if lines.is_empty() {
        lines.push("nothing to revoke: every matching draft was already decided".into());
    }
    Ok(lines)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best effort: index the new skill when a uteke index is configured. The
/// skill is already written and approved, so a failure is only reported.
#[cfg(feature = "shell-tools")]
fn index_promoted(name: &str, ws: Option<&Path>) {
    let Some(index) = crate::skill_recall::SkillRecall::from_env(None)
        .ok()
        .and_then(|r| r.index)
    else {
        return;
    };
    let entry = tole_core::skills::load_skill(ws, name).and_then(|skill| {
        let path = tole_core::skills::skill_file(ws, name).ok_or("SKILL.md not found")?;
        let raw =
            tole_core::skills::read_file_capped(&path, tole_core::skills::MAX_SKILL_FILE_BYTES)
                .map_err(|e| e.to_string())?;
        Ok(tole_core::skill_index::SkillEntry::from_local(&skill, &raw))
    });
    match entry.and_then(|e| index.upsert(&e)) {
        Ok(()) => println!("indexed {} in the {} index", printable(name), index.label()),
        Err(e) => eprintln!("tole: could not index {}: {e}", printable(name)),
    }
}

/// Run one action against `store`.
pub fn run(action: Option<DraftsAction>, store: &DraftStore) -> anyhow::Result<()> {
    use anyhow::Context as _;
    match action.unwrap_or(DraftsAction::List { all: false }) {
        DraftsAction::List { all } => println!("{}", format_list(&store.list(), all)),
        DraftsAction::Show { id, workspace } => {
            let d = store.get(&id).map_err(anyhow::Error::msg)?;
            let md = store.skill_md(&id).map_err(anyhow::Error::msg)?;
            let ws = match workspace {
                Some(w) => PathBuf::from(w),
                None => std::env::current_dir().context("reading the current directory")?,
            };
            println!("{}", format_show(&d, &md, nearest_md(&ws, &d).as_deref()));
        }
        DraftsAction::Reject { id } => {
            store
                .decide(&id, DraftStatus::Rejected)
                .map_err(anyhow::Error::msg)?;
            println!("rejected {}", printable(&id));
        }
        DraftsAction::Verdict { id, verdict } => {
            let v: Verdict = verdict.into();
            store.set_verdict(&id, v).map_err(anyhow::Error::msg)?;
            println!("recorded {v:?} for {}", printable(&id));
        }
        DraftsAction::Promote {
            id,
            hash,
            workspace,
            accept_taint,
        } => {
            use std::io::{BufRead as _, IsTerminal as _, Write as _};
            crate::owner_only::refuse_under_session("tole skills drafts promote")
                .map_err(anyhow::Error::msg)?;
            let approvals_path = Approvals::default_path()
                .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
            let d = store.get(&id).map_err(anyhow::Error::msg)?;
            let md = store.skill_md(&id).map_err(anyhow::Error::msg)?;
            let workspace = workspace.map(PathBuf::from);
            let ws = match &workspace {
                Some(w) => w.clone(),
                None => std::env::current_dir().context("reading the current directory")?,
            };
            println!("{}", format_show(&d, &md, nearest_md(&ws, &d).as_deref()));
            let env = PromoteEnv {
                store,
                ws: workspace.as_deref(),
                approvals_path: &approvals_path,
                now: now_secs(),
            };
            let msg = promote(
                &env,
                &id,
                hash.as_deref(),
                accept_taint,
                std::io::stdin().is_terminal(),
                || {
                    print!("approve and write these exact bytes as a skill? [y/N] ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line).ok()?;
                    Some(line)
                },
            )
            .map_err(anyhow::Error::msg)?;
            println!("{msg}");
            #[cfg(feature = "shell-tools")]
            index_promoted(&d.name, workspace.as_deref());
        }
        DraftsAction::Revoke { source } => {
            let approvals_path = Approvals::default_path()
                .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
            for line in
                revoke_source(store, &approvals_path, &source).map_err(anyhow::Error::msg)?
            {
                println!("{line}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "skill_drafts_cli_tests.rs"]
mod tests;
