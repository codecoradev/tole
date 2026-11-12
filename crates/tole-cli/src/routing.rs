//! Model routing on the host side (#586): `[router]` in the config, compiled
//! into a [`tole_core::router::RuleRouter`].
//!
//! - **Off by default.** Nothing routes unless the user config says
//!   `[router] enabled = true`.
//! - **User config only for tiers and rules.** A project config can only
//!   turn routing off or narrow `allow_tiers`. Its rules and tiers are
//!   ignored with a warning, so a cloned repository cannot route to a model
//!   the user did not declare.
//! - **Load-time errors.** An unknown tier, kind or risk, a rule whose tier
//!   is not declared where it resolves, or an unknown profile is an error
//!   naming the remedy, never a silent fallback to another model.
//! - **Names only** in `tole doctor`; every string a dry run prints is
//!   redacted with the process secrets before it is capped.

use std::collections::BTreeMap;

use tole_core::router::{self, Brief, Route, Router, Rule, RuleRouter, Tier, Tiers, When};

use crate::config::{Config, RouterConfig, RouterRule};

/// Most rules a config may declare.
pub const MAX_RULES: usize = 64;
/// Most keywords per rule, and their longest length in characters.
pub const MAX_KEYWORDS: usize = 32;
pub const MAX_KEYWORD_CHARS: usize = 64;

