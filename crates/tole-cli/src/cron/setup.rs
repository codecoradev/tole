//! What an agent run of a cron job brings besides its prompt (#579): the
//! job's skills (`--skill`) and its profile (`--profile`, #584).
//!
//! Both are validated when the job is saved, and again when a run is
//! claimed: a skill that is gone or broken, or a profile the user config no
//! longer defines, records the occurrence `blocked` before any token is
//! spent (never a silent run without them).
//!
//! A profile applies its persona, model, skills and memory room, under the
//! unattended rules:
//! - **Writes:** a run auto-approves a Write only when BOTH the job's
//!   `--allow` and the profile's `allow` cover it (the intersection, never
//!   wider than either). Anything else is parked or refused as before.
//! - **Secrets:** a run receives `Profile::granted(job grants)`. A job has
//!   no grant list, so a profile's `grants` give a cron run nothing: every
//!   secret stays scrubbed from its child processes.
//! - **Model:** the job's `--model` pin, else the profile's default tier,
//!   else the environment's model.

use std::path::{Path, PathBuf};

use tole_core::ledger::Job;
use tole_core::skills::Skill;

use crate::config::Config;
use crate::profile::Profile;

/// A skill entry is a file when it names a path (`/`) or a `.md` file;
/// otherwise it is a skill name, discovered from the job's workdir.
pub fn is_file_entry(entry: &str) -> bool {
    entry.contains('/') || entry.ends_with(".md")
}

/// Load one `--skill` entry: a SKILL.md file (its directory names the
/// skill) or a skill name found from `workdir` (project, then user-global).
pub fn load_entry(entry: &str, workdir: &Path) -> Result<Skill, String> {
    if is_file_entry(entry) {
        let path = Path::new(entry);
        let content = std::fs::read_to_string(path).map_err(|e| format!("{entry}: {e}"))?;
        let name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("skill");
        tole_core::skills::parse_skill(&content, name).map_err(|e| format!("{entry}: {e}"))
    } else {
        tole_core::skills::load_skill(Some(workdir), entry)
    }
}

/// Validate the `--skill` entries of a job being saved; a file entry is
/// made absolute so the run finds it from any directory.
pub fn normalize_skills(entries: &[String], workdir: &Path) -> Result<Vec<String>, String> {
    entries
        .iter()
        .map(|e| {
            let e = e.trim();
            let saved = if is_file_entry(e) {
                std::fs::canonicalize(e)
                    .map_err(|err| format!("{e}: {err}"))?
                    .display()
                    .to_string()
            } else {
                e.to_string()
            };
            load_entry(&saved, workdir)?;
            Ok(saved)
        })
        .collect()
}

