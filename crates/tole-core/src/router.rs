//! Model routing by task weight (#586, design
//! `profiles-collaboration-secrets.md` A9.3): the [`Router`] seam and its
//! deterministic baseline, [`RuleRouter`].
//!
//! A router picks a model TIER (`light`, `standard`, `heavy`), never a model
//! name. The owner's config maps tiers to models, so a rule cannot name a
//! model the config did not declare. A [`Route`] carries only the tier, the
//! rule that chose it and why: routing cannot touch a run's budget, allow
//! list or secrets, because a route has no field for them.
//!
//! Rules:
//! - **Safe default.** When no rule matches, the route has no tier and the
//!   caller keeps the model it would have used anyway (the configured one),
//!   never a cheaper or riskier one.
//! - **Ties go to the cheaper tier.** Every matching rule is a candidate and
//!   the cheapest tier wins (the routing-collapse lesson); among rules of
//!   that tier, the first one declared names the route.
//! - **Narrowing.** An allowed-tier set (a project config can only narrow
//!   it) drops matches on other tiers before the pick.
//! - **Deterministic.** The same brief and rules give the same route.
//! - **Recorded redacted.** [`Route::record`] redacts with the process
//!   secrets before it caps.
//!
//! Platform-agnostic, no new dependency, no I/O.

use crate::secrets::SecretSet;
use crate::tool::Risk;

/// A model tier, cheapest first (the order is the cost order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    Light,
    Standard,
    Heavy,
}

impl Tier {
    /// Every tier, cheapest first.
    pub const ALL: [Tier; 3] = [Tier::Light, Tier::Standard, Tier::Heavy];

    pub fn parse(s: &str) -> Option<Tier> {
        match s.trim() {
            "light" => Some(Tier::Light),
            "standard" => Some(Tier::Standard),
            "heavy" => Some(Tier::Heavy),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Light => "light",
            Tier::Standard => "standard",
            Tier::Heavy => "heavy",
        }
    }
}

/// What kind of run is being routed. Routing happens once per run (a
/// session start, a cron run, a delegated child), never per turn: a model
/// switch mid-session discards the cached prompt prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    /// An interactive session (`tole chat`, the TUI).
    Chat,
    /// A one-shot `tole run`.
    Run,
    /// A `tole mission`.
    Mission,
    /// A cron agent run, scheduled or fired by a webhook.
    Cron,
    /// A delegated child.
    Delegate,
}

impl TaskKind {
    pub const ALL: [TaskKind; 5] = [
        TaskKind::Chat,
        TaskKind::Run,
        TaskKind::Mission,
        TaskKind::Cron,
        TaskKind::Delegate,
    ];

    pub fn parse(s: &str) -> Option<TaskKind> {
        TaskKind::ALL.into_iter().find(|k| k.as_str() == s.trim())
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Chat => "chat",
            TaskKind::Run => "run",
            TaskKind::Mission => "mission",
            TaskKind::Cron => "cron",
            TaskKind::Delegate => "delegate",
        }
    }
}

/// `read_only`, `write` or `destructive`.
pub fn parse_risk(s: &str) -> Option<Risk> {
    match s.trim() {
        "read_only" => Some(Risk::ReadOnly),
        "write" => Some(Risk::Write),
        "destructive" => Some(Risk::Destructive),
        _ => None,
    }
}

fn risk_rank(r: Risk) -> u8 {
    match r {
        Risk::ReadOnly => 0,
        Risk::Write => 1,
        Risk::Destructive => 2,
    }
}

fn risk_name(r: Risk) -> &'static str {
    match r {
        Risk::ReadOnly => "read_only",
        Risk::Write => "write",
        Risk::Destructive => "destructive",
    }
}

/// What a router sees about a run: owner-side facts only, never tool
/// output.
#[derive(Debug, Clone, Copy)]
pub struct Brief<'a> {
    pub kind: TaskKind,
    /// The task text; only its size and the rules' keywords are read.
    pub prompt: &'a str,
    /// The highest risk among the tools the run may use.
    pub max_risk: Risk,
    /// The run's profile, if any.
    pub profile: Option<&'a str>,
    /// The cron job's name, if any.
    pub job: Option<&'a str>,
    /// Tokens the run may still spend, when it has a budget.
    pub budget_left: Option<u64>,
}