fn valid_rule_name(n: &str) -> bool {
    (1..=64).contains(&n.len())
        && !n.starts_with('-')
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn tier_of(what: &str, s: &str) -> Result<Tier, String> {
    Tier::parse(s).ok_or_else(|| {
        format!(
            "{what}: unknown tier {s:?}: use light, standard or heavy (map them to models in \
             [router.tiers] or profiles.<name>.models)"
        )
    })
}

fn rule_of(r: &RouterRule) -> Result<Rule, String> {
    let at = format!("router rule `{}`", r.name);
    if !valid_rule_name(&r.name) {
        return Err(format!(
            "router rule name {:?} must be 1-64 chars of a-z, 0-9, `_` or `-`, not starting \
             with `-`",
            r.name
        ));
    }
    let tier = tier_of(&at, &r.tier)?;
    let kinds = r
        .kinds
        .iter()
        .flatten()
        .map(|k| {
            router::TaskKind::parse(k).ok_or_else(|| {
                format!("{at}: unknown kind {k:?}: use chat, run, mission, cron or delegate")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let risk = |v: &Option<String>| {
        v.as_deref()
            .map(|s| {
                router::parse_risk(s).ok_or_else(|| {
                    format!("{at}: unknown risk {s:?}: use read_only, write or destructive")
                })
            })
            .transpose()
    };
    let (min_risk, max_risk) = (risk(&r.min_risk)?, risk(&r.max_risk)?);
    if let (Some(a), Some(b)) = (r.min_chars, r.max_chars) {
        if a > b {
            return Err(format!("{at}: min_chars is above max_chars"));
        }
    }
    let keywords = r.keywords.clone().unwrap_or_default();
    if keywords.len() > MAX_KEYWORDS
        || keywords
            .iter()
            .any(|k| k.trim().is_empty() || k.chars().count() > MAX_KEYWORD_CHARS)
    {
        return Err(format!(
            "{at}: at most {MAX_KEYWORDS} keywords, each 1 to {MAX_KEYWORD_CHARS} characters"
        ));
    }
    let profiles = r.profiles.clone().unwrap_or_default();
    if let Some(p) = profiles
        .iter()
        .find(|p| !crate::config::valid_profile_name(p))
    {
        return Err(format!("{at}: {p:?} is not a profile name"));
    }
    Ok(Rule {
        name: r.name.clone(),
        tier,
        when: When {
            kinds,
            min_chars: r.min_chars,
            max_chars: r.max_chars,
            min_risk,
            max_risk,
            profiles,
            jobs: r.jobs.clone().unwrap_or_default(),
            budget_below: r.budget_below,
            keywords,
        },
    })
}

/// Parses `[router]` into a RuleRouter (syntax only; the tiers are checked
/// against the whole config by [`Routing::from_config`]).
pub fn compile(r: &RouterConfig) -> Result<RuleRouter, String> {
    let rules = r.rules.as_deref().unwrap_or_default();
    if rules.len() > MAX_RULES {
        return Err(format!("router: at most {MAX_RULES} rules"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for r in rules {
        let rule = rule_of(r)?;
        if !seen.insert(rule.name.clone()) {
            return Err(format!("router rule `{}` is declared twice", rule.name));
        }
        out.push(rule);
    }
    let allowed = r
        .allow_tiers
        .as_ref()
        .map(|a| {
            a.iter()
                .map(|t| tier_of("router.allow_tiers", t))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(RuleRouter::new(out, allowed))
}

/// Layers `[router]`: the user's tiers and rules; a project can only set
/// `enabled = false` and narrow `allow_tiers` (intersected with the
/// user's).
pub fn layered(
    project: &Option<RouterConfig>,
    user: &Option<RouterConfig>,
) -> Option<RouterConfig> {
    let mut out = user.clone()?;
    let Some(p) = project else {
        return Some(out);
    };
    if p.enabled == Some(false) {
        out.enabled = Some(false);
    }
    if let Some(pa) = &p.allow_tiers {
        out.allow_tiers = Some(match &out.allow_tiers {
            Some(ua) => ua.iter().filter(|t| pa.contains(t)).cloned().collect(),
            None => pa.clone(),
        });
    }
    Some(out)
}

/// The warning for a project `[router]` that tries to do more than narrow.
pub fn project_warning(project: &Config, shown: &str) -> Option<String> {
    let r = project.router.as_ref()?;
    let widen = r.enabled == Some(true) || r.tiers.is_some() || r.rules.is_some();
    widen.then(|| {
        format!(
            "ignoring router tiers, rules and `enabled = true` in project config {shown}: a \
             project config can only set `enabled = false` or narrow `allow_tiers`; declare \
             tiers and rules under [router] in the user config"
        )
    })
}

fn tiers_of(t: &Option<crate::config::RouterTiers>) -> Tiers {
    let t = t.clone().unwrap_or_default();
    Tiers {
        light: t.light,
        standard: t.standard,
        heavy: t.heavy,
    }
}

/// A checked, enabled router with the tier maps it resolves against.
#[derive(Debug, Clone)]
pub struct Routing {
    pub router: RuleRouter,
    /// Runs without a profile.
    pub tiers: Tiers,
    /// Each profile's tiers.
    pub profiles: BTreeMap<String, Tiers>,
}

impl Routing {
    /// `Ok(None)` when routing is off (no `[router]`, or not
    /// `enabled = true`). An error names the remedy.
    pub fn from_config(cfg: &Config) -> Result<Option<Routing>, String> {
        let Some(r) = cfg.router.as_ref().filter(|r| r.enabled == Some(true)) else {
            return Ok(None);
        };
        let router = compile(r)?;
        let tiers = tiers_of(&r.tiers);
        let profiles: BTreeMap<String, Tiers> = cfg
            .profiles
            .iter()
            .flatten()
            .map(|(n, p)| {
                let m = p.models.clone().unwrap_or_default();
                (
                    n.clone(),
                    Tiers {
                        light: m.light,
                        standard: m.standard,
                        heavy: m.heavy,
                    },
                )
            })
            .collect();
        router.check(|p| match p {
            None => Some(&tiers),
            Some(p) => profiles.get(p),
        })?;
        Ok(Some(Routing {
            router,
            tiers,
            profiles,
        }))
    }

    /// The route for `b` and the model it selects (`None`: keep the
    /// configured model).
    pub fn decide(&self, b: &Brief<'_>) -> Result<(Route, Option<String>), String> {
        let route = self.router.route(b);
        let tiers = match b.profile {
            None => &self.tiers,
            Some(p) => self
                .profiles
                .get(p)
                .ok_or_else(|| format!("router: unknown profile `{p}`"))?,
        };
        let model = route.model(tiers)?.map(str::to_string);
        Ok((route, model))
    }
}

/// The session register that holds a run's routing decision (#586).
pub const ROUTE: (&str, &str) = ("fact", "route");

/// Routes a NEW run once, before its first provider call (#586 part 2).
///
/// - Off (`Ok(None)`) unless the config enables routing.
/// - An explicit model wins: a session that already has `fact/model` is
///   left alone, and the cron caller skips a job with a `--model` pin.
/// - A broken router is an error here (the run's load time), naming the
///   remedy; the run must not start on some other model.
/// - Otherwise the decision is recorded in `fact/route`, redacted then
///   capped, and a routed model is stored as `fact/model`, so every face
///   and every resume uses it. Only the model changes: budget, allow list
///   and secrets are untouched.
///
/// Returns a one-line summary for stderr.
pub fn route_new_session(
    storage: &mut dyn tole_core::storage::Storage,
    cfg: &Config,
    brief: &Brief<'_>,
    secrets: &tole_core::secrets::SecretSet,
) -> Result<Option<String>, String> {
    let Some(rt) = Routing::from_config(cfg)? else {
        return Ok(None);
    };
    let pinned = storage
        .get_register(
            crate::session_settings::MODEL.0,
            crate::session_settings::MODEL.1,
        )
        .and_then(serde_json::Value::as_str)
        .is_some_and(|m| !m.trim().is_empty());
    if pinned {
        return Ok(None);
    }
    let (route, model) = rt.decide(brief)?;
    let commit = |s: &mut dyn tole_core::storage::Storage, w| {
        s.commit(tole_core::storage::Commit::new().register(w))
            .map(|_| ())
            .map_err(|e| format!("recording the route: {e}"))
    };
    if let Some(m) = &model {
        crate::session_settings::write_model(storage, m)
            .map_err(|e| format!("recording the routed model: {e}"))?;
    }
    commit(
        storage,
        tole_core::register::RegisterWrite::set(
            ROUTE.0,
            ROUTE.1,
            route.record(model.as_deref(), secrets),
        ),
    )?;
    let s = |v: &str| router::redact_cap(v, secrets, router::MAX_REASON_CHARS);
    Ok(Some(match (&route.tier, &model) {
        (Some(t), Some(m)) => format!("router: {} -> {} ({})", t.as_str(), s(m), s(&route.reason)),
        _ => format!("router: the configured model ({})", s(&route.reason)),
    }))
}

/// The kind of a `tole mission`: a delegated child (`TOLE_AGENT_DEPTH` of
/// 1 or more) or a top-level mission.
pub fn mission_kind(env: &dyn Fn(&str) -> Option<String>) -> router::TaskKind {
    let depth = env("TOLE_AGENT_DEPTH")
        .and_then(|d| d.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if depth >= 1 {
        router::TaskKind::Delegate
    } else {
        router::TaskKind::Mission
    }
}

/// `tole doctor` lines: state, rule and tier NAMES only (never models or
/// keywords).
pub fn doctor_lines(cfg: &Config) -> Vec<String> {
    let Some(r) = cfg.router.as_ref() else {
        return vec!["router: off (no [router] in the user config)".into()];
    };
    if r.enabled != Some(true) {
        return vec!["router: off ([router] enabled is not true)".into()];
    }
    let mut out = Vec::new();
    match Routing::from_config(cfg) {
        Err(e) => out.push(format!("router: ERROR {e}")),
        Ok(None) => out.push("router: off".into()),
        Ok(Some(rt)) => {
            let names: Vec<&str> = rt.router.rules().iter().map(|r| r.name.as_str()).collect();
            let declared: Vec<&str> = rt.tiers.declared().iter().map(|t| t.as_str()).collect();
            out.push(format!(
                "router: on (rule router), {} rule(s): {}",
                names.len(),
                if names.is_empty() {
                    "-".into()
                } else {
                    names.join(", ")
                }
            ));
            out.push(format!(
                "router tiers: {}",
                if declared.is_empty() {
                    "-".into()
                } else {
                    declared.join(", ")
                }
            ));
            if let Some(a) = rt.router.allowed() {
                let a: Vec<&str> = a.iter().map(|t| t.as_str()).collect();
                out.push(format!("router allowed tiers: {}", a.join(", ")));
            }
        }
    }
    out
}

/// What `tole route` prints: the decision for `b`, without calling a
/// provider. `configured` is the model the run would use without routing.
pub fn explain(
    cfg: &Config,
    b: &Brief<'_>,
    configured: Option<&str>,
    secrets: &tole_core::secrets::SecretSet,
) -> Result<String, String> {
    let s = |v: &str| router::redact_cap(v, secrets, router::MAX_REASON_CHARS);
    let Some(rt) = Routing::from_config(cfg)? else {
        return Ok(format!(
            "router: off; the run uses the configured model ({})\n",
            configured.map_or("unset".into(), s)
        ));
    };
    let (route, model) = rt.decide(b)?;
    let chosen = match (&model, configured) {
        (Some(m), _) => s(m),
        (None, Some(c)) => format!("{} (configured)", s(c)),
        (None, None) => "the configured model (unset)".into(),
    };
    Ok(format!(
        "tier: {}\nmodel: {chosen}\nreason: {}\n(dry run: no provider was called)\n",
        route.tier.map_or("- (default)", Tier::as_str),
        s(&route.reason)
    ))
}

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;
