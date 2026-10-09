//! Project config application (#208, part 3a of 4): startup wiring of the
//! trust gate, the flag > env > config > default precedence framework and
//! the LOW-RISK keys only (`model`, `base_url`, `system_prompt`, `memory`,
//! `sessions_dir`, `workspace`, `[mission]` budgets).
//!
//! The security-sensitive keys (`trust`, `allow`, `mcp_server`, the hook
//! lists, `skill`, `plan_mode`, `no_auto_mcp`, `no_skills`, `mission.verify`,
//! `mission.verify_timeout`) are validated by [`crate::config`] but are
//! NEVER applied here — that is part 3b.
//!
//! Everything is a pure function of its inputs (flags, an injected env
//! lookup, the parsed [`Config`]) so `tole config check` and startup run the
//! SAME resolution and cannot drift. The only process-wide state is the
//! write-once [`install_fill`] cell, which carries the config's
//! `model`/`base_url`/`system_prompt` to the deep server faces; it is empty
//! (and every reader falls through to today's env-only behavior) unless the
//! binary installs it after a config was loaded and gated.

use crate::config::{Config, ConfigError};
use crate::config_trust::{self, ConfigTrustIo, Origin, StdIo, Subject};
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tole_core::openai::{OpenAiConfig, ENV_PREFIXES};

/// Where sessions live unless a flag / the config overrides it.
pub const DEFAULT_SESSIONS_DIR: &str = ".tole/sessions";

// ------------------------------------------------------------ precedence

/// Which layer supplied a resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Config,
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Flag => "flag",
            Source::Env => "env",
            Source::Config => "config",
            Source::Default => "default",
        })
    }
}

/// An empty / whitespace-only string counts as unset (the existing env
/// convention).
pub fn nonblank(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// flag > env > config. `None` when no layer has a value (the caller then
/// applies its default, or has none). Part 3b adds `resolve_list` (a higher
/// layer replaces the whole list) and the "flags can only turn on" boolean
/// next to this; both reuse [`Source`].
pub fn resolve<T>(flag: Option<T>, env: Option<T>, cfg: Option<T>) -> Option<(T, Source)> {
    flag.map(|v| (v, Source::Flag))
        .or_else(|| env.map(|v| (v, Source::Env)))
        .or_else(|| cfg.map(|v| (v, Source::Config)))
}

/// [`resolve`] with a final default.
pub fn resolve_or<T>(flag: Option<T>, env: Option<T>, cfg: Option<T>, default: T) -> (T, Source) {
    resolve(flag, env, cfg).unwrap_or((default, Source::Default))
}

/// String flavor: blank env / config values count as unset. The flag is
/// taken as given (per-key flag blank rules stay with the caller).
pub fn resolve_string(
    flag: Option<String>,
    env: Option<String>,
    cfg: Option<String>,
) -> Option<(String, Source)> {
    resolve(flag, nonblank(env), nonblank(cfg))
}

/// [`resolve_string`] with a default.
pub fn resolve_string_or(
    flag: Option<String>,
    env: Option<String>,
    cfg: Option<String>,
    default: &str,
) -> (String, Source) {
    resolve_string(flag, env, cfg).unwrap_or((default.to_string(), Source::Default))
}

/// Real environment lookup for [`Settings::resolve`].
pub fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

// -------------------------------------------------------------- provider

const BASE_URL_ENV: [&str; 2] = ["TOLE_BASE_URL", "OPENAI_BASE_URL"];
const MODEL_ENV: [&str; 2] = ["TOLE_MODEL", "OPENAI_MODEL"];
const API_KEY_ENV: [&str; 2] = ["TOLE_API_KEY", "OPENAI_API_KEY"];

/// A resolved value with its layer and (for env) the variable name.
#[derive(Debug, Clone, PartialEq)]
pub struct Eff<T> {
    pub value: T,
    pub source: Source,
    pub env: Option<&'static str>,
}

impl<T> Eff<T> {
    fn new(value: T, source: Source) -> Self {
        Eff {
            value,
            source,
            env: None,
        }
    }
    fn env(value: T, name: &'static str) -> Self {
        Eff {
            value,
            source: Source::Env,
            env: Some(name),
        }
    }
    fn from_pair((value, source): (T, Source), env_name: &'static str) -> Self {
        Eff {
            value,
            source,
            env: (source == Source::Env).then_some(env_name),
        }
    }
    /// `(env TOLE_MODEL)`, `(config)`, `(flag)`, `(default)`.
    pub fn describe(&self) -> String {
        match self.env {
            Some(n) => format!("({} {n})", self.source),
            None => format!("({})", self.source),
        }
    }
}

/// What the provider env + config resolve to. The API key only ever comes
/// from the environment and is never part of this struct's display.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResolution {
    pub base_url: Option<Eff<String>>,
    pub model: Option<Eff<String>>,
    /// The full provider config when a usable triple exists. Carries the
    /// API key: never print it (its `Debug` is redacted by tole-core).
    pub config: Option<OpenAiConfig>,
}

