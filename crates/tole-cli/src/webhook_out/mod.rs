//! Outbound webhook destinations (#911, design
//! `docs/webhook-delivery-design.md`): loading `[webhook_out.<name>]` from the
//! user config into checked destinations.
//!
//! A destination is a URL `tole` may push signed events to, plus the secret
//! that signs them (from an environment variable or a file, never inline) and
//! what it may reach. It is declared in the USER config only, and nothing
//! agent-written can name a URL, a header or a secret: a cron job refers to a
//! destination by its name. A destination that cannot load stops `tole serve`
//! with its name and the fix. The secret is never formatted: not in errors,
//! `Debug`, `config check`, doctor, logs or the ledger.
//!
//! This part only loads and checks. A [`Destinations`] holds no secret: the
//! sender resolves a key itself when it signs, so the map the cron tick
//! carries (under the host lock) cannot leak one. The secret IS checked here
//! (present, long enough, a plain file) and registered as a role secret, so it
//! is redacted wherever text leaves the process and scrubbed from children.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tole_core::web::AddrPolicy;

use crate::config::WebhookOutConfig;

pub mod event;
pub mod outbox;
pub mod sign;

/// Longest destination name.
pub const MAX_NAME: usize = 32;
/// Default whole-request time.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
/// Longest whole-request time a destination may set.
pub const MAX_TIMEOUT_SECS: u64 = 30;

/// `[a-z0-9_-]`, 1 to 32.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

/// What a destination may be sent. The names are the wire `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Event {
    /// A cron run settled; sent to the destinations a job names in `deliver`.
    CronRunFinished,
    /// An approval was queued; sent to destinations that list it.
    ApprovalRequested,
    /// An approval was decided or expired; sent to destinations that list it.
    ApprovalResolved,
}

impl Event {
    pub const ALL: [Event; 3] = [
        Event::CronRunFinished,
        Event::ApprovalRequested,
        Event::ApprovalResolved,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Event::CronRunFinished => "cron_run_finished",
            Event::ApprovalRequested => "approval_requested",
            Event::ApprovalResolved => "approval_resolved",
        }
    }

    pub fn parse(s: &str) -> Option<Event> {
        Event::ALL.into_iter().find(|e| e.as_str() == s)
    }
}

/// One checked destination. No secret.
#[derive(Clone, PartialEq, Eq)]
pub struct Destination {
    pub url: String,
    pub events: BTreeSet<Event>,
    pub policy: AddrPolicy,
    pub allow_agent_jobs: bool,
    pub timeout: Duration,
}

// The URL can carry a token in its path: `Debug` shows the shape only.
impl std::fmt::Debug for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Destination")
            .field("events", &self.events)
            .field("policy", &self.policy)
            .field("allow_agent_jobs", &self.allow_agent_jobs)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Every checked destination, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Destinations(BTreeMap<String, Destination>);

