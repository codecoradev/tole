//! Dormant profiles (#584, design `profiles-collaboration-secrets.md` A5.1,
//! A6, A9.3): a named `[profiles.<name>]` table in the user config. A
//! profile is configuration and a persona file only. [`Profile::resolve`] is
//! the one way to use it: it reads that profile's persona file and nothing
//! else, and starts nothing (no process, no provider call, no memory read).
//! An unselected profile costs nothing but its config lines.
//!
//! The resolved values enter the existing settings resolution as one layer
//! (`config_apply::Source::Profile`, between the per-key flags and the
//! environment), so every face that consumes `Settings` honors a profile
//! without new plumbing.

use crate::config::{Config, ModelTiers, ProfileConfig};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Size cap of a persona file, like the config file itself.
pub const MAX_PERSONA_BYTES: u64 = 64 * 1024;

/// A selected profile, resolved: what it contributes to a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    /// The persona file's text (`None` when the profile names none).
    pub system_prompt: Option<String>,
    /// Pre-authorized tool patterns (empty = the profile adds none).
    pub allow: Vec<String>,
    /// uteke room of the memory loop: `memory_room`, else `profile-<name>`.
    pub memory_room: String,
    /// Skill files to preload.
    pub skills: Vec<PathBuf>,
    /// Secret NAMES the profile may be granted (never values).
    pub grants: Vec<String>,
    /// All tiers, for the router (#586) and the advisor (#587).
    pub models: ModelTiers,
    /// The `default` tier's model, used for the session.
    pub model: Option<String>,
}