fn env_nonblank(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve the provider triple.
///
/// Pass 1 is EXACTLY [`OpenAiConfig::from_env`]: the first prefix whose
/// whole triple is in the environment wins and the config file is not
/// consulted (env beats config; prefixes are never mixed). Only when no
/// prefix is complete may the config's `base_url`/`model` fill the gaps of
/// the first prefix that has an API key. With no config values the result
/// is therefore identical to `from_env()`.
pub fn resolve_provider(
    cfg_base_url: Option<&str>,
    cfg_model: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> ProviderResolution {
    let cfg_base = nonblank(cfg_base_url.map(str::to_string));
    let cfg_model = nonblank(cfg_model.map(str::to_string));
    for i in 0..ENV_PREFIXES.len() {
        if let (Some(b), Some(m), Some(k)) = (
            env_nonblank(env, BASE_URL_ENV[i]),
            env_nonblank(env, MODEL_ENV[i]),
            env_nonblank(env, API_KEY_ENV[i]),
        ) {
            return ProviderResolution {
                base_url: Some(Eff::env(b.clone(), BASE_URL_ENV[i])),
                model: Some(Eff::env(m.clone(), MODEL_ENV[i])),
                config: Some(OpenAiConfig::new(b, m, k)),
            };
        }
    }
    if cfg_base.is_some() || cfg_model.is_some() {
        for i in 0..ENV_PREFIXES.len() {
            let Some(k) = env_nonblank(env, API_KEY_ENV[i]) else {
                continue;
            };
            let b = resolve(None, env_nonblank(env, BASE_URL_ENV[i]), cfg_base.clone());
            let m = resolve(None, env_nonblank(env, MODEL_ENV[i]), cfg_model.clone());
            if let (Some(b), Some(m)) = (b, m) {
                return ProviderResolution {
                    config: Some(OpenAiConfig::new(b.0.clone(), m.0.clone(), k)),
                    base_url: Some(Eff::from_pair(b, BASE_URL_ENV[i])),
                    model: Some(Eff::from_pair(m, MODEL_ENV[i])),
                };
            }
        }
    }
    // No usable triple: report the best candidate per field (display only).
    let pick = |names: &[&'static str; 2], cfgv: &Option<String>| {
        for n in names {
            if let Some(v) = env_nonblank(env, n) {
                return Some(Eff::env(v, n));
            }
        }
        cfgv.clone().map(|v| Eff::new(v, Source::Config))
    };
    ProviderResolution {
        base_url: pick(&BASE_URL_ENV, &cfg_base),
        model: pick(&MODEL_ENV, &cfg_model),
        config: None,
    }
}

// ------------------------------------------------------ process-wide fill

/// The config's low-risk provider-side values, installed once at startup
/// (after the gate) so the deep server faces see them without threading a
/// parameter through every layer. Absent = today's behavior.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderFill {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
}

static FILL: OnceLock<ProviderFill> = OnceLock::new();

/// Install the fill (first call wins; later calls are ignored).
pub fn install_fill(fill: ProviderFill) {
    let _ = FILL.set(fill);
}

fn fill() -> ProviderFill {
    FILL.get().cloned().unwrap_or_default()
}

/// The provider config every session-building site must use instead of
/// `OpenAiConfig::from_env()`: identical when no config was installed.
pub fn provider_config() -> Option<OpenAiConfig> {
    let f = fill();
    resolve_provider(f.base_url.as_deref(), f.model.as_deref(), &real_env).config
}

/// The config's `model`, for hosts that need the model name without a
/// full provider (the ACP advertised "current model").
pub fn config_model() -> Option<String> {
    nonblank(fill().model)
}

/// The config's `system_prompt` as the fallback under `TOLE_SYSTEM_PROMPT`
/// for the server faces.
pub fn config_system_prompt() -> Option<String> {
    nonblank(fill().system_prompt)
}

// ------------------------------------------------------------- settings

/// What the command line supplied for the keys applied in this part.
/// `memory` and the mission budgets are `None` for commands without them.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub sessions_dir: Option<String>,
    pub workspace: Option<String>,
    pub memory: Option<String>,
    /// `--system` (run/chat).
    pub system: Option<String>,
    pub max_steps: Option<u64>,
    pub max_minutes: Option<u64>,
    pub max_tokens: Option<u64>,
}