impl Destinations {
    pub fn get(&self, name: &str) -> Option<&Destination> {
        self.0.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Loads every destination. `env` reads an environment variable.
pub fn load(
    cfg: &BTreeMap<String, WebhookOutConfig>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Destinations, String> {
    let mut out = BTreeMap::new();
    for (name, w) in cfg {
        let err = |why: String| format!("webhook destination `{name}`: {why}");
        if !valid_name(name) {
            return Err(err(format!("the name must be 1-{MAX_NAME} of [a-z0-9_-]")));
        }
        let policy = AddrPolicy::new(
            w.allow_loopback == Some(true),
            w.allow_private == Some(true),
        );
        let url = w
            .url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| err("no `url`".into()))?;
        tole_core::web::check_destination_url(url, policy).map_err(|e| {
            err(match e {
                tole_core::web::PostError::SchemeRefused => {
                    "an http URL is only for a loopback host (localhost or a loopback IP) with \
                     allow_loopback = true; use https"
                        .to_string()
                }
                other => format!("`url` is refused ({other})"),
            })
        })?;

        let key =
            match (&w.hmac_key_env, &w.hmac_key_file) {
                (Some(_), Some(_)) => {
                    return Err(err(
                        "set one of hmac_key_env and hmac_key_file, not both".into()
                    ))
                }
                (None, None) => return Err(err(
                    "no secret: set hmac_key_env (a variable name) or hmac_key_file (a path); the \
                     secret itself is never written in the config"
                        .into(),
                )),
                (Some(var), None) => env(var)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| err(format!("hmac_key_env names `{var}`, which is not set")))?
                    .into_bytes(),
                (None, Some(path)) => {
                    // A relative path would resolve against whatever
                    // directory the process runs in, e.g. a cloned repo.
                    if !std::path::Path::new(path).is_absolute() {
                        return Err(err("hmac_key_file must be an absolute path".into()));
                    }
                    crate::hooks::read_secret_file(path).map_err(err)?
                }
            };
        if key.len() < crate::hooks::MIN_KEY {
            return Err(err(format!(
                "the secret is shorter than {} bytes",
                crate::hooks::MIN_KEY
            )));
        }
        // Held by role from here on: redacted on egress, scrubbed from
        // children. A secret that is not UTF-8 cannot appear in text.
        if let Ok(text) = std::str::from_utf8(&key) {
            tole_core::secrets::add_role_secret(&format!("webhook_out.{name}"), text);
        }

        let events = match &w.events {
            None => BTreeSet::from([Event::CronRunFinished]),
            Some(list) => {
                let mut set = BTreeSet::new();
                for e in list {
                    set.insert(Event::parse(e).ok_or_else(|| {
                        err(format!(
                            "unknown event `{}` (known: {})",
                            printable(e),
                            Event::ALL.map(Event::as_str).join(", ")
                        ))
                    })?);
                }
                set
            }
        };
        let secs = w.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(1..=MAX_TIMEOUT_SECS).contains(&secs) {
            return Err(err(format!("timeout_secs must be 1-{MAX_TIMEOUT_SECS}")));
        }
        out.insert(
            name.clone(),
            Destination {
                url: url.trim().to_string(),
                events,
                policy,
                allow_agent_jobs: w.allow_agent_jobs == Some(true),
                timeout: Duration::from_secs(secs),
            },
        );
    }
    Ok(Destinations(out))
}

/// The user config's destinations, loaded and checked. Empty when there is
/// no user config or no `[webhook_out]`. A broken config is an error here:
/// the caller decides whether that stops it (`tole serve`) or is reported.
pub fn load_user() -> Result<Destinations, String> {
    let cfg = match crate::config::user_config_path() {
        Some(p) if crate::config::user_config_present(&p)? => crate::config::load(&p)
            .map_err(|e| e.to_string())?
            .webhook_out
            .unwrap_or_default(),
        _ => BTreeMap::new(),
    };
    load(&cfg, &|k| std::env::var(k).ok())
}

/// The destinations this process loaded at start. `tole serve` installs them
/// before it listens (a destination that cannot load stops it); a standalone
/// `tole cron tick` or `run` installs them on first use. The cron tick reads
/// the map from here, so a config edit applies on the next start.
static LOADED: std::sync::OnceLock<std::sync::Arc<Destinations>> = std::sync::OnceLock::new();

/// Installs the process's destinations; the first install wins.
pub fn install(dests: Destinations) -> std::sync::Arc<Destinations> {
    std::sync::Arc::clone(LOADED.get_or_init(|| std::sync::Arc::new(dests)))
}

/// The installed destinations, if any.
pub fn loaded() -> Option<std::sync::Arc<Destinations>> {
    LOADED.get().cloned()
}

/// A name a config gave, for an error: printable ASCII only, cut short, so
/// hostile text cannot shape the message.
fn printable(s: &str) -> String {
    s.chars()
        .take(40)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

/// `tole doctor` lines for the destinations (#911): names, events and flags,
/// never a URL, a header or a secret value. Nothing is printed
/// when the user config defines none. A destination that does not load is
/// said with its name and the fix; a long-running `tole serve` keeps the map
/// it loaded until restart.
pub fn doctor_lines(cfg: &crate::config::Config) -> Vec<String> {
    doctor_lines_with(cfg, &|k| std::env::var(k).ok())
}

/// [`doctor_lines`] with an explicit environment (the tests').
pub fn doctor_lines_with(
    cfg: &crate::config::Config,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let Some(all) = cfg.webhook_out.as_ref().filter(|w| !w.is_empty()) else {
        return Vec::new();
    };
    match load(all, env) {
        Ok(dests) => {
            let items: Vec<String> = dests
                .0
                .iter()
                .map(|(name, d)| {
                    let events: Vec<&str> = d.events.iter().map(|e| e.as_str()).collect();
                    let mut flags = Vec::new();
                    if d.policy.loopback {
                        flags.push("loopback");
                    }
                    if d.policy.private {
                        flags.push("private");
                    }
                    if d.allow_agent_jobs {
                        flags.push("agent jobs");
                    }
                    let flags = if flags.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", flags.join(", "))
                    };
                    format!("{name} ({}{flags})", events.join(", "))
                })
                .collect();
            vec![format!(
                "webhook destinations: {} (a running `tole serve` applies edits on its next start)",
                items.join("; ")
            )]
        }
        Err(e) => vec![format!("webhook destinations: NOT LOADED: {e}")],
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
