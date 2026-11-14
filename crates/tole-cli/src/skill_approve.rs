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
use tole_core::skill_approval::{format_time, read_package, Approvals, Gate};

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
            Some(p) => Ok((Gate::Approved(Arc::new(Approvals::load(p)?)), Vec::new())),
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
    },
    /// Skill drafts the agent proposed with `skill_propose` (#592): list,
    /// show, reject, record a reviewer's verdict. A draft never loads.
    Drafts {
        #[command(subcommand)]
        action: Option<crate::skill_drafts_cli::DraftsAction>,
    },
    /// Remove a skill's approval; it stays on disk.
    Revoke { name: String },
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
    match approvals.skills.get(name) {
        Some(a) if a.hash == pkg.hash => format!(
            "{name}  approved  {}  {}",
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
    approvals.approve(&pkg, now);
    Ok(pkg)
}

pub fn cli(action: SkillsAction) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let store = || -> anyhow::Result<(PathBuf, Approvals)> {
        let path = Approvals::default_path()
            .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
        let approvals = Approvals::load(&path).map_err(anyhow::Error::msg)?;
        Ok((path, approvals))
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
        } => {
            use std::io::{BufRead as _, IsTerminal as _, Write as _};
            crate::owner_only::refuse_under_session("tole skills approve")
                .map_err(anyhow::Error::msg)?;
            let ws = root(workspace)?;
            let (path, mut approvals) = store()?;
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
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // Approve exactly what was shown: the package read again must
            // still hash to it.
            let pkg = approve(Some(&ws), &name, Some(&shown.hash), &mut approvals, now)
                .map_err(anyhow::Error::msg)?;
            approvals.save(&path).map_err(anyhow::Error::msg)?;
            println!("approved {name} ({})", pkg.hash);
        }
        SkillsAction::Drafts { action } => {
            let store = tole_core::skill_draft::DraftStore::default_store()
                .context("the CodeCora home is unresolvable (set CODECORA_HOME or HOME)")?;
            return crate::skill_drafts_cli::run(action, &store);
        }
        SkillsAction::Revoke { name } => {
            let (path, mut approvals) = store()?;
            if !approvals.revoke(&name) {
                anyhow::bail!("skill {name:?} has no approval");
            }
            approvals.save(&path).map_err(anyhow::Error::msg)?;
            println!(
                "revoked {name}: it stays on disk and no longer loads when approval is required"
            );
        }
        SkillsAction::List { workspace } => {
            let ws = root(workspace)?;
            let (_, approvals) = store()?;
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

    fn ws(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("tole-skapprove-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
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
}