/// The resolved low-risk settings. Security-sensitive keys have NO field
/// here by design (they are not applied until part 3b).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub sessions_dir: Eff<String>,
    pub workspace: Option<Eff<String>>,
    pub memory: Option<Eff<String>>,
    pub system_prompt: Option<Eff<String>>,
    pub provider: ProviderResolution,
    pub max_steps: Option<Eff<u64>>,
    pub max_minutes: Option<Eff<u64>>,
    pub max_tokens: Option<Eff<u64>>,
}

impl Settings {
    /// THE resolution: startup and `config check` both call this.
    pub fn resolve(flags: &Flags, cfg: &Config, env: &dyn Fn(&str) -> Option<String>) -> Settings {
        let (sd, sd_src) = resolve_or(
            flags.sessions_dir.clone(),
            None,
            nonblank(cfg.sessions_dir.clone()),
            DEFAULT_SESSIONS_DIR.to_string(),
        );
        let mission = cfg.mission.clone().unwrap_or_default();
        let num =
            |flag: Option<u64>, c: Option<u64>| resolve(flag, None, c).map(|(v, s)| Eff::new(v, s));
        Settings {
            sessions_dir: Eff::new(sd, sd_src),
            workspace: resolve_string(flags.workspace.clone(), None, cfg.workspace.clone())
                .map(|(v, s)| Eff::new(v, s)),
            memory: resolve_string(
                nonblank(flags.memory.clone()),
                env("TOLE_MEMORY"),
                cfg.memory.clone(),
            )
            .map(|(v, s)| Eff::from_pair((v.trim().to_string(), s), "TOLE_MEMORY")),
            system_prompt: resolve_string(
                // `--system ""` stays an explicit (empty) prompt, as before.
                flags.system.clone(),
                env("TOLE_SYSTEM_PROMPT"),
                cfg.system_prompt.clone(),
            )
            .map(|p| Eff::from_pair(p, "TOLE_SYSTEM_PROMPT")),
            provider: resolve_provider(cfg.base_url.as_deref(), cfg.model.as_deref(), env),
            max_steps: num(flags.max_steps, mission.max_steps),
            max_minutes: num(flags.max_minutes, mission.max_minutes),
            max_tokens: num(flags.max_tokens, mission.max_tokens),
        }
    }

    /// The fill to install for the deep server faces: ONLY the config's own
    /// values (the faces still apply env first).
    pub fn fill_from(cfg: &Config) -> ProviderFill {
        ProviderFill {
            base_url: nonblank(cfg.base_url.clone()),
            model: nonblank(cfg.model.clone()),
            system_prompt: nonblank(cfg.system_prompt.clone()),
        }
    }
}

// ---------------------------------------------------------- check output

const PROMPT_DISPLAY_CHARS: usize = 80;

fn show_str(s: &str) -> String {
    format!("{s:?}")
}

fn show_prompt(p: &str) -> String {
    let total = p.chars().count();
    if total > PROMPT_DISPLAY_CHARS {
        let head: String = p.chars().take(PROMPT_DISPLAY_CHARS).collect();
        format!("{}… ({total} chars)", show_str(&head))
    } else {
        show_str(p)
    }
}

const NOT_APPLIED: &str = "  (parsed, not applied yet — 3b)";

