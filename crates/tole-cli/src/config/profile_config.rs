//! `[profiles.<name>]`: the tier models and persona table, and their validation (#584).

use super::secrets_config::valid_grant_name;
use serde::Deserialize;

/// `[profiles.<name>.models]` (#584): model names per task-weight tier.
/// `default` names the tier a session uses unless something picks another
/// (the `[router]` of #586, when enabled, for rules that name the profile).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTiers {
    pub light: Option<String>,
    pub standard: Option<String>,
    pub heavy: Option<String>,
    /// Consulted, never executes (#587).
    pub advisor: Option<String>,
    /// `light | standard | heavy`; default `standard`.
    pub default: Option<String>,
}

/// The executing tiers a `default` may name (`advisor` never executes).
pub const EXEC_TIERS: [&str; 3] = ["light", "standard", "heavy"];

impl ModelTiers {
    /// The model of an executing tier, when that tier is set.
    pub fn tier(&self, name: &str) -> Option<&str> {
        match name {
            "light" => self.light.as_deref(),
            "standard" => self.standard.as_deref(),
            "heavy" => self.heavy.as_deref(),
            _ => None,
        }
        .filter(|m| !m.trim().is_empty())
    }

    /// The `default` tier's name (`standard` when unset).
    pub fn default_tier(&self) -> &str {
        self.default.as_deref().map(str::trim).unwrap_or("standard")
    }
}

/// One `[profiles.<name>]` table (#584): a dormant persona. It holds
/// configuration only; selecting it (`--profile <name>`) is the only thing
/// that reads its persona file, and nothing is ever started for it.
/// USER LAYER ONLY, like `[tui]`: a project config may not define who the
/// agent is or what it may do without asking (see [`layered`]).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Persona file (`~/` expanded; relative to the user config's directory).
    pub system_prompt_file: Option<String>,
    /// Tool patterns pre-authorized for this profile (same syntax as `allow`).
    pub allow: Option<Vec<String>>,
    /// uteke room of this profile's memory loop (default `profile-<name>`).
    pub memory_room: Option<String>,
    /// Skill files preloaded for this profile (same syntax as `skill`).
    pub skills: Option<Vec<String>>,
    /// NAMES of the environment secrets this profile may be granted
    /// (never values; #598 releases them, #585 intersects with the parent).
    pub grants: Option<Vec<String>>,
    pub models: Option<ModelTiers>,
    /// `consult_advisor` calls per run (#587; default 3, 1 to 20). Only with
    /// `models.advisor` set.
    pub advisor_max_uses: Option<u32>,
}

/// Profile names: `[a-z0-9_-]`, 1 to 64 chars, not starting with `-`
/// (they become `--profile` values and the `profile-<name>` room).
pub fn valid_profile_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

pub(super) fn check_profile(name: &str, p: &ProfileConfig) -> Result<(), String> {
    if !valid_profile_name(name) {
        return Err(format!(
            "profile name `{name}` must be 1-64 chars of a-z, 0-9, `_` or `-`, not starting with `-`"
        ));
    }
    // The discussion's built-in roles (#588) take only their models.
    if let Some(role) = crate::discussion_roles::Role::of(name) {
        return crate::discussion_roles::check_override(role, p);
    }
    if let Some(room) = &p.memory_room {
        let ok = !room.is_empty()
            && !room.starts_with('-')
            && room
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(format!(
                "profiles.{name}.memory_room must be [a-zA-Z0-9._-], not starting with `-`"
            ));
        }
    }
    // Never echo the entry: it may be a value pasted by mistake.
    if p.grants.iter().flatten().any(|g| !valid_grant_name(g)) {
        return Err(format!(
            "profiles.{name}.grants: every entry must be an environment variable NAME              ([A-Z0-9_]), never a value"
        ));
    }
    // The advisor tier (#587): a model that is consulted, never executes.
    let advisor = p.models.as_ref().and_then(|m| m.advisor.as_deref());
    if advisor.is_some_and(|a| a.trim().is_empty()) {
        return Err(format!(
            "profiles.{name}.models.advisor is blank: name the advisor model or remove the key"
        ));
    }
    if let Some(n) = p.advisor_max_uses {
        let max = tole_core::advisor::MAX_USES_CEILING;
        if !(1..=max).contains(&n) {
            return Err(format!(
                "profiles.{name}.advisor_max_uses must be 1 to {max}, got {n}"
            ));
        }
        if advisor.is_none() {
            return Err(format!(
                "profiles.{name}.advisor_max_uses is set but profiles.{name}.models.advisor is \
                 not set: declare the advisor model or drop advisor_max_uses"
            ));
        }
    }
    if let Some(m) = &p.models {
        let tier = m.default_tier();
        if !EXEC_TIERS.contains(&tier) {
            return Err(format!(
                "profiles.{name}.models.default must be light, standard or heavy, got {tier:?}"
            ));
        }
        if m.tier(tier).is_none() {
            return Err(format!(
                "profiles.{name}.models.default is {tier:?} but profiles.{name}.models.{tier} is not set"
            ));
        }
    }
    Ok(())
}
