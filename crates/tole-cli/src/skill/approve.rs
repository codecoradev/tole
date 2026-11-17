//! Owner approval of skill content (#591): the `TOLE_SKILL_APPROVAL`
//! setting and `tole skills approve|revoke|list`.
//!
//! - `required` (the default, owner decision 2026-11-11): a session loads a
//!   skill only when its package (SKILL.md plus every other file of its
//!   directory) hashes to the owner's approval in
//!   `$CODECORA_HOME/tole/skill-approvals.json`. Any other skill is listed
//!   by name as unavailable and `load_skill` refuses it. A malformed
//!   approvals file fails startup.
//! - `TOLE_SKILL_APPROVAL=off`: every skill on disk loads (the pre-default
//!   behavior). Only the environment can opt out: there is no config key,
//!   and the config schema refuses unknown keys, so a project config can
//!   never loosen the gate.
//!
//! Skills the owner names directly (`--skill <file>`, a profile's `skills`)
//! are not gated: naming the file is the approval.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tole_core::skill_approval::{format_time, read_package, Approvals, Gate, Source};

pub const APPROVAL_ENV: &str = "TOLE_SKILL_APPROVAL";

/// The session's gate from `TOLE_SKILL_APPROVAL`, reading the approvals at
/// `path` (`None` = the CodeCora home is unresolvable: nothing is
/// approved). Returns the gate and the startup notes.
pub fn gate_from(
    env: &dyn Fn(&str) -> Option<String>,
    path: Option<&Path>,
) -> Result<(Gate, Vec<String>), String> {
    let mode = env(APPROVAL_ENV).map(|v| v.trim().to_string());
    match mode.as_deref() {
        Some("off") => Ok((Gate::Open, Vec::new())),
        None | Some("") | Some("required") => match path {
            Some(p) => {
                let (approvals, notes) = load_checked(p)?;
                Ok((Gate::Approved(Arc::new(approvals)), notes))
            }
            None => Ok((
                Gate::Approved(Arc::new(Approvals::default())),
                vec![format!(
                    "skill approval is required but the CodeCora home is unresolvable: \
                     no skill can be approved (set CODECORA_HOME or HOME, or \
                     {APPROVAL_ENV}=off)"
                )],
            )),
        },
        Some(v) => Err(format!("{APPROVAL_ENV}={v:?}: expected off or required")),
    }
}

/// What [`load_verified`] found.
#[derive(Debug)]
pub struct Loaded {
    /// The approvals that may load (empty after a ledger fault).
    pub approvals: Approvals,
    /// Startup notes, one line each.
    pub notes: Vec<String>,
    /// The ledger fails its checkpoint (#879): why, with the remedy. Every
    /// approval is ignored and no owner command may write until the owner
    /// accepts the ledger (`tole skills recheckpoint`).
    pub ledger_fault: Option<String>,
}

/// The approvals at `path` minus every approval the append-only skill
/// ledger next to it does not allow (a revoke recorded after it, or another
/// hash): a hand-edited approvals file cannot approve what the ledger shows
/// the owner revoked or never approved at that hash (#600). One note per
/// dropped approval. An unreadable ledger is an error, never "no history".
/// A ledger that fails the checkpoint in the approvals file (#879: deleted,
/// truncated or rewritten) makes every approval blocked, with one error
/// note naming what is wrong and the remedy.
pub fn load_verified(path: &Path) -> Result<Loaded, String> {
    use tole_core::skill_approval as sa;
    let (mut approvals, checkpoint) = Approvals::load_with_checkpoint(path)?;
    // One read of the ledger: the same bytes are verified and parsed.
    let raw = sa::read_ledger_bytes(path)?;
    let one_line = |s: &str| tole_core::plugin_lock::terminal_safe(s).replace(['\n', '\t'], " ");
    if let Some(why) = checkpoint.as_ref().and_then(|c| c.verify(&raw).err()) {
        let msg = sa::fault_message(path, &why);
        return Ok(Loaded {
            approvals: Approvals::default(),
            notes: vec![one_line(&format!("ERROR: {msg}"))],
            ledger_fault: Some(msg),
        });
    }
    let ledger = String::from_utf8_lossy(&raw);
    let check = sa::check_against_ledger(&approvals, &ledger);
    let mut notes = Vec::new();
    for (name, why) in &check.blocked {
        approvals.skills.remove(name);
        notes.push(one_line(&format!(
            "skill {name:?}: approval ignored, {why}; review it and run `tole skills approve {name}`"
        )));
    }
    // Still loaded: approved before the ledger existed, or the ledger was
    // deleted (which forgets every revoke). Said once, not per skill.
    if !check.unrecorded.is_empty() {
        notes.push(one_line(&format!(
            "{} approved skill(s) are not in the skill ledger ({}): they still load; \
             `tole skills adopt --all` records them (or `tole skills approve <name>` one)",
            check.unrecorded.len(),
            check.unrecorded.join(", ")
        )));
    }
    // Only worth saying when there is a ledger whose deletion would go
    // unnoticed.
    if checkpoint.is_none() && !raw.is_empty() && !approvals.skills.is_empty() {
        notes.push(
            "the skill approvals carry no ledger checkpoint (written before #879): deleting the \
             ledger would go unnoticed; the next `tole skills approve|revoke|adopt` adds one"
                .into(),
        );
    }
    Ok(Loaded {
        approvals,
        notes,
        ledger_fault: None,
    })
}

/// [`load_verified`] as `(approvals, notes)`; after a ledger fault the
/// approvals are empty and the error is the one note.
pub fn load_checked(path: &Path) -> Result<(Approvals, Vec<String>), String> {
    load_verified(path).map(|l| (l.approvals, l.notes))
}