/// Per-key suffixes for `tole config check`: applied keys get the
/// effective value + its source, security-sensitive ones the "not applied"
/// marker. Keys are the names [`crate::config::render_with`] uses.
pub fn annotations(s: &Settings) -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    let eff = |e: &Option<Eff<String>>, show: &dyn Fn(&str) -> String| match e {
        Some(e) => format!("  effective: {}  {}", show(&e.value), e.describe()),
        None => "  effective: (unset)".to_string(),
    };
    let num = |e: &Option<Eff<u64>>| match e {
        Some(e) => format!("  effective: {}  {}", e.value, e.describe()),
        None => "  effective: (budget tier default)".to_string(),
    };
    m.insert("model", eff(&s.provider.model, &show_str));
    m.insert("base_url", eff(&s.provider.base_url, &show_str));
    m.insert(
        "sessions_dir",
        format!(
            "  effective: {}  {}",
            show_str(&s.sessions_dir.value),
            s.sessions_dir.describe()
        ),
    );
    m.insert("workspace", eff(&s.workspace, &show_str));
    #[cfg(feature = "shell-tools")]
    m.insert("memory", eff(&s.memory, &show_str));
    #[cfg(not(feature = "shell-tools"))]
    m.insert(
        "memory",
        "  (parsed, not applied — needs the shell-tools feature)".to_string(),
    );
    m.insert("system_prompt", eff(&s.system_prompt, &show_prompt));
    m.insert("mission.max_steps", num(&s.max_steps));
    m.insert("mission.max_minutes", num(&s.max_minutes));
    m.insert("mission.max_tokens", num(&s.max_tokens));
    for k in [
        "plan_mode",
        "trust",
        "allow",
        "mcp_server",
        "no_auto_mcp",
        "on_pretool",
        "on_posttool",
        "on_turnend",
        "skill",
        "no_skills",
        "mission.verify",
        "mission.verify_timeout",
    ] {
        m.insert(k, NOT_APPLIED.to_string());
    }
    m
}

/// `tole config check`: validate + render with effective values. Runs the
/// same [`Settings::resolve`] as startup.
pub fn check(
    cwd: &Path,
    explicit: Option<&Path>,
    flags: &Flags,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    crate::config::check(cwd, explicit, &|cfg| {
        annotations(&Settings::resolve(flags, cfg, env))
    })
}

// --------------------------------------------------------------- startup

/// A config that passed the gate and validation.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    /// Canonical path shown to the user.
    pub shown: PathBuf,
    pub config: Config,
}

/// The startup IO: real stderr/stdin, but a question is only ever asked
/// when `allow_prompt` (the command is interactive and its stdin is not
/// the prompt/protocol channel).
pub struct StartupIo {
    pub allow_prompt: bool,
    inner: StdIo,
}

impl StartupIo {
    pub fn new(allow_prompt: bool) -> Self {
        StartupIo {
            allow_prompt,
            inner: StdIo,
        }
    }
}

impl ConfigTrustIo for StartupIo {
    fn is_interactive(&self) -> bool {
        self.allow_prompt && self.inner.is_interactive()
    }
    fn inform(&mut self, text: &str) {
        self.inner.inform(text)
    }
    fn warn(&mut self, text: &str) {
        self.inner.warn(text)
    }
    fn confirm(&mut self, prompt_text: &str) -> bool {
        self.inner.confirm(prompt_text)
    }
}

/// Discover, gate and parse the project config. `Ok(None)` when there is
/// no config (nothing is read, printed or looked up — in particular the
/// trust store location is not even resolved). The GATE'S vetted bytes are
/// parsed — the file is never re-read after vetting. On success one
/// `tole: using config <path>` line goes to `io.inform` (stderr).
pub fn load_project_config(
    cwd: &Path,
    explicit: Option<&Path>,
    store_path: &dyn Fn() -> Result<PathBuf>,
    io: &mut dyn ConfigTrustIo,
) -> Result<Option<Loaded>> {
    if crate::config::discover(cwd, explicit).is_none() {
        return Ok(None);
    }
    let (subject, origin) = Subject::resolve(cwd, explicit)?;
    let store = match origin {
        Origin::Discovered => store_path()?,
        Origin::Explicit => PathBuf::new(),
    };
    let outcome = config_trust::gate(&subject, origin, &store, io)?;
    let shown = subject.shown();
    let config = crate::config::parse(&outcome.content, &shown).map_err(|e| anyhow!("{e}"))?;
    io.inform(&format!(
        "tole: using config {}",
        config_trust::sanitize(&shown.display().to_string())
    ));
    Ok(Some(Loaded { shown, config }))
}

#[cfg(test)]
#[path = "config_apply_tests.rs"]
mod tests;