/// When a rule applies. Every set condition must hold; an unset one always
/// holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct When {
    /// Run kinds (empty: any).
    pub kinds: Vec<TaskKind>,
    /// Prompt length bounds, in characters (inclusive).
    pub min_chars: Option<usize>,
    pub max_chars: Option<usize>,
    /// Bounds on the run's highest tool risk (inclusive).
    pub min_risk: Option<Risk>,
    pub max_risk: Option<Risk>,
    /// Profiles this rule applies to. Empty: runs WITHOUT a profile only, so
    /// a rule's tier is always checked against the tiers that will resolve
    /// it.
    pub profiles: Vec<String>,
    /// Cron job names (empty: any run, job or not).
    pub jobs: Vec<String>,
    /// Holds when the run's budget left is below this many tokens.
    pub budget_below: Option<u64>,
    /// Holds when the prompt contains any of these (case-insensitive).
    pub keywords: Vec<String>,
}

/// One declared rule.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub name: String,
    pub tier: Tier,
    pub when: When,
}

impl Rule {
    /// Whether every condition holds for `b`.
    pub fn matches(&self, b: &Brief<'_>) -> bool {
        let w = &self.when;
        let chars = b.prompt.chars().count();
        let risk = risk_rank(b.max_risk);
        let profile_ok = match b.profile {
            None => w.profiles.is_empty(),
            Some(p) => w.profiles.iter().any(|x| x == p),
        };
        let prompt = b.prompt.to_lowercase();
        (w.kinds.is_empty() || w.kinds.contains(&b.kind))
            && w.min_chars.is_none_or(|n| chars >= n)
            && w.max_chars.is_none_or(|n| chars <= n)
            && w.min_risk.is_none_or(|r| risk >= risk_rank(r))
            && w.max_risk.is_none_or(|r| risk <= risk_rank(r))
            && profile_ok
            && (w.jobs.is_empty() || b.job.is_some_and(|j| w.jobs.iter().any(|x| x == j)))
            && w.budget_below
                .is_none_or(|n| b.budget_left.is_some_and(|left| left < n))
            && (w.keywords.is_empty()
                || w.keywords
                    .iter()
                    .any(|k| prompt.contains(&k.to_lowercase())))
    }

    /// The rule's conditions, for a reason line (config values only, never
    /// the prompt).
    fn conditions(&self) -> String {
        let w = &self.when;
        let mut c: Vec<String> = Vec::new();
        if !w.kinds.is_empty() {
            let k: Vec<&str> = w.kinds.iter().map(|k| k.as_str()).collect();
            c.push(format!("kind {}", k.join("|")));
        }
        if let Some(n) = w.min_chars {
            c.push(format!(">= {n} chars"));
        }
        if let Some(n) = w.max_chars {
            c.push(format!("<= {n} chars"));
        }
        if let Some(r) = w.min_risk {
            c.push(format!("risk >= {}", risk_name(r)));
        }
        if let Some(r) = w.max_risk {
            c.push(format!("risk <= {}", risk_name(r)));
        }
        if !w.profiles.is_empty() {
            c.push(format!("profile {}", w.profiles.join("|")));
        }
        if !w.jobs.is_empty() {
            c.push(format!("job {}", w.jobs.join("|")));
        }
        if let Some(n) = w.budget_below {
            c.push(format!("budget left < {n}"));
        }
        if !w.keywords.is_empty() {
            c.push(format!("keyword {}", w.keywords.join("|")));
        }
        if c.is_empty() {
            "always".into()
        } else {
            c.join(", ")
        }
    }
}

/// The tier-to-model map one run resolves against (a profile's tiers, or
/// the router's own for runs without a profile).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tiers {
    pub light: Option<String>,
    pub standard: Option<String>,
    pub heavy: Option<String>,
}

impl Tiers {
    /// The model of `tier`, when it is declared (blank counts as not).
    pub fn get(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Light => self.light.as_deref(),
            Tier::Standard => self.standard.as_deref(),
            Tier::Heavy => self.heavy.as_deref(),
        }
        .filter(|m| !m.trim().is_empty())
    }

    /// The declared tiers, cheapest first.
    pub fn declared(&self) -> Vec<Tier> {
        Tier::ALL
            .into_iter()
            .filter(|t| self.get(*t).is_some())
            .collect()
    }
}

/// Who decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A rule matched (its name).
    Rule(String),
    /// No rule matched: the configured model stays.
    Default,
}

/// A routing decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// `None`: keep the configured model.
    pub tier: Option<Tier>,
    pub source: Source,
    pub reason: String,
}

/// Longest reason recorded or printed, in characters.
pub const MAX_REASON_CHARS: usize = 300;