/// Appends one line to the skill ledger next to `approvals_path`. The
/// ledger is written BEFORE the approvals file, so a crash in between leaves
/// a skill unapproved, never approved without a record.
///
/// A ledger whose last line was cut short (a crash in the middle of a write)
/// does not end in a newline: a newline is written first, so the torn
/// fragment stays one unparseable line and the new line is its own.
pub(crate) fn record(approvals_path: &Path, line: &str) -> Result<(), String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use tole_core::ledger::LedgerStore as _;
    let dir = approvals_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(tole_core::skill_approval::LEDGER_FILE);
    let torn = std::fs::File::open(&path).is_ok_and(|mut f| {
        let mut last = [0u8; 1];
        f.seek(SeekFrom::End(-1)).is_ok() && f.read_exact(&mut last).is_ok() && last[0] != b'\n'
    });
    let mut text = String::new();
    if torn {
        text.push('\n');
    }
    text.push_str(line);
    crate::ledger_file::FileStore::new(path)
        .append(text.as_bytes())
        .map_err(|e| format!("skill ledger: {e}"))
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How many discovered skills `gate` refuses as not approved.
pub fn unapproved_count(workspace: Option<&Path>, gate: &Gate) -> usize {
    if *gate == Gate::Open {
        return 0;
    }
    tole_core::skills::skill_names(workspace)
        .iter()
        .filter(|n| {
            matches!(
                gate.load(workspace, n),
                Err(tole_core::skills::LoadError::NotApproved { .. })
            )
        })
        .count()
}

/// The fix an owner runs for unapproved skills.
pub const APPROVE_HINT: &str = "tole skills list; tole skills approve <name> --hash <h>";

/// The one startup note for unapproved skills (`None` when there are
/// none). It never names a description.
pub fn startup_note(workspace: Option<&Path>, gate: &Gate) -> Option<String> {
    match unapproved_count(workspace, gate) {
        0 => None,
        n => Some(format!(
            "{n} skill(s) unavailable until approved ({APPROVE_HINT})"
        )),
    }
}

/// `tole doctor` lines for skill approval.
pub fn doctor_lines(
    env: &dyn Fn(&str) -> Option<String>,
    path: Option<&Path>,
    workspace: Option<&Path>,
) -> Vec<String> {
    let gate = match gate_from(env, path) {
        Ok((g, _)) => g,
        Err(e) => return vec![format!("  ! skills: {e}")],
    };
    if gate == Gate::Open {
        return vec![format!(
            "  skills: approval off ({APPROVAL_ENV}=off): every discovered skill loads"
        )];
    }
    if path.is_none() {
        return vec![format!(
            "  ! skills: approval is required but the CodeCora home is unresolvable, so no \
             skill can ever be approved (set CODECORA_HOME or HOME, or {APPROVAL_ENV}=off)"
        )];
    }
    let total = tole_core::skills::skill_names(workspace).len();
    match unapproved_count(workspace, &gate) {
        0 => vec![format!(
            "  skills: approval required, all {total} discovered skill(s) approved"
        )],
        n => vec![format!(
            "  ! skills: {n} skill(s) unavailable until approved ({APPROVE_HINT})"
        )],
    }
}

/// [`gate_from`] with the process environment and the default approvals
/// path.
pub fn gate_from_env() -> Result<(Gate, Vec<String>), String> {
    gate_from(
        &crate::config_apply::real_env,
        Approvals::default_path().as_deref(),
    )
}

/// `tole skills <action>`: the owner's approvals of skill content, and in
/// shell-tools builds the uteke skill index (#590).
#[derive(Debug, clap::Subcommand)]
pub enum SkillsAction {
    #[cfg(feature = "shell-tools")]
    #[command(flatten)]
    Index(crate::skill_recall::IndexAction),
    /// Approve a skill as it is on disk now: shows every file in its
    /// directory with its size and the package hash, then asks. Without a
    /// terminal (a script), --hash is required. Any later change to any
    /// file needs a new approval.
    Approve {
        name: String,
        /// Approve only if the content hash is still this one (the hash
        /// `tole skills list` showed when you reviewed it).
        #[arg(long)]
        hash: Option<String>,
        /// Discovery root (default: the current directory).
        #[arg(long)]
        workspace: Option<String>,
        /// Where a third-party skill was pulled from. Recorded as data in
        /// the approval and the ledger; never fetched or verified.
        #[arg(long)]
        source_url: Option<String>,
        /// The skill's id in the registry that listed it (data only).
        #[arg(long)]
        registry_id: Option<String>,
        /// The commit the skill was pinned at, 40 or 64 lowercase hex
        /// digits (data only: the approved hash is what is enforced).
        #[arg(long)]
        pinned_sha: Option<String>,
    },
    /// Skill drafts the agent proposed with `skill_propose` (#592): list,
    /// show, reject, record a reviewer's verdict. A draft never loads.
    Drafts {
        #[command(subcommand)]
        action: Option<crate::skill_drafts_cli::DraftsAction>,
    },
    /// Remove a skill's approval; it stays on disk.
    Revoke { name: String },
    /// Record in the skill ledger the approvals it does not name (made
    /// before the ledger existed, or while it was missing), as they are
    /// stored, and refresh the ledger checkpoint. Shows what it adopts;
    /// skills the ledger already records are left alone. Owner only.
    Adopt {
        /// Skills to adopt (default: none; use --all for every unrecorded one).
        #[arg(required_unless_present = "all")]
        names: Vec<String>,
        /// Adopt every approval the ledger does not name.
        #[arg(long, conflicts_with = "names")]
        all: bool,
    },
    /// After the skill ledger failed its checkpoint (deleted, truncated or
    /// rewritten): show the ledger as it is and, once you confirm, record it
    /// as the new checkpoint. It brings back no revoke the old ledger held:
    /// review `tole skills list` after. Owner only.
    Recheckpoint {
        /// Confirm without a prompt: the `sha256:` digest shown for the
        /// ledger you inspected (refused if the ledger is different now).
        #[arg(long)]
        sha256: Option<String>,
    },
    /// Every discovered skill with its approval status, content hash and
    /// approval time.
    List {
        /// Discovery root (default: the current directory).
        #[arg(long)]
        workspace: Option<String>,
    },
}

