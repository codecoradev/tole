//! The discussion's built-in roles (#588, design
//! `profiles-collaboration-secrets.md` A8.3, A9.3): the **scribe**, who
//! writes a round's digest, and the **claim-checker**, who checks the
//! acceptance criteria against the raw contributions.
//!
//! - **Built in.** Each runs on the `light` tier (the role pin of A9.3: it
//!   wins over the router), with no tools, no streaming and the fixed
//!   instructions of `tole_core::discussion` (`scribe_request`,
//!   `checker_request`). The built-in model is the `light` tier of
//!   `[router.tiers]` in the user config, else the run's configured model.
//! - **Overridden only by the owner**, through `[profiles.scribe]` or
//!   `[profiles.claim-checker]` in the USER config (a project config cannot
//!   define profiles at all, see `config::layered`). An override sets only
//!   `models`: the role keeps its fixed instructions and gets no tools, no
//!   allow list, no skills and no secrets. Its tier is `models.default`, else
//!   `light`, and that tier must name a model: an override that resolves to
//!   a missing or unknown tier is a load error, never a fallback to the
//!   built-in.
//! - **Not participants.** A role is never a voice in the discussion.

use crate::config::{Config, ProfileConfig, EXEC_TIERS};
use tole_core::router::Tier;

/// A built-in discussion role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Scribe,
    ClaimChecker,
}

impl Role {
    pub const ALL: [Role; 2] = [Role::Scribe, Role::ClaimChecker];

    /// Its profile name, the one an override uses.
    pub fn name(self) -> &'static str {
        match self {
            Role::Scribe => "scribe",
            Role::ClaimChecker => "claim-checker",
        }
    }

    /// The role a profile name overrides, if any.
    pub fn of(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.name() == name)
    }
}

/// The tier a role runs on unless its override says otherwise.
pub const ROLE_TIER: Tier = Tier::Light;

/// A role's model, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleModel {
    pub role: Role,
    pub tier: Tier,
    /// `None`: the run's configured model (no `light` tier is declared).
    pub model: Option<String>,
    /// Where it comes from, names only (never the model).
    pub source: &'static str,
}

/// Checks a `[profiles.scribe]` or `[profiles.claim-checker]` override at
/// load time (called from `config::check_profile`).
pub fn check_override(role: Role, p: &ProfileConfig) -> Result<(), String> {
    let name = role.name();
    let extra = [
        ("system_prompt_file", p.system_prompt_file.is_some()),
        ("allow", p.allow.is_some()),
        ("memory_room", p.memory_room.is_some()),
        ("skills", p.skills.is_some()),
        ("grants", p.grants.is_some()),
        ("advisor_max_uses", p.advisor_max_uses.is_some()),
        (
            "models.advisor",
            p.models.as_ref().is_some_and(|m| m.advisor.is_some()),
        ),
    ];
    if let Some((key, _)) = extra.iter().find(|(_, set)| *set) {
        return Err(format!(
            "profiles.{name}.{key}: `{name}` is a built-in discussion role with fixed \
             instructions and no tools; an override sets only its models"
        ));
    }
    tier_of(role, p).map(|_| ())
}

/// The override's tier and model, or the load error naming the remedy.
fn tier_of(role: Role, p: &ProfileConfig) -> Result<(Tier, String), String> {
    let name = role.name();
    let Some(m) = p.models.as_ref() else {
        return Err(format!(
            "profiles.{name} overrides the built-in {name} but sets no models: set \
             profiles.{name}.models.{} (the role's tier), or remove the table to use the \
             built-in",
            ROLE_TIER.as_str()
        ));
    };
    let tier = m.default.as_deref().map_or(ROLE_TIER.as_str(), str::trim);
    if !EXEC_TIERS.contains(&tier) {
        return Err(format!(
            "profiles.{name}.models.default must be light, standard or heavy, got {tier:?}"
        ));
    }
    let model = m.tier(tier).ok_or_else(|| {
        format!(
            "profiles.{name} resolves to the {tier} tier but profiles.{name}.models.{tier} is \
             not set: name a model there (an override never falls back to the built-in)"
        )
    })?;
    let tier = Tier::parse(tier).ok_or_else(|| format!("unknown tier {tier:?}"))?;
    Ok((tier, model.to_string()))
}

/// A role's model under `cfg` (the layered config, whose profiles and
/// router tiers come from the user layer only).
pub fn resolve(cfg: &Config, role: Role) -> Result<RoleModel, String> {
    if let Some(p) = cfg.profiles.as_ref().and_then(|p| p.get(role.name())) {
        let (tier, model) = tier_of(role, p)?;
        return Ok(RoleModel {
            role,
            tier,
            model: Some(model),
            source: "user override",
        });
    }
    let light = cfg
        .router
        .as_ref()
        .and_then(|r| r.tiers.as_ref())
        .and_then(|t| t.light.as_deref())
        .map(str::trim)
        .filter(|m| !m.is_empty());
    Ok(RoleModel {
        role,
        tier: ROLE_TIER,
        model: light.map(str::to_string),
        source: if light.is_some() {
            "built-in, [router.tiers]"
        } else {
            "built-in, the configured model"
        },
    })
}

/// `tole doctor` lines: where each role's model comes from, names only.
pub fn doctor_lines(cfg: &Config) -> Vec<String> {
    Role::ALL
        .into_iter()
        .map(|role| match resolve(cfg, role) {
            Ok(r) => format!(
                "discussion {}: {} tier ({}){}",
                role.name(),
                r.tier.as_str(),
                r.source,
                if r.model.is_none() {
                    "; declare [router.tiers] light for a cheaper model"
                } else {
                    ""
                }
            ),
            Err(e) => format!("discussion {}: ERROR {e}", role.name()),
        })
        .collect()
}

#[cfg(test)]
#[path = "discussion_roles_tests.rs"]
mod tests;