impl Profile {
    /// Resolve profile `name` of the (layered) config. `base` is the
    /// directory relative paths are taken from (the user config's), `home`
    /// expands `~/`. Reads only this profile's persona file.
    pub fn resolve(
        cfg: &Config,
        name: &str,
        base: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<Profile, String> {
        let profiles = cfg.profiles.as_ref().filter(|p| !p.is_empty());
        let Some(p) = profiles.and_then(|p| p.get(name)) else {
            return Err(match profiles {
                None => format!(
                    "unknown profile `{name}`: the user config defines no profiles \
                     (add a [profiles.{name}] table)"
                ),
                Some(all) => format!(
                    "unknown profile `{name}` (defined: {})",
                    all.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            });
        };
        Self::from_config(name, p, base, home)
    }

    fn from_config(
        name: &str,
        p: &ProfileConfig,
        base: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<Profile, String> {
        let at = |key: &str, raw: &str| {
            expand(raw, base, home).ok_or_else(|| {
                format!(
                    "profiles.{name}.{key}: cannot resolve {raw:?} (no home or config directory)"
                )
            })
        };
        let system_prompt = match p.system_prompt_file.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => {
                let path = at("system_prompt_file", raw)?;
                Some(
                    read_persona(&path)
                        .map_err(|e| format!("profiles.{name}.system_prompt_file: {e}"))?,
                )
            }
        };
        let skills = p
            .skills
            .iter()
            .flatten()
            .map(|s| at("skills", s))
            .collect::<Result<Vec<_>, _>>()?;
        let models = p.models.clone().unwrap_or_default();
        let model = models.tier(models.default_tier()).map(str::to_string);
        Ok(Profile {
            name: name.to_string(),
            system_prompt,
            allow: p.allow.clone().unwrap_or_default(),
            memory_room: p
                .memory_room
                .clone()
                .unwrap_or_else(|| format!("profile-{name}")),
            skills,
            grants: p.grants.clone().unwrap_or_default(),
            models,
            model,
        })
    }

    /// The secrets a run of this profile may receive: the names both the
    /// parent's grant and this profile's grant list (#583 invariant
    /// "child allowlist = parent ∩ profile", applied to secrets). Order
    /// follows the profile; nothing outside the parent's grant ever passes.
    pub fn granted(&self, parent: &[String]) -> Vec<String> {
        self.grants
            .iter()
            .filter(|g| parent.contains(g))
            .cloned()
            .collect()
    }
}

/// `~/x` against `home`, an absolute path as is, anything else against
/// `base`. `None` when the needed directory is unknown.
fn expand(raw: &str, base: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        base.map(|b| b.join(p))
    }
}

fn read_persona(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = Vec::new();
    file.take(MAX_PERSONA_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if buf.len() as u64 > MAX_PERSONA_BYTES {
        return Err(format!(
            "{} is larger than the {} KiB limit",
            path.display(),
            MAX_PERSONA_BYTES / 1024
        ));
    }
    String::from_utf8(buf).map_err(|_| format!("{} is not valid UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-profile-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(src: &str) -> Config {
        crate::config::parse(src, Path::new("user.toml")).unwrap()
    }

    const TWO: &str = r#"
[profiles.coder]
system_prompt_file = "coder.md"
allow = ["write_*"]
skills = ["skills/rust/SKILL.md", "~/shared/SKILL.md"]
grants = ["GITHUB_TOKEN", "NGINJEN_API_KEY"]
[profiles.coder.models]
light = "small-model"
standard = "mid-model"
heavy = "big-model"
advisor = "wise-model"
default = "heavy"

[profiles.reviewer]
system_prompt_file = "missing-on-purpose.md"
"#;

    #[test]
    fn resolving_one_profile_reads_only_its_persona() {
        let d = tmp("one");
        std::fs::write(d.join("coder.md"), "You write code.").unwrap();
        let home = d.join("home");
        let p = Profile::resolve(&cfg(TWO), "coder", Some(&d), Some(&home)).unwrap();
        assert_eq!(p.system_prompt.as_deref(), Some("You write code."));
        assert_eq!(p.allow, vec!["write_*"]);
        assert_eq!(p.memory_room, "profile-coder");
        assert_eq!(
            p.skills,
            vec![d.join("skills/rust/SKILL.md"), home.join("shared/SKILL.md")]
        );
        assert_eq!(
            p.model.as_deref(),
            Some("big-model"),
            "default tier = heavy"
        );
        assert_eq!(p.models.advisor.as_deref(), Some("wise-model"));
        // Dormant: `reviewer` names a persona file that does not exist, and
        // resolving `coder` never touched it.
        let err = Profile::resolve(&cfg(TWO), "reviewer", Some(&d), Some(&home)).unwrap_err();
        assert!(err.contains("missing-on-purpose.md"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_profiles_name_the_defined_ones() {
        let e = Profile::resolve(&cfg(TWO), "writer", None, None).unwrap_err();
        assert_eq!(e, "unknown profile `writer` (defined: coder, reviewer)");
        let e = Profile::resolve(&Config::default(), "writer", None, None).unwrap_err();
        assert!(e.contains("defines no profiles"), "{e}");
    }

    #[test]
    fn a_minimal_profile_has_defaults_and_no_model() {
        let p = Profile::resolve(&cfg("[profiles.min]\n"), "min", None, None).unwrap();
        assert_eq!(p.system_prompt, None);
        assert!(p.allow.is_empty() && p.skills.is_empty() && p.grants.is_empty());
        assert_eq!(p.memory_room, "profile-min");
        assert_eq!(p.model, None);
    }

    #[test]
    fn standard_is_the_default_tier() {
        let p = Profile::resolve(
            &cfg("[profiles.a.models]\nlight = \"s\"\nstandard = \"m\"\n"),
            "a",
            None,
            None,
        )
        .unwrap();
        assert_eq!(p.model.as_deref(), Some("m"));
    }

    #[test]
    fn granted_secrets_are_the_intersection_with_the_parent() {
        let p = Profile::resolve(
            &cfg("[profiles.a]\ngrants = [\"A_TOKEN\", \"B_KEY\"]\n"),
            "a",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            p.granted(&["B_KEY".into(), "C_SECRET".into()]),
            vec!["B_KEY".to_string()]
        );
        assert!(
            p.granted(&[]).is_empty(),
            "no parent grant, nothing granted"
        );
    }

    #[test]
    fn oversized_or_unreadable_personas_are_errors() {
        let d = tmp("big");
        std::fs::write(d.join("big.md"), "x".repeat(MAX_PERSONA_BYTES as usize + 1)).unwrap();
        let c = cfg("[profiles.a]\nsystem_prompt_file = \"big.md\"\n");
        let e = Profile::resolve(&c, "a", Some(&d), None).unwrap_err();
        assert!(e.contains("larger than the 64 KiB limit"), "{e}");
        let c = cfg("[profiles.a]\nsystem_prompt_file = \"rel.md\"\n");
        let e = Profile::resolve(&c, "a", None, None).unwrap_err();
        assert!(e.contains("cannot resolve"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