/// The status of one skill against the approvals, for `list`.
pub fn status_line(ws: Option<&Path>, name: &str, approvals: &Approvals) -> String {
    let pkg = match read_package(ws, name) {
        Ok(p) => p,
        Err(e) => return format!("{name}  unreadable  {e}"),
    };
    let from = approvals
        .skills
        .get(name)
        .map(|a| source_text(a.source.as_ref()))
        .unwrap_or_default();
    match approvals.skills.get(name) {
        Some(a) if a.hash == pkg.hash => format!(
            "{name}  approved  {}  {}{from}",
            pkg.hash,
            format_time(a.approved_at)
        ),
        Some(a) => format!(
            "{name}  changed since approval  {}  (approved {} at {})",
            pkg.hash,
            a.hash,
            format_time(a.approved_at)
        ),
        None => format!("{name}  not approved  {}", pkg.hash),
    }
}

/// `  source <url> registry <id> pinned <sha>` of a recorded source (empty
/// when none), terminal-safe: it is text from a file.
pub fn source_text(s: Option<&Source>) -> String {
    let Some(s) = s.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let mut out = String::new();
    for (label, v) in [
        ("source", &s.url),
        ("registry", &s.registry_id),
        ("pinned", &s.sha),
    ] {
        if let Some(v) = v {
            out.push_str(&format!("  {label} {v}"));
        }
    }
    tole_core::plugin_lock::terminal_safe(&out).replace(['\n', '\t'], " ")
}

