//! Who a job's power comes from (#581 part 2, design
//! `docs/automation-design.md` §5).
//!
//! A job saved by a human through the CLI (`created_by = cli`) carries the
//! owner's authority. Every other job (one an agent created with the
//! `cron_create` tool, or a ledger line of any other origin) is held to its
//! creator's grant:
//!
//! - it runs only after a human enabled it (`tole cron resume` records
//!   `enabled_by = cli`); the tool saves it paused;
//! - its `allow` never exceeds the creator's (`creator_allow`): each allow
//!   pattern is one of the creator's, and each of the creator's deny (`!`)
//!   patterns is kept;
//! - it is never a script job (a script runs a command with the owner's
//!   authority).
//!
//! The creator fields cannot be changed by a later line of the same live
//! job (the ledger fold keeps the creating line's values,
//! `tole_core::ledger::PROVENANCE`). And for any agent job, an agent run
//! whose file jail (its workdir) holds the cron dir is blocked, because its
//! file tools could write ledger lines.

use std::path::Path;

use serde_json::Value;
use tole_core::approval::DENY_PREFIX;
use tole_core::ledger::Job;

use super::{field, mode};

/// `created_by` of a job saved through the CLI.
pub const CLI: &str = "cli";
/// `created_by` of a job saved by the `cron_create` tool.
pub const AGENT: &str = "agent";

/// A list field of a job (`[]` when absent or not a list of strings).
pub fn list(job: &Job, key: &str) -> Vec<String> {
    job.extra
        .get(key)
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default()
}

/// `allow` as saved for a creator holding `creator`: `requested` must name
/// only the creator's allow patterns; every creator deny pattern is added.
pub fn within(requested: &[String], creator: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for p in requested.iter().map(|p| p.trim()) {
        // A deny pattern only narrows; an allow pattern must be the creator's.
        if p.starts_with(DENY_PREFIX) || creator.iter().any(|c| c == p) {
            out.push(p.to_string());
        } else {
            return Err(format!(
                "allow pattern `{p}` is not in the creator's allow list ({})",
                if creator.is_empty() {
                    "empty".to_string()
                } else {
                    creator.join(", ")
                }
            ));
        }
    }
    for deny in creator.iter().filter(|c| c.starts_with(DENY_PREFIX)) {
        if !out.contains(deny) {
            out.push(deny.clone());
        }
    }
    Ok(out)
}

/// True when the job was saved through the CLI.
pub fn by_owner(job: &Job) -> bool {
    field(job, "created_by") == Some(CLI)
}

/// The pre-dispatch trust check: `Err((code, detail))` blocks the run.
pub fn check(job: &Job, cron_dir: &Path) -> Result<(), (&'static str, String)> {
    let agent = mode(job) != "script";
    if agent {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let jail = canon(Path::new(field(job, "workdir").unwrap_or_default()));
        if canon(cron_dir).starts_with(&jail) {
            return Err((
                "ledger_in_workdir",
                format!(
                    "the workdir {} holds the cron dir, so the run's file tools could write \
                     job records; give the job a workdir that does not contain {}",
                    jail.display(),
                    cron_dir.display()
                ),
            ));
        }
    }
    if by_owner(job) {
        return Ok(());
    }
    if !agent {
        return Err((
            "script_untrusted",
            "only a job saved with `tole cron create` may run a script".into(),
        ));
    }
    if field(job, "enabled_by") != Some(CLI) {
        return Err((
            "not_enabled",
            format!(
                "the job was not saved by its owner; review it, then enable it with \
                 `tole cron resume {}`",
                job.id
            ),
        ));
    }
    let creator = list(job, "creator_allow");
    let allow = list(job, "allow");
    match within(&allow, &creator) {
        Ok(saved) if saved.len() == allow.len() => Ok(()),
        Ok(_) => Err((
            "allow_exceeds_creator",
            "the job's allow list drops a deny pattern of its creator".into(),
        )),
        Err(e) => Err(("allow_exceeds_creator", e)),
    }
}

/// The provenance fields an edit of `old` carries over (the creator, its
/// grant and the owner's enable).
pub fn carried(old: &Job) -> impl Iterator<Item = (&'static str, Value)> + '_ {
    ["created_by", "creator_allow", "enabled_by"]
        .into_iter()
        .filter_map(|k| old.extra.get(k).map(|v| (k, v.clone())))
}