/// The user config (`None` when there is none).
pub fn user_config() -> Result<Option<Config>, String> {
    match crate::config::user_config_path().filter(|p| p.exists()) {
        Some(path) => crate::config::load(&path)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

/// Resolve profile `name` from the user config, the way `--profile` does.
pub fn resolve_profile(name: &str) -> Result<Profile, String> {
    let cfg = user_config()?.unwrap_or_default();
    let base = crate::config::user_config_path().and_then(|p| p.parent().map(Path::to_path_buf));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Profile::resolve(&cfg, name, base.as_deref(), home.as_deref())
}

/// Everything an agent run needs from the job's skills and profile.
#[derive(Debug, Clone, Default)]
pub struct AgentSetup {
    pub profile: Option<Profile>,
    /// The job's skills, then the profile's.
    pub skills: Vec<Skill>,
}

/// Resolve a job's skills and profile for a run. The error is the
/// `blocked` reason code and its detail.
pub fn resolve(job: &Job) -> Result<AgentSetup, (&'static str, String)> {
    let workdir = PathBuf::from(super::field(job, "workdir").unwrap_or_default());
    let profile = match super::field(job, "profile") {
        Some(name) => Some(resolve_profile(name).map_err(|e| ("profile_missing", e))?),
        None => None,
    };
    let mut skills = Vec::new();
    for entry in list(job, "skills") {
        skills.push(load_entry(&entry, &workdir).map_err(|e| ("skill_missing", e))?);
    }
    for path in profile.iter().flat_map(|p| p.skills.iter()) {
        let entry = path.display().to_string();
        skills.push(load_entry(&entry, &workdir).map_err(|e| ("skill_missing", e))?);
    }
    Ok(AgentSetup { profile, skills })
}

/// A string-list field of a job (empty when absent).
pub fn list(job: &Job, key: &str) -> Vec<String> {
    job.extra
        .get(key)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

impl AgentSetup {
    /// The run's system prompt: the profile's persona, then one section
    /// per skill. `None` when there is neither (the run is unchanged).
    pub fn system_prompt(&self) -> Option<String> {
        let mut out = self
            .profile
            .as_ref()
            .and_then(|p| p.system_prompt.clone())
            .unwrap_or_default();
        for s in &self.skills {
            out.push_str(&format!("\n\n# Skill: {}\n{}", s.name, s.body));
        }
        let out = out.trim_start().to_string();
        (!out.is_empty()).then_some(out)
    }

    /// The model of the run: the job's pin, else the profile's.
    pub fn model(&self, job: &Job) -> Option<String> {
        super::field(job, "model")
            .map(str::to_string)
            .or_else(|| self.profile.as_ref().and_then(|p| p.model.clone()))
    }

    /// The secret names the run may receive: the profile's grants that the
    /// job grants too. A job grants none, so this is always empty today.
    pub fn granted(&self) -> Vec<String> {
        let job_grants: [String; 0] = [];
        self.profile
            .as_ref()
            .map(|p| p.granted(&job_grants))
            .unwrap_or_default()
    }

    /// The memory loop of the run: only for a profiled job while memory is
    /// on (`TOLE_MEMORY=uteke` and the binary present), in the profile's
    /// room.
    pub fn memory(&self, workdir: &Path) -> Option<tole_core::memory::MemoryConfig> {
        let profile = self.profile.as_ref()?;
        let on = std::env::var("TOLE_MEMORY").is_ok_and(|v| v.trim() == "uteke")
            && crate::tool_roster::binary_available("uteke");
        on.then(|| {
            let mut m = tole_core::memory::MemoryConfig::for_cwd("uteke", workdir);
            m.room = tole_core::memory::sanitize_namespace(&profile.memory_room);
            m
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-cron-setup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn skill(root: &Path, name: &str, body: &str) -> PathBuf {
        let d = root.join("skills").join(name);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("SKILL.md");
        std::fs::write(
            &f,
            format!("---\nname: {name}\ndescription: \"d\"\n---\n{body}"),
        )
        .unwrap();
        f
    }

    fn job(extra: serde_json::Value) -> Job {
        Job {
            id: "j".into(),
            schedule: "@daily".into(),
            timezone: None,
            catch_up: tole_core::ledger::CatchUp::Skip,
            grace_minutes: 0,
            paused: false,
            removed: false,
            at: 0,
            extra: extra.as_object().unwrap().clone(),
        }
    }

    #[test]
    fn skill_entries_are_names_or_files_validated_on_save() {
        let ws = tmp("skills");
        skill(&ws, "lint", "run the linters");
        let other = tmp("skills-other");
        let file = skill(&other, "deploy", "ship it");
        let saved = normalize_skills(&["lint".into(), file.display().to_string()], &ws).unwrap();
        assert_eq!(saved, ["lint".to_string(), file.display().to_string()]);
        assert!(normalize_skills(&["absent".into()], &ws).is_err());
        assert!(normalize_skills(&["/no/such/SKILL.md".into()], &ws).is_err());
        std::fs::write(ws.join("skills/lint/SKILL.md"), "no frontmatter").unwrap();
        assert!(normalize_skills(&["lint".into()], &ws).is_err());
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn a_run_gets_its_skills_in_the_system_prompt_and_is_blocked_when_one_is_gone() {
        let ws = tmp("run");
        skill(&ws, "lint", "run the linters");
        let j = job(json!({"workdir": ws.display().to_string(), "skills": ["lint"]}));
        let setup = resolve(&j).unwrap();
        assert_eq!(
            setup.system_prompt().as_deref(),
            Some("# Skill: lint\nrun the linters")
        );
        // No skills, no profile: the run is unchanged.
        let plain = job(json!({"workdir": ws.display().to_string()}));
        assert_eq!(resolve(&plain).unwrap().system_prompt(), None);
        std::fs::remove_dir_all(ws.join("skills/lint")).unwrap();
        let (code, detail) = resolve(&j).unwrap_err();
        assert_eq!(code, "skill_missing");
        assert!(detail.contains("lint"), "{detail}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_profile_sets_persona_model_and_grants_nothing() {
        let cfg = crate::config::parse(
            "[profiles.ops]\nallow = [\"write_*\"]\ngrants = [\"GH_TOKEN\"]\n\
             [profiles.ops.models]\nstandard = \"ops-model\"\n",
            Path::new("/c.toml"),
        )
        .unwrap();
        let mut p = Profile::resolve(&cfg, "ops", None, None).unwrap();
        p.system_prompt = Some("You are ops.".into());
        let setup = AgentSetup {
            profile: Some(p),
            skills: vec![tole_core::skills::parse_skill(
                "---\nname: lint\ndescription: \"d\"\n---\nbody",
                "lint",
            )
            .unwrap()],
        };
        assert_eq!(
            setup.system_prompt().as_deref(),
            Some("You are ops.\n\n# Skill: lint\nbody")
        );
        assert_eq!(setup.model(&job(json!({}))).as_deref(), Some("ops-model"));
        assert_eq!(
            setup.model(&job(json!({"model": "pin"}))).as_deref(),
            Some("pin"),
            "the job's pin beats the profile"
        );
        assert!(
            setup.granted().is_empty(),
            "a cron run is granted no secret"
        );
    }
}