/// One audit row (`tole plugin audit`) per discovered skill: status, the
/// package hash on disk against the approved one, the stated source and the
/// last ledger lines. Read-only; unreadable approvals or ledger are an error.
pub fn audit_rows(ws: Option<&Path>) -> Result<Vec<serde_json::Value>, String> {
    let path = Approvals::default_path()
        .ok_or("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
    audit_rows_at(ws, &path)
}

fn audit_rows_at(ws: Option<&Path>, path: &Path) -> Result<Vec<serde_json::Value>, String> {
    use tole_core::skill_approval as sa;
    // A ledger that fails its checkpoint is an error, not a report built on
    // a ledger that lost history.
    sa::verify_checkpoint_at(path)?;
    let approvals = Approvals::load(path)?;
    let ledger = sa::read_ledger(path)?;
    let check = sa::check_against_ledger(&approvals, &ledger);
    let unrecorded = check.unrecorded;
    let blocked: std::collections::BTreeMap<String, String> = check.blocked.into_iter().collect();
    let mut rows = Vec::new();
    for name in tole_core::skills::skill_names(ws) {
        let pkg = read_package(ws, &name);
        let approved = approvals.skills.get(&name);
        let status = match (&pkg, approved) {
            (Err(e), _) => format!("unreadable: {e}"),
            (Ok(_), Some(_)) if blocked.contains_key(&name) => {
                format!("refused: approval ignored, {}", blocked[&name])
            }
            (Ok(p), Some(a)) if a.hash == p.hash && unrecorded.contains(&name) => {
                "approved, unrecorded in the ledger (loads; approve again to record it)".to_string()
            }
            (Ok(p), Some(a)) if a.hash == p.hash => "approved".to_string(),
            (Ok(_), Some(_)) => "changed since approval: refused until approved again".to_string(),
            (Ok(_), None) => "not approved: refused".to_string(),
        };
        rows.push(serde_json::json!({
            "name": name,
            "status": status,
            "hash": pkg.as_ref().ok().map(|p| p.hash.clone()),
            "approved_hash": approved.map(|a| a.hash.clone()),
            "approved_at": approved.map(|a| format_time(a.approved_at)),
            "source": approved.and_then(|a| a.source.clone()),
            "source_text": source_text(approved.and_then(|a| a.source.as_ref())),
            "history": sa::ledger_history(&ledger, &name, 3)
                .into_iter()
                .map(|(at, event, hash)| serde_json::json!({
                    "at": format_time(at), "event": event, "hash": hash,
                }))
                .collect::<Vec<_>>(),
        }));
    }
    Ok(rows)
}

/// Whether the owner consents to approving a package with hash `hash`:
/// `--hash` must match it; without `--hash` an interactive owner is asked
/// (`ask` returns their answer) and a non-interactive run is refused.
pub fn consent(
    hash: &str,
    expect: Option<&str>,
    interactive: bool,
    ask: impl FnOnce() -> Option<String>,
) -> Result<(), String> {
    match expect.map(str::trim) {
        Some(want) if want == hash => Ok(()),
        Some(want) => Err(format!(
            "the package is now {hash}, not the reviewed {want}: review it again"
        )),
        None if !interactive => Err(format!(
            "stdin is not a terminal, so nobody can confirm: pass --hash {hash} \
             (the hash `tole skills list` shows) after reviewing the files above"
        )),
        None => match ask().as_deref().map(str::trim) {
            Some("y" | "Y" | "yes" | "YES" | "Yes") => Ok(()),
            _ => Err("not approved".into()),
        },
    }
}

/// Print the package the owner is about to approve: directory, each file
/// with its size, and the hash.
fn show_package(pkg: &tole_core::skill_approval::SkillPackage) {
    println!("skill {} in {}", pkg.name, pkg.dir.display());
    for (f, bytes) in &pkg.files {
        println!("  {f} ({} bytes)", bytes.len());
    }
    println!("hash {}", pkg.hash);
}

/// Approve `name` in `approvals` (not saved). Refuses when `expect` is
/// given and is not the hash on disk.
pub fn approve(
    ws: Option<&Path>,
    name: &str,
    expect: Option<&str>,
    approvals: &mut Approvals,
    now: u64,
) -> Result<tole_core::skill_approval::SkillPackage, String> {
    approve_from(ws, name, expect, approvals, now, None)
}

/// [`approve`] recording the stated source of a third-party skill (checked,
/// stored as data, never fetched).
pub fn approve_from(
    ws: Option<&Path>,
    name: &str,
    expect: Option<&str>,
    approvals: &mut Approvals,
    now: u64,
    source: Option<Source>,
) -> Result<tole_core::skill_approval::SkillPackage, String> {
    if let Some(s) = &source {
        s.validate()?;
    }
    let pkg = read_package(ws, name)?;
    if let Some(want) = expect.map(str::trim) {
        if want != pkg.hash {
            return Err(format!(
                "skill {name:?} is now {}, not the reviewed {want}: review it again",
                pkg.hash
            ));
        }
    }
    tole_core::skills::parse_skill(
        std::str::from_utf8(pkg.skill_md()).map_err(|_| "SKILL.md is not UTF-8".to_string())?,
        name,
    )?;
    approvals.approve_from(&pkg, now, source);
    Ok(pkg)
}

/// `tole skills adopt` (#879): appends an `approve` line (hash and source as
/// stored, noted as adopted) for each approval the ledger does not name that
/// the owner selected, then refreshes the checkpoint. Skills the ledger
/// already records are not touched. Returns the lines to print.
pub fn adopt(
    path: &Path,
    approvals: &Approvals,
    names: &[String],
    all: bool,
    now: u64,
) -> Result<Vec<String>, String> {
    use tole_core::skill_approval as sa;
    let ledger = sa::read_ledger(path)?;
    let unrecorded = sa::check_against_ledger(approvals, &ledger).unrecorded;
    let mut out = Vec::new();
    let chosen: Vec<&String> = if all {
        unrecorded.iter().collect()
    } else {
        if let Some(n) = names.iter().find(|n| !approvals.skills.contains_key(*n)) {
            return Err(format!("skill {n:?} has no approval to adopt"));
        }
        let mut c = Vec::new();
        for n in names {
            if !unrecorded.contains(n) {
                out.push(format!("{n}: already recorded in the ledger, left alone"));
            } else if !c.contains(&n) {
                c.push(n);
            }
        }
        c
    };
    let mut text = String::new();
    for name in &chosen {
        let a = &approvals.skills[*name];
        out.push(format!(
            "adopt {name}  {}  approved {}{}",
            a.hash,
            format_time(a.approved_at),
            source_text(a.source.as_ref())
        ));
        text.push_str(&sa::adopted_ledger_line(
            name,
            &a.hash,
            a.source.as_ref(),
            now,
        ));
    }
    let mut out: Vec<String> = out
        .into_iter()
        .map(|l| tole_core::plugin_lock::terminal_safe(&l).replace(['\n', '\t'], " "))
        .collect();
    // The ledger first, then the approvals file with the new checkpoint.
    if !text.is_empty() {
        record(path, &text)?;
    }
    if !chosen.is_empty() || !approvals.skills.is_empty() {
        approvals.save_checkpointed(path)?;
        out.push(format!(
            "adopted {} skill(s); ledger checkpoint refreshed",
            chosen.len()
        ));
    } else {
        out.push("nothing to adopt".into());
    }
    Ok(out)
}

/// What `tole skills recheckpoint` shows before it asks: the ledger as it
/// is (path, size, digest), whether the old checkpoint holds, and what the
/// ledger says about each approval. Returns the digest to confirm.
fn review_ledger(
    path: &Path,
) -> Result<(tole_core::skill_approval::LedgerCheckpoint, Vec<String>), String> {
    use tole_core::skill_approval as sa;
    if !path.exists() {
        return Err(format!(
            "{}: no skill approvals recorded, nothing to checkpoint",
            path.display()
        ));
    }
    let (approvals, old) = Approvals::load_with_checkpoint(path)?;
    let raw = sa::read_ledger_bytes(path)?;
    let cp = sa::LedgerCheckpoint::of(&raw);
    let text = String::from_utf8_lossy(&raw);
    let check = sa::check_against_ledger(&approvals, &text);
    let mut lines = vec![
        format!(
            "skill ledger {}: {} bytes, {} line(s), {}",
            sa::ledger_path(path).display(),
            cp.bytes,
            text.lines().count(),
            cp.sha256
        ),
        match old.as_ref().map(|o| o.verify(&raw)) {
            None => "checkpoint: none recorded yet".to_string(),
            Some(Ok(())) => "checkpoint: holds (nothing to accept)".to_string(),
            Some(Err(why)) => format!("checkpoint: FAILS, the ledger {why}"),
        },
        format!(
            "{} approval(s); the ledger blocks {}, does not name {}",
            approvals.skills.len(),
            check.blocked.len(),
            check.unrecorded.len()
        ),
    ];
    lines.extend(
        check
            .blocked
            .iter()
            .map(|(n, why)| format!("  blocked {n}: {why}")),
    );
    lines.extend(check.unrecorded.iter().map(|n| format!("  unrecorded {n}")));
    let lines = lines
        .into_iter()
        .map(|l| tole_core::plugin_lock::terminal_safe(&l).replace(['\n', '\t'], " "))
        .collect();
    Ok((cp, lines))
}

pub fn cli(action: SkillsAction) -> anyhow::Result<()> {
    use anyhow::Context as _;
    // `write`: the caller is about to change the approvals, so a ledger that
    // fails its checkpoint (#879) is an error, never a quiet re-record.
    let store = |write: bool| -> anyhow::Result<(PathBuf, Approvals)> {
        let path = Approvals::default_path()
            .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
        let loaded = load_verified(&path).map_err(anyhow::Error::msg)?;
        if let (true, Some(fault)) = (write, &loaded.ledger_fault) {
            anyhow::bail!("{fault}");
        }
        for n in loaded.notes {
            eprintln!("tole: {n}");
        }
        Ok((path, loaded.approvals))
    };
    let root = |ws: Option<String>| -> anyhow::Result<PathBuf> {
        match ws {
            Some(p) => Ok(PathBuf::from(p)),
            None => std::env::current_dir().context("reading the current directory"),
        }
    };
    match action {
        #[cfg(feature = "shell-tools")]
        SkillsAction::Index(a) => return crate::skill_recall::cli(a),
        SkillsAction::Approve {
            name,
            hash,
            workspace,
            source_url,
            registry_id,
            pinned_sha,
        } => {
            use std::io::{BufRead as _, IsTerminal as _, Write as _};
            crate::owner_only::refuse_under_session("tole skills approve")
                .map_err(anyhow::Error::msg)?;
            let ws = root(workspace)?;
            let (path, mut approvals) = store(true)?;
            // Show what would be approved first, then ask (or require the
            // reviewed --hash when nobody is at a terminal).
            let shown = read_package(Some(&ws), &name).map_err(anyhow::Error::msg)?;
            show_package(&shown);
            let interactive = std::io::stdin().is_terminal();
            consent(&shown.hash, hash.as_deref(), interactive, || {
                print!("approve these exact bytes? [y/N] ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).ok()?;
                Some(line)
            })
            .map_err(anyhow::Error::msg)?;
            let now = now();
            let source = Source {
                url: source_url,
                registry_id,
                sha: pinned_sha,
            };
            // Approve exactly what was shown: the package read again must
            // still hash to it.
            let pkg = approve_from(
                Some(&ws),
                &name,
                Some(&shown.hash),
                &mut approvals,
                now,
                Some(source),
            )
            .map_err(anyhow::Error::msg)?;
            // The ledger first: a crash between the two leaves the skill
            // unapproved, never approved without a record.
            let src = approvals.skills.get(&name).and_then(|a| a.source.as_ref());
            record(
                &path,
                &tole_core::skill_approval::ledger_line("approve", &name, &pkg.hash, src, now),
            )
            .map_err(anyhow::Error::msg)?;
            approvals
                .save_checkpointed(&path)
                .map_err(anyhow::Error::msg)?;
            println!("approved {name} ({})", pkg.hash);
        }
        SkillsAction::Drafts { action } => {
            let store = tole_core::skill_draft::DraftStore::default_store()
                .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
            return crate::skill_drafts_cli::run(action, &store);
        }
        SkillsAction::Revoke { name } => {
            crate::owner_only::refuse_under_session("tole skills revoke")
                .map_err(anyhow::Error::msg)?;
            let (path, mut approvals) = store(true)?;
            let Some(hash) = approvals.skills.get(&name).map(|a| a.hash.clone()) else {
                anyhow::bail!("skill {name:?} has no approval");
            };
            record(
                &path,
                &tole_core::skill_approval::ledger_line("revoke", &name, &hash, None, now()),
            )
            .map_err(anyhow::Error::msg)?;
            approvals.revoke(&name);
            approvals
                .save_checkpointed(&path)
                .map_err(anyhow::Error::msg)?;
            println!(
                "revoked {name}: it stays on disk and no longer loads when approval is required"
            );
        }
        SkillsAction::Adopt { names, all } => {
            crate::owner_only::refuse_under_session("tole skills adopt")
                .map_err(anyhow::Error::msg)?;
            let (path, approvals) = store(true)?;
            for l in adopt(&path, &approvals, &names, all, now()).map_err(anyhow::Error::msg)? {
                println!("{l}");
            }
        }
        SkillsAction::Recheckpoint { sha256 } => {
            use std::io::{BufRead as _, IsTerminal as _, Write as _};
            crate::owner_only::refuse_under_session("tole skills recheckpoint")
                .map_err(anyhow::Error::msg)?;
            let path = Approvals::default_path()
                .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
            let (cp, lines) = review_ledger(&path).map_err(anyhow::Error::msg)?;
            for l in lines {
                println!("{l}");
            }
            let interactive = std::io::stdin().is_terminal();
            match sha256.as_deref().map(str::trim) {
                Some(want) if want == cp.sha256 => {}
                Some(want) => anyhow::bail!(
                    "the ledger is now {}, not the reviewed {want}: review it again",
                    cp.sha256
                ),
                None if !interactive => anyhow::bail!(
                    "stdin is not a terminal, so nobody can confirm: pass --sha256 {} after \
                     inspecting the ledger above",
                    cp.sha256
                ),
                None => {
                    print!("record this ledger as the checkpoint? [y/N] ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line).ok();
                    if !matches!(line.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
                        anyhow::bail!("not recorded");
                    }
                }
            }
            Approvals::accept_ledger(&path, &cp).map_err(anyhow::Error::msg)?;
            println!(
                "ledger checkpoint recorded; run `tole skills list` and re-approve what you need"
            );
        }
        SkillsAction::List { workspace } => {
            let ws = root(workspace)?;
            let (_, approvals) = store(false)?;
            let names = tole_core::skills::skill_names(Some(&ws));
            if names.is_empty() {
                println!("no skills found");
            }
            for name in names {
                println!("{}", status_line(Some(&ws), &name, &approvals));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(v: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
        move |k| (k == APPROVAL_ENV).then(|| v.map(String::from)).flatten()
    }

    use crate::test_dir::TestDir;
    fn ws(tag: &str) -> TestDir {
        let d = TestDir::new(&format!("tole-skapprove-cli-{tag}"));
        let s = d.join("skills").join("demo");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(
            s.join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\n---\nb",
        )
        .unwrap();
        d
    }

    #[test]
    fn the_setting_is_required_by_default_and_off_is_explicit() {
        let dir = ws("env");
        let path = dir.join("approvals.json");
        // Owner decision 2026-11-11: required unless the environment opts
        // out explicitly.
        for unset in [None, Some(""), Some(" ")] {
            assert_eq!(
                gate_from(&env(unset), Some(&path)).unwrap().0,
                Gate::Approved(Arc::new(Approvals::default())),
                "{unset:?}"
            );
        }
        assert_eq!(
            gate_from(&env(Some(" off ")), Some(&path)).unwrap().0,
            Gate::Open
        );
        let (gate, notes) = gate_from(&env(Some("required")), Some(&path)).unwrap();
        assert_eq!(gate, Gate::Approved(Arc::new(Approvals::default())));
        assert!(notes.is_empty());
        // No CodeCora home: required, nothing approved, with a note.
        let (gate, notes) = gate_from(&env(Some("required")), None).unwrap();
        assert_eq!(gate, Gate::Approved(Arc::new(Approvals::default())));
        assert!(notes[0].contains("unresolvable"), "{notes:?}");
        // A bad value or a malformed file fails startup.
        assert!(gate_from(&env(Some("yes")), Some(&path)).is_err());
        std::fs::write(&path, "nope").unwrap();
        assert!(gate_from(&env(Some("required")), Some(&path)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only the environment can opt out: no config key exists for it, and
    /// the config schema refuses unknown keys, so a project config cannot
    /// loosen the gate.
    #[test]
    fn a_project_config_cannot_turn_approval_off() {
        let p = Path::new("/repo/.tole/config.toml");
        for src in [
            "skill_approval = \"off\"\n",
            "skills_approval = \"off\"\n",
            "[skills]\napproval = \"off\"\n",
            "TOLE_SKILL_APPROVAL = \"off\"\n",
        ] {
            assert!(crate::config::parse(src, p).is_err(), "{src}");
        }
    }

    /// Migration UX: one note with the count and the fix, no description.
    #[test]
    fn unapproved_skills_get_one_note_and_a_doctor_line() {
        let dir = ws("note");
        let s = dir.join("skills").join("other");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(
            s.join("SKILL.md"),
            "---\nname: other\ndescription: \"SECRET PLAN\"\n---\nb",
        )
        .unwrap();
        let mut a = Approvals::default();
        a.approve(&read_package(Some(&dir), "demo").unwrap(), 1);
        let gate = Gate::Approved(Arc::new(a.clone()));
        assert_eq!(unapproved_count(Some(&dir), &gate), 1);
        let note = startup_note(Some(&dir), &gate).unwrap();
        assert_eq!(
            note,
            "1 skill(s) unavailable until approved \
             (tole skills list; tole skills approve <name> --hash <h>)"
        );
        assert_eq!(startup_note(Some(&dir), &Gate::Open), None);
        a.approve(&read_package(Some(&dir), "other").unwrap(), 1);
        assert_eq!(
            startup_note(Some(&dir), &Gate::Approved(Arc::new(a.clone()))),
            None
        );
        // doctor: the same count and hint; a warning when nothing can ever
        // be approved; the explicit opt-out is reported.
        let path = dir.join("approvals.json");
        let lines = doctor_lines(&env(None), Some(&path), Some(&dir)).join("\n");
        assert!(
            lines.contains("! skills: 2 skill(s) unavailable until approved"),
            "{lines}"
        );
        assert!(!lines.contains("SECRET"), "{lines}");
        let lines = doctor_lines(&env(None), None, Some(&dir)).join("\n");
        assert!(
            lines.contains("! skills:") && lines.contains("unresolvable"),
            "{lines}"
        );
        let lines = doctor_lines(&env(Some("off")), Some(&path), Some(&dir)).join("\n");
        assert!(lines.contains("TOLE_SKILL_APPROVAL=off"), "{lines}");
        a.save(&path).unwrap();
        let lines = doctor_lines(&env(None), Some(&path), Some(&dir)).join("\n");
        assert!(
            lines.contains("all 2 discovered skill(s) approved"),
            "{lines}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review note on #617: approving without `--hash` needs an owner at
    /// a terminal who says yes; a script must pass the reviewed hash.
    #[test]
    fn approval_needs_the_reviewed_hash_or_an_interactive_yes() {
        let h = "sha256:aa";
        let never = || -> Option<String> { panic!("must not prompt") };
        assert!(consent(h, Some(h), false, never).is_ok());
        assert!(consent(h, Some(h), true, never).is_ok());
        assert!(consent(h, Some("sha256:bb"), false, never).is_err());
        let err = consent(h, None, false, never).unwrap_err();
        assert!(err.contains("--hash"), "{err}");
        for yes in ["y", "Y", "yes", " yes\n"] {
            assert!(consent(h, None, true, || Some(yes.into())).is_ok(), "{yes}");
        }
        for no in ["", "n", "no", "maybe"] {
            assert!(consent(h, None, true, || Some(no.into())).is_err(), "{no}");
        }
        assert!(consent(h, None, true, || None).is_err(), "EOF is a no");
    }

    #[test]
    fn approve_pins_the_reviewed_hash_and_list_shows_each_status() {
        let dir = ws("approve");
        let mut a = Approvals::default();
        assert!(status_line(Some(&dir), "demo", &a).starts_with("demo  not approved  sha256:"));
        // The reviewed hash no longer matches: refused, nothing recorded.
        let err = approve(Some(&dir), "demo", Some("sha256:00"), &mut a, 1).unwrap_err();
        assert!(err.contains("review it again"), "{err}");
        assert!(a.skills.is_empty());
        let hash = read_package(Some(&dir), "demo").unwrap().hash;
        approve(Some(&dir), "demo", Some(&hash), &mut a, 1_700_000_000).unwrap();
        assert_eq!(
            status_line(Some(&dir), "demo", &a),
            format!("demo  approved  {hash}  2023-11-14 22:13 UTC")
        );
        std::fs::write(dir.join("skills/demo/extra.md"), "x").unwrap();
        assert!(status_line(Some(&dir), "demo", &a).contains("changed since approval"));
        // A broken SKILL.md is never approved.
        std::fs::write(dir.join("skills/demo/SKILL.md"), "no frontmatter").unwrap();
        assert!(approve(Some(&dir), "demo", None, &mut a, 2).is_err());
        assert_eq!(a.skills["demo"].hash, hash);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An approval the ledger never names still loads, but is said so once
    /// at load and in the audit; a torn last ledger line is repaired by the
    /// next append; a hash with a newline cannot make a two-line note.
    #[test]
    fn unrecorded_approvals_are_reported_a_torn_tail_is_repaired_and_notes_stay_one_line() {
        let dir = ws("unrecorded");
        let path = dir.join("approvals.json");
        let mut a = Approvals::default();
        let pkg = approve_from(Some(&dir), "demo", None, &mut a, 1, None).unwrap();
        a.save_checkpointed(&path).unwrap();
        let (g, notes) = gate_from(&env(None), Some(&path)).unwrap();
        assert!(g.load(Some(&dir), "demo").is_ok(), "still loads");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("not in the skill ledger"), "{notes:?}");
        let rows = audit_rows_at(Some(&dir), &path).unwrap();
        assert!(
            rows[0]["status"]
                .as_str()
                .unwrap()
                .starts_with("approved, unrecorded"),
            "{rows:?}"
        );
        // A crash cut the ledger mid-line: the next record starts a new line.
        let ledger = dir.join(tole_core::skill_approval::LEDGER_FILE);
        std::fs::write(&ledger, "{\"v\":1,\"kind\":\"skill\",\"event\":\"app").unwrap();
        let line = tole_core::skill_approval::ledger_line("approve", "demo", &pkg.hash, None, 2);
        record(&path, &line).unwrap();
        let text = std::fs::read_to_string(&ledger).unwrap();
        assert_eq!(text.lines().count(), 2, "{text:?}");
        assert!(text.ends_with(&line), "{text:?}");
        assert!(gate_from(&env(None), Some(&path)).unwrap().1.is_empty());
        // A clean ledger gets no extra blank line.
        record(&path, &line).unwrap();
        assert_eq!(std::fs::read_to_string(&ledger).unwrap().lines().count(), 3);
        // The ledger's own hash is flattened in the note.
        std::fs::write(
            &ledger,
            tole_core::skill_approval::ledger_line("approve", "demo", "sha256:a\nb\tc", None, 3),
        )
        .unwrap();
        let notes = gate_from(&env(None), Some(&path)).unwrap().1;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(!notes[0].contains(['\n', '\t']), "{notes:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sha() -> String {
        "c".repeat(40)
    }

    fn source() -> Source {
        Source {
            url: Some("https://example.test/s.git".into()),
            registry_id: Some("uteke/demo".into()),
            sha: Some(sha()),
        }
    }

    /// #600: a revoke recorded in the ledger beats an approvals file that
    /// was edited back by hand; the gate then refuses the skill, and a fresh
    /// approval lifts it. The skill also refuses once its file drifts.
    #[test]
    fn a_hand_edited_approvals_file_cannot_undo_a_ledger_revoke() {
        let dir = ws("ledger");
        let path = dir.join("approvals.json");
        let mut a = Approvals::default();
        let pkg = approve_from(Some(&dir), "demo", None, &mut a, 1, Some(source())).unwrap();
        a.save_checkpointed(&path).unwrap();
        record(
            &path,
            &tole_core::skill_approval::ledger_line("approve", "demo", &pkg.hash, None, 1),
        )
        .unwrap();
        let gate = |p: &Path| gate_from(&env(None), Some(p)).unwrap();
        let (g, notes) = gate(&path);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(g.load(Some(&dir), "demo").is_ok());
        // Revoked in the ledger, then the approvals file is put back by hand.
        record(
            &path,
            &tole_core::skill_approval::ledger_line("revoke", "demo", &pkg.hash, None, 2),
        )
        .unwrap();
        let (g, notes) = gate(&path);
        assert!(g.load(Some(&dir), "demo").is_err(), "must stay revoked");
        assert!(notes[0].contains("approval ignored"), "{notes:?}");
        let rows = audit_rows_at(Some(&dir), &path).unwrap();
        assert!(
            rows[0]["status"].as_str().unwrap().starts_with("refused"),
            "{rows:?}"
        );
        // A fresh approval is recorded and loads again; a drifted file does not.
        record(
            &path,
            &tole_core::skill_approval::ledger_line("approve", "demo", &pkg.hash, None, 3),
        )
        .unwrap();
        assert!(gate(&path).0.load(Some(&dir), "demo").is_ok());
        std::fs::write(dir.join("skills/demo/extra.md"), "drift").unwrap();
        assert!(gate(&path).0.load(Some(&dir), "demo").is_err());
        let rows = audit_rows_at(Some(&dir), &path).unwrap();
        assert!(rows[0]["status"]
            .as_str()
            .unwrap()
            .contains("changed since approval"));
        assert_eq!(rows[0]["source"]["sha"], serde_json::json!(sha()));
        // An unreadable ledger fails startup instead of meaning "no history".
        let bad = dir.join("bad");
        std::fs::create_dir_all(bad.join(tole_core::skill_approval::LEDGER_FILE)).unwrap();
        a.save(&bad.join("approvals.json")).unwrap();
        assert!(gate_from(&env(None), Some(&bad.join("approvals.json"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_source_is_validated_before_it_is_recorded_and_shown_safely() {
        let dir = ws("source");
        let mut a = Approvals::default();
        let mut bad = source();
        bad.sha = Some("abc".into());
        assert!(approve_from(Some(&dir), "demo", None, &mut a, 1, Some(bad)).is_err());
        assert!(a.skills.is_empty());
        approve_from(Some(&dir), "demo", None, &mut a, 1, Some(source())).unwrap();
        let line = status_line(Some(&dir), "demo", &a);
        assert!(
            line.contains(&format!("  pinned {}", sha())) && line.contains("registry uteke/demo"),
            "{line}"
        );
        // A file-borne source with control characters cannot forge output.
        a.skills.get_mut("demo").unwrap().source = Some(Source {
            url: Some("https://x\u{1b}[2J\n! forged".into()),
            ..Source::default()
        });
        let line = status_line(Some(&dir), "demo", &a);
        assert!(!line.contains('\u{1b}') && !line.contains('\n'), "{line:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An approved and then revoked skill, recorded the way the owner's
    /// commands do (ledger first, then the checkpointed approvals file).
    fn revoked_with_checkpoint(dir: &Path, path: &Path) {
        let mut a = Approvals::default();
        let pkg = approve_from(Some(dir), "demo", None, &mut a, 1, None).unwrap();
        for ev in ["approve", "revoke"] {
            record(
                path,
                &tole_core::skill_approval::ledger_line(ev, "demo", &pkg.hash, None, 1),
            )
            .unwrap();
        }
        // A hand-edit put the approval back after the revoke.
        a.save_checkpointed(path).unwrap();
    }

    /// #879: deleting or truncating the ledger no longer forgets a revoke:
    /// every approval is blocked, with the remedy, and `audit` is an error;
    /// a ledger that only grew is fine.
    #[test]
    fn a_ledger_that_lost_history_blocks_every_approval_with_the_remedy() {
        let dir = ws("cp-fault");
        let path = dir.join("approvals.json");
        revoked_with_checkpoint(&dir, &path);
        let ledger = dir.join(tole_core::skill_approval::LEDGER_FILE);
        // Intact (and grown): the revoke still wins, no fault.
        record(
            &path,
            &tole_core::skill_approval::ledger_line("approve", "other", "sha256:z", None, 3),
        )
        .unwrap();
        let l = load_verified(&path).unwrap();
        assert!(l.ledger_fault.is_none() && l.approvals.skills.is_empty());
        assert!(l.notes[0].contains("approval ignored"), "{:?}", l.notes);
        let saved = std::fs::read(&ledger).unwrap();

        for tamper in [None, Some(saved[..saved.len() / 2].to_vec())] {
            match tamper {
                None => std::fs::remove_file(&ledger).unwrap(),
                Some(b) => std::fs::write(&ledger, b).unwrap(),
            }
            let l = load_verified(&path).unwrap();
            let fault = l.ledger_fault.expect("fault");
            assert!(l.approvals.skills.is_empty(), "everything is blocked");
            assert!(
                fault.contains("tole skills recheckpoint") && fault.contains("skill-ledger.jsonl"),
                "{fault}"
            );
            assert_eq!(l.notes.len(), 1);
            assert!(l.notes[0].starts_with("ERROR: "), "{:?}", l.notes);
            let (g, notes) = gate_from(&env(None), Some(&path)).unwrap();
            assert!(g.load(Some(&dir), "demo").is_err());
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(audit_rows_at(Some(&dir), &path)
                .unwrap_err()
                .contains("recheckpoint"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #879: an approvals file from before the checkpoint keeps loading
    /// (with one note when a ledger exists) and the first owner write adds
    /// the checkpoint.
    #[test]
    fn an_old_approvals_file_loads_with_a_note_until_the_first_owner_write() {
        let dir = ws("cp-legacy");
        let path = dir.join("approvals.json");
        let mut a = Approvals::default();
        let pkg = approve_from(Some(&dir), "demo", None, &mut a, 1, None).unwrap();
        a.save(&path).unwrap();
        let line = tole_core::skill_approval::ledger_line("approve", "demo", &pkg.hash, None, 1);
        record(&path, &line).unwrap();
        let l = load_verified(&path).unwrap();
        assert!(l.ledger_fault.is_none() && l.approvals.skills.len() == 1);
        assert_eq!(l.notes.len(), 1, "{:?}", l.notes);
        assert!(l.notes[0].contains("no ledger checkpoint"), "{:?}", l.notes);
        // Without a ledger the file is not a downgrade candidate: no note.
        let bare = dir.join("bare");
        std::fs::create_dir(&bare).unwrap();
        a.save(&bare.join("approvals.json")).unwrap();
        assert!(load_verified(&bare.join("approvals.json"))
            .unwrap()
            .notes
            .iter()
            .all(|n| !n.contains("checkpoint")));
        a.save_checkpointed(&path).unwrap();
        assert!(load_verified(&path).unwrap().notes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #879: `adopt` records only the approvals the ledger does not name,
    /// as stored and noted, refreshes the checkpoint and is quiet after.
    #[test]
    fn adopt_records_unrecorded_approvals_and_leaves_recorded_ones_alone() {
        let dir = ws("adopt");
        let path = dir.join("approvals.json");
        let mut a = Approvals::default();
        let pkg = approve_from(Some(&dir), "demo", None, &mut a, 1, Some(source())).unwrap();
        // A second approval the ledger already records.
        a.skills.insert("rec".into(), a.skills["demo"].clone());
        a.save(&path).unwrap();
        let rec = tole_core::skill_approval::ledger_line("approve", "rec", &pkg.hash, None, 1);
        record(&path, &rec).unwrap();
        let ledger = dir.join(tole_core::skill_approval::LEDGER_FILE);
        let before = std::fs::read_to_string(&ledger).unwrap();

        // Unknown name: refused, nothing written.
        assert!(adopt(&path, &a, &["nope".into()], false, 7)
            .unwrap_err()
            .contains("no approval"));
        assert_eq!(std::fs::read_to_string(&ledger).unwrap(), before);
        // A recorded one: reported, left alone.
        let out = adopt(&path, &a, &["rec".into()], false, 7).unwrap();
        assert!(out[0].contains("already recorded"), "{out:?}");
        assert!(out.last().unwrap().contains("adopted 0"), "{out:?}");
        assert_eq!(std::fs::read_to_string(&ledger).unwrap(), before);
        // --all adopts the one the ledger does not name.
        let out = adopt(&path, &a, &[], true, 7).unwrap();
        assert!(out[0].starts_with("adopt demo  sha256:"), "{out:?}");
        assert!(out[0].contains("registry uteke/demo"), "{out:?}");
        assert!(out.last().unwrap().contains("adopted 1"), "{out:?}");
        let after = std::fs::read_to_string(&ledger).unwrap();
        let added = after.strip_prefix(&before).unwrap();
        assert!(
            added.lines().count() == 1
                && added.contains("\"skill\":\"demo\"")
                && added.contains("\"note\":\"adopted\"")
                && added.contains(&pkg.hash)
                && added.contains(&sha()),
            "{added}"
        );
        let l = load_verified(&path).unwrap();
        assert!(l.notes.is_empty(), "noise gone: {:?}", l.notes);
        assert_eq!(l.approvals.skills.len(), 2);
        tole_core::skill_approval::verify_checkpoint_at(&path).unwrap();
        // Nothing left to adopt: no new line.
        adopt(&path, &l.approvals, &[], true, 8).unwrap();
        assert_eq!(std::fs::read_to_string(&ledger).unwrap(), after);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
