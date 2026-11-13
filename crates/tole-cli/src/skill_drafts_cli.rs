//! `tole skills drafts` (#592 part 2): the owner's side of skill growth.
//!
//! A draft written by `skill_propose` waits here. The owner lists, reads,
//! rejects and records a reviewer's verdict; nothing on this surface makes a
//! skill loadable (promotion is a separate, later step).
//!
//! Everything shown came from a model, so it is printed through
//! [`printable`]: a control or invisible character is shown as `\u{..}`
//! instead of reaching the terminal.

use std::path::{Path, PathBuf};

use tole_core::skill_approval::{format_time, read_package};
use tole_core::skill_draft::{is_hidden, Draft, DraftStatus, DraftStore, Severity, Verdict};

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
        workspace: Option<PathBuf>,
    },
    /// Reject a pending draft. It stays on disk, out of the pending list.
    Reject { id: String },
    /// Record a reviewer's call on the draft's nearest skill. `conflict`
    /// means similar but contradictory: never merged.
    Verdict { id: String, verdict: VerdictArg },
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

/// Run one action against `store`.
pub fn run(action: Option<DraftsAction>, store: &DraftStore) -> anyhow::Result<()> {
    use anyhow::Context as _;
    match action.unwrap_or(DraftsAction::List { all: false }) {
        DraftsAction::List { all } => println!("{}", format_list(&store.list(), all)),
        DraftsAction::Show { id, workspace } => {
            let d = store.get(&id).map_err(anyhow::Error::msg)?;
            let md = store.skill_md(&id).map_err(anyhow::Error::msg)?;
            let ws = match workspace {
                Some(w) => w,
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
    }
    Ok(())
}

#[cfg(test)]
#[path = "skill_drafts_cli_tests.rs"]
mod tests;