/// `s` redacted with `secrets`, THEN capped at `max` characters, so a cap
/// never leaves part of a secret behind.
pub fn redact_cap(s: &str, secrets: &SecretSet, max: usize) -> String {
    let red = secrets.redact(s);
    if red.chars().count() <= max {
        return red.into_owned();
    }
    let mut out: String = red.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl Route {
    /// The decision as a JSON object for a session register or ledger
    /// line: every string redacted, then capped.
    pub fn record(&self, model: Option<&str>, secrets: &SecretSet) -> serde_json::Value {
        let s = |v: &str| redact_cap(v, secrets, MAX_REASON_CHARS);
        serde_json::json!({
            "tier": self.tier.map(Tier::as_str),
            "model": model.map(s),
            "source": match &self.source {
                Source::Rule(n) => format!("rule:{}", s(n)),
                Source::Default => "default".to_string(),
            },
            "reason": s(&self.reason),
        })
    }

    /// The model this route selects from `tiers`: `Ok(None)` keeps the
    /// configured model. A tier the map does not declare is an error, never
    /// a fallback to another model (load-time checks make it unreachable).
    pub fn model<'t>(&self, tiers: &'t Tiers) -> Result<Option<&'t str>, String> {
        match self.tier {
            None => Ok(None),
            Some(t) => tiers.get(t).map(Some).ok_or_else(|| {
                format!(
                    "router: tier `{}` is not declared; add it to the tiers this run uses",
                    t.as_str()
                )
            }),
        }
    }
}

/// The seam: a deterministic baseline today, a learned router (laya) later,
/// only after it beats this baseline in the evals.
pub trait Router {
    fn route(&self, brief: &Brief<'_>) -> Route;
}

/// The baseline router: declared rules, cheapest matching tier.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleRouter {
    rules: Vec<Rule>,
    /// `Some`: only these tiers may be picked (a project config's narrowing).
    allowed: Option<Vec<Tier>>,
}

impl RuleRouter {
    pub fn new(rules: Vec<Rule>, allowed: Option<Vec<Tier>>) -> RuleRouter {
        RuleRouter { rules, allowed }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn allowed(&self) -> Option<&[Tier]> {
        self.allowed.as_deref()
    }

    /// Load-time check: every rule's tier is declared where it resolves.
    /// `tiers_for(None)` is the map of runs without a profile,
    /// `tiers_for(Some(p))` profile `p`'s (`None` when `p` is unknown).
    pub fn check<'a>(
        &self,
        tiers_for: impl Fn(Option<&str>) -> Option<&'a Tiers>,
    ) -> Result<(), String> {
        for r in &self.rules {
            let t = r.tier.as_str();
            if r.when.profiles.is_empty() {
                if tiers_for(None).is_none_or(|m| m.get(r.tier).is_none()) {
                    return Err(format!(
                        "router rule `{}` picks tier `{t}`, which [router.tiers] does not declare: \
                         add `{t} = \"<model>\"` under [router.tiers] in the user config, or \
                         change the rule's tier",
                        r.name
                    ));
                }
                continue;
            }
            for p in &r.when.profiles {
                match tiers_for(Some(p)) {
                    None => {
                        return Err(format!(
                            "router rule `{}` names unknown profile `{p}`: define \
                             [profiles.{p}] in the user config or drop it from the rule",
                            r.name
                        ))
                    }
                    Some(m) if m.get(r.tier).is_none() => {
                        return Err(format!(
                            "router rule `{}` picks tier `{t}`, which profile `{p}` does not \
                             declare: set profiles.{p}.models.{t} in the user config, or change \
                             the rule's tier",
                            r.name
                        ))
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }
}

impl Router for RuleRouter {
    fn route(&self, brief: &Brief<'_>) -> Route {
        let allowed = |t: Tier| self.allowed.as_ref().is_none_or(|a| a.contains(&t));
        // Cheapest tier wins; `min_by_key` keeps the first of equal keys, so
        // the first declared rule of that tier names the route.
        let pick = self
            .rules
            .iter()
            .filter(|r| allowed(r.tier) && r.matches(brief))
            .min_by_key(|r| r.tier);
        match pick {
            Some(r) => Route {
                tier: Some(r.tier),
                source: Source::Rule(r.name.clone()),
                reason: format!(
                    "rule `{}` ({}) picks {}",
                    r.name,
                    r.conditions(),
                    r.tier.as_str()
                ),
            },
            None => Route {
                tier: None,
                source: Source::Default,
                reason: "no rule matched; the configured model stays".into(),
            },
        }
    }
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
