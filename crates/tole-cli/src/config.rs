//! Config files (#208): LOAD + VALIDATE. Applying the keys (precedence,
//! trust gate at startup) lives in `config_apply`.
//!
//! Two layers share one schema (#433): the project config
//! `.tole/config.toml` and the user config `$CODECORA_HOME/tole/config.toml`
//! (default `~/.codecora/tole/config.toml`). The project layer wins key by
//! key ([`layered`]); the user layer holds a user's global defaults.
//!
//! The project file is untrusted input (it lives in a cloned repo), so the loader
//! is strict: a 64 KiB bounded read, a pre-check that refuses secret-like
//! keys at any depth, trust-preset names validated by the SAME function
//! the `--trust` flag uses, and `deny_unknown_fields` so typos fail
//! loudly. Pure functions only — cwd and paths are parameters, no global
//! state.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use toml::de::{DeTable, DeValue};
use toml::Spanned;

/// Hard cap on the config file size (64 KiB).
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Project-relative location of the config file.
pub const CONFIG_REL_PATH: &str = ".tole/config.toml";

/// File name of the user config under the tole data dir (#433).
pub const USER_CONFIG_FILE: &str = "config.toml";

/// Where the user config lives: `$CODECORA_HOME/tole/config.toml`, default
/// `~/.codecora/tole/config.toml` (next to the config trust store). `None`
/// when neither `CODECORA_HOME` nor a home directory is set.
pub fn user_config_path() -> Option<PathBuf> {
    tole_core::paths::tole_data_dir().map(|d| d.join(USER_CONFIG_FILE))
}

/// The `[mission]` table. Types mirror the `tole mission` flags
/// (`--max-steps/--max-minutes/--max-tokens` are `Option<u64>`,
/// `--verify` an `Option<String>`, `--verify-timeout` a `u64` with
/// default 300 — here optional like every key).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionConfig {
    pub max_steps: Option<u64>,
    pub max_minutes: Option<u64>,
    pub max_tokens: Option<u64>,
    pub verify: Option<String>,
    pub verify_timeout: Option<u64>,
}

/// The whole file: flat snake_case keys mirroring the global flags, all
/// optional, plus the single `[mission]` table.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub sessions_dir: Option<String>,
    pub workspace: Option<String>,
    pub plan_mode: Option<bool>,
    pub memory: Option<String>,
    pub trust: Option<Vec<String>>,
    pub allow: Option<Vec<String>>,
    pub mcp_server: Option<Vec<String>>,
    pub no_auto_mcp: Option<bool>,
    pub on_pretool: Option<Vec<String>>,
    pub on_posttool: Option<Vec<String>>,
    pub on_turnend: Option<Vec<String>>,
    pub skill: Option<Vec<String>>,
    pub no_skills: Option<bool>,
    pub system_prompt: Option<String>,
    /// The model's context window in tokens (#440); overrides `/models`.
    pub context_window: Option<u64>,
    /// Fraction of the context window that triggers compaction (#440).
    pub compact_at: Option<f64>,
    /// Turn context compaction off (#440).
    pub no_compaction: Option<bool>,
    pub mission: Option<MissionConfig>,
    /// Keys (rendered names, e.g. `model`, `mission.verify`) whose value
    /// came from the user layer; set by [`layered`], never by the file.
    #[serde(skip)]
    pub user_keys: BTreeSet<&'static str>,
}

/// A config problem, rendered as `<path>:<line>:<col>: <message>` when a
/// source position is known, `<path>: <message>` otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line_col: Option<(usize, usize)>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line_col {
            Some((l, c)) => write!(f, "{}:{l}:{c}: {}", self.path.display(), self.message),
            None => write!(f, "{}: {}", self.path.display(), self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// 1-based line/column of a byte offset (column counted in chars).
fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let head = src.get(..offset).unwrap_or_else(|| {
        // Offset inside a multi-byte char: back up to a boundary.
        let mut o = offset;
        while !src.is_char_boundary(o) {
            o -= 1;
        }
        &src[..o]
    });
    let line = head.matches('\n').count() + 1;
    let col = head.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

/// Where the config applies: an explicit path wins; otherwise
/// `<cwd>/.tole/config.toml` if present. No parent walk. `None` = no
/// config. An explicit path is returned as-is even when missing so the
/// load reports the failure instead of silently ignoring a typo.
pub fn discover(cwd: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let p = cwd.join(CONFIG_REL_PATH);
    p.exists().then_some(p)
}

/// Read at most `MAX_CONFIG_BYTES` (+1 to detect overflow) — never the
/// whole file — and decode as UTF-8.
pub fn read_source(path: &Path) -> Result<String, ConfigError> {
    let err = |message: String| ConfigError {
        path: path.to_path_buf(),
        line_col: None,
        message,
    };
    let file = std::fs::File::open(path).map_err(|e| err(format!("cannot read config: {e}")))?;
    let mut buf = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| err(format!("cannot read config: {e}")))?;
    if buf.len() as u64 > MAX_CONFIG_BYTES {
        return Err(err(format!(
            "config file is larger than the {} KiB limit",
            MAX_CONFIG_BYTES / 1024
        )));
    }
    String::from_utf8(buf).map_err(|_| err("config file is not valid UTF-8".to_string()))
}

/// Does this key name look like a secret? Case-insensitive substring
/// match over `secret`, `password`, `token`, `api_key` (`-` ≙ `_`).
/// The one schema key that legitimately contains such a word is
/// `mission.max_tokens` (a budget, not a credential); it is exempted by
/// exact position in [`check_forbidden`], never by name alone.
fn is_secret_name(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace('-', "_");
    ["secret", "password", "token", "api_key"]
        .iter()
        .any(|w| k.contains(w))
}

/// Walk the spanned tree; the first secret-like key (any depth, through
/// arrays of tables too) is a hard error naming the key.
fn check_forbidden(
    table: &DeTable<'_>,
    parent: Option<&str>,
    src: &str,
    path: &Path,
) -> Result<(), ConfigError> {
    for (k, v) in table.iter() {
        let name: &str = k.get_ref();
        let allowed = parent == Some("mission") && name == "max_tokens";
        if !allowed && is_secret_name(name) {
            return Err(ConfigError {
                path: path.to_path_buf(),
                line_col: Some(line_col(src, k.span().start)),
                message: format!(
                    "forbidden key `{name}`: secrets must never live in a config file \
                     (keep credentials in the environment, e.g. TOLE_API_KEY)"
                ),
            });
        }
        check_value(v.get_ref(), name, src, path)?;
    }
    Ok(())
}

fn check_value(v: &DeValue<'_>, key: &str, src: &str, path: &Path) -> Result<(), ConfigError> {
    match v {
        DeValue::Table(t) => check_forbidden(t, Some(key), src, path),
        DeValue::Array(a) => {
            for item in a.iter() {
                check_value(item.get_ref(), key, src, path)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate top-level `trust` entries with the shared `--trust` rule
/// ([`crate::trust::expand_trust`]). Non-string entries are left to the
/// typed deserialization for a type error.
fn check_trust(table: &DeTable<'_>, src: &str, path: &Path) -> Result<(), ConfigError> {
    let Some(entry) = table.get("trust") else {
        return Ok(());
    };
    let DeValue::Array(items) = entry.get_ref() else {
        return Ok(());
    };
    for item in items.iter() {
        if let DeValue::String(s) = item.get_ref() {
            if let Err(e) = crate::trust::expand_trust(&[s.to_string()]) {
                return Err(ConfigError {
                    path: path.to_path_buf(),
                    line_col: Some(line_col(src, Spanned::span(item).start)),
                    message: format!("{e}"),
                });
            }
        }
    }
    Ok(())
}

/// Parse + validate config text. `path` is only used in error messages.
pub fn parse(src: &str, path: &Path) -> Result<Config, ConfigError> {
    let toml_err = |e: toml::de::Error| ConfigError {
        path: path.to_path_buf(),
        line_col: e.span().map(|s| line_col(src, s.start)),
        message: e.message().to_string(),
    };
    let doc = DeTable::parse(src).map_err(toml_err)?;
    check_forbidden(doc.get_ref(), None, src, path)?;
    check_trust(doc.get_ref(), src, path)?;
    let cfg = toml::from_str::<Config>(src).map_err(toml_err)?;
    let range = |message: String| ConfigError {
        path: path.to_path_buf(),
        line_col: None,
        message,
    };
    if let Some(at) = cfg.compact_at {
        if !(at > 0.0 && at <= 1.0) {
            return Err(range(format!(
                "compact_at must be greater than 0 and at most 1, got {at}"
            )));
        }
    }
    if cfg.context_window == Some(0) {
        return Err(range(
            "context_window must be a positive number of tokens".into(),
        ));
    }
    Ok(cfg)
}

/// Read (bounded) and validate the config file at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let src = read_source(path)?;
    parse(&src, path)
}

/// Merge the two layers key by key: the project value wins when it is set
/// (by the same rules the precedence functions use: a blank string or an
/// empty list counts as unset), else the user value. Lists are never
/// merged across layers. The result records which keys came from the user
/// layer in [`Config::user_keys`].
pub fn layered(project: &Config, user: &Config) -> Config {
    let mut from_user = BTreeSet::new();
    fn pick<T: Clone>(
        key: &'static str,
        p: &Option<T>,
        u: &Option<T>,
        set: fn(&T) -> bool,
        from_user: &mut BTreeSet<&'static str>,
    ) -> Option<T> {
        match (p, u) {
            (Some(p), _) if set(p) => Some(p.clone()),
            (_, Some(u)) if set(u) => {
                from_user.insert(key);
                Some(u.clone())
            }
            _ => p.clone(),
        }
    }
    let text = |v: &String| !v.trim().is_empty();
    let list = |v: &Vec<String>| !v.is_empty();
    fn any<T>(_: &T) -> bool {
        true
    }
    let pm = project.mission.clone().unwrap_or_default();
    let um = user.mission.clone().unwrap_or_default();
    let u = &mut from_user;
    let mission = MissionConfig {
        max_steps: pick("mission.max_steps", &pm.max_steps, &um.max_steps, any, u),
        max_minutes: pick(
            "mission.max_minutes",
            &pm.max_minutes,
            &um.max_minutes,
            any,
            u,
        ),
        max_tokens: pick("mission.max_tokens", &pm.max_tokens, &um.max_tokens, any, u),
        verify: pick("mission.verify", &pm.verify, &um.verify, text, u),
        verify_timeout: pick(
            "mission.verify_timeout",
            &pm.verify_timeout,
            &um.verify_timeout,
            any,
            u,
        ),
    };
    let mut out = Config {
        model: pick("model", &project.model, &user.model, text, u),
        base_url: pick("base_url", &project.base_url, &user.base_url, text, u),
        sessions_dir: pick(
            "sessions_dir",
            &project.sessions_dir,
            &user.sessions_dir,
            text,
            u,
        ),
        workspace: pick("workspace", &project.workspace, &user.workspace, text, u),
        plan_mode: pick("plan_mode", &project.plan_mode, &user.plan_mode, any, u),
        memory: pick("memory", &project.memory, &user.memory, text, u),
        trust: pick("trust", &project.trust, &user.trust, list, u),
        allow: pick("allow", &project.allow, &user.allow, list, u),
        mcp_server: pick("mcp_server", &project.mcp_server, &user.mcp_server, list, u),
        no_auto_mcp: pick(
            "no_auto_mcp",
            &project.no_auto_mcp,
            &user.no_auto_mcp,
            any,
            u,
        ),
        on_pretool: pick("on_pretool", &project.on_pretool, &user.on_pretool, list, u),
        on_posttool: pick(
            "on_posttool",
            &project.on_posttool,
            &user.on_posttool,
            list,
            u,
        ),
        on_turnend: pick("on_turnend", &project.on_turnend, &user.on_turnend, list, u),
        skill: pick("skill", &project.skill, &user.skill, list, u),
        no_skills: pick("no_skills", &project.no_skills, &user.no_skills, any, u),
        system_prompt: pick(
            "system_prompt",
            &project.system_prompt,
            &user.system_prompt,
            text,
            u,
        ),
        context_window: pick(
            "context_window",
            &project.context_window,
            &user.context_window,
            any,
            u,
        ),
        compact_at: pick("compact_at", &project.compact_at, &user.compact_at, any, u),
        no_compaction: pick(
            "no_compaction",
            &project.no_compaction,
            &user.no_compaction,
            any,
            u,
        ),
        mission: (project.mission.is_some() || user.mission.is_some()).then_some(mission),
        user_keys: BTreeSet::new(),
    };
    out.user_keys = from_user;
    out
}

/// Longest `system_prompt` rendered by `config check`, in chars.
const PROMPT_DISPLAY_CHARS: usize = 80;

fn quote(s: &str) -> String {
    format!("{s:?}")
}

/// Per-key text appended after `[config]` on the key's line (effective
/// value + source, or the "not applied" marker). Keys use the rendered
/// names (`model`, `mission.max_steps`, ...).
pub type Annotations = std::collections::BTreeMap<&'static str, String>;

/// Render `key = value  [config]` lines for every key that is set. List
/// values go one entry per line; unset keys are not listed.
pub fn render(cfg: &Config) -> Vec<String> {
    render_with(cfg, PROJECT_TAG, &Annotations::new())
}

/// The `[tag]` on a project config line.
pub const PROJECT_TAG: &str = "config";
/// The `[tag]` on a user config line.
pub const USER_TAG: &str = "user config";

/// [`render`] with the layer's tag and a per-key suffix after it.
pub fn render_with(cfg: &Config, tag: &str, ann: &Annotations) -> Vec<String> {
    let src = tag;
    let mut out = Vec::new();
    let note = |key: &str| ann.get(key).map_or("", String::as_str);
    let scalar = |out: &mut Vec<String>, key: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("{key} = {v}  [{src}]{}", note(key)));
        }
    };
    let list = |out: &mut Vec<String>, key: &str, v: &Option<Vec<String>>| {
        if let Some(items) = v {
            out.push(format!(
                "{key} = [{} entries]  [{src}]{}",
                items.len(),
                note(key)
            ));
            for i in items {
                out.push(format!("  - {}", quote(i)));
            }
        }
    };
    scalar(&mut out, "model", cfg.model.as_deref().map(quote));
    scalar(&mut out, "base_url", cfg.base_url.as_deref().map(quote));
    scalar(
        &mut out,
        "sessions_dir",
        cfg.sessions_dir.as_deref().map(quote),
    );
    scalar(&mut out, "workspace", cfg.workspace.as_deref().map(quote));
    scalar(&mut out, "plan_mode", cfg.plan_mode.map(|b| b.to_string()));
    scalar(&mut out, "memory", cfg.memory.as_deref().map(quote));
    list(&mut out, "trust", &cfg.trust);
    list(&mut out, "allow", &cfg.allow);
    list(&mut out, "mcp_server", &cfg.mcp_server);
    scalar(
        &mut out,
        "no_auto_mcp",
        cfg.no_auto_mcp.map(|b| b.to_string()),
    );
    list(&mut out, "on_pretool", &cfg.on_pretool);
    list(&mut out, "on_posttool", &cfg.on_posttool);
    list(&mut out, "on_turnend", &cfg.on_turnend);
    list(&mut out, "skill", &cfg.skill);
    scalar(&mut out, "no_skills", cfg.no_skills.map(|b| b.to_string()));
    scalar(
        &mut out,
        "system_prompt",
        cfg.system_prompt.as_deref().map(|p| {
            let total = p.chars().count();
            if total > PROMPT_DISPLAY_CHARS {
                let head: String = p.chars().take(PROMPT_DISPLAY_CHARS).collect();
                format!("{}… ({total} chars)", quote(&head))
            } else {
                quote(p)
            }
        }),
    );
    scalar(
        &mut out,
        "context_window",
        cfg.context_window.map(|n| n.to_string()),
    );
    scalar(
        &mut out,
        "compact_at",
        cfg.compact_at.map(|n| n.to_string()),
    );
    scalar(
        &mut out,
        "no_compaction",
        cfg.no_compaction.map(|b| b.to_string()),
    );
    if let Some(m) = &cfg.mission {
        scalar(
            &mut out,
            "mission.max_steps",
            m.max_steps.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "mission.max_minutes",
            m.max_minutes.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "mission.max_tokens",
            m.max_tokens.map(|n| n.to_string()),
        );
        scalar(&mut out, "mission.verify", m.verify.as_deref().map(quote));
        scalar(
            &mut out,
            "mission.verify_timeout",
            m.verify_timeout.map(|n| n.to_string()),
        );
    }
    out
}

/// `tole config check`: returns the text to print, or the error. `cwd`,
/// `explicit` and the user config path are parameters (no global state).
/// `annotate` supplies the per-key suffixes from the shared resolution
/// over the LAYERED config (see `config_apply::check`), so a user key a
/// project key overrides shows the project as its effective source.
pub fn check(
    cwd: &Path,
    explicit: Option<&Path>,
    user: Option<&Path>,
    annotate: &dyn Fn(&Config) -> Annotations,
) -> Result<String, ConfigError> {
    let user_cfg = match user {
        Some(p) if p.exists() => Some((p.to_path_buf(), load(p)?)),
        _ => None,
    };
    let project = match discover(cwd, explicit) {
        Some(path) => Some((path.clone(), load(&path)?)),
        None => None,
    };
    let none = Config::default();
    let merged = layered(
        project.as_ref().map_or(&none, |(_, c)| c),
        user_cfg.as_ref().map_or(&none, |(_, c)| c),
    );
    let ann = annotate(&merged);
    let mut lines = Vec::new();
    let section = |lines: &mut Vec<String>, cfg: &Config, tag: &str| {
        let rendered = render_with(cfg, tag, &ann);
        if rendered.is_empty() {
            lines.push("(valid; no keys set)".to_string());
        } else {
            lines.extend(rendered);
        }
    };
    match (&user_cfg, user) {
        (Some((path, cfg)), _) => {
            let shown = path.canonicalize().unwrap_or(path.clone());
            lines.push(format!("user config: {}", shown.display()));
            section(&mut lines, cfg, USER_TAG);
        }
        (None, Some(p)) => lines.push(format!("no user config at {}", p.display())),
        (None, None) => lines.push("no user config (set CODECORA_HOME or HOME)".into()),
    }
    match &project {
        Some((path, cfg)) => {
            let shown = path.canonicalize().unwrap_or(path.clone());
            lines.push(format!("config: {}", shown.display()));
            section(&mut lines, cfg, PROJECT_TAG);
        }
        None => lines.push(format!("no {CONFIG_REL_PATH} in {}", cwd.display())),
    }
    lines.push(
        "note: precedence is flag > env > config > user config > default; a higher layer \
         replaces a whole list key, flags can only turn boolean keys on, and --no-config \
         ignores both files."
            .into(),
    );
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmpdir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("tole-config-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn p() -> PathBuf {
        PathBuf::from("cfg.toml")
    }

    const FULL: &str = r#"
model = "m"
base_url = "http://x"
sessions_dir = "s"
workspace = "w"
plan_mode = true
memory = "uteke"
trust = ["internal", "none"]
allow = ["read_file"]
mcp_server = ["a=b c"]
no_auto_mcp = true
on_pretool = ["p"]
on_posttool = ["q"]
on_turnend = ["cargo check"]
skill = ["skills/x/SKILL.md"]
no_skills = false
system_prompt = "hi"
context_window = 131072
compact_at = 0.75
no_compaction = false

[mission]
max_steps = 10
max_minutes = 5
max_tokens = 1000
verify = "cargo test"
verify_timeout = 60
"#;

    #[test]
    fn full_file_round_trips() {
        let c = parse(FULL, &p()).unwrap();
        assert_eq!(c.model.as_deref(), Some("m"));
        assert_eq!(c.plan_mode, Some(true));
        assert_eq!(c.no_skills, Some(false));
        assert_eq!(c.trust.as_deref().unwrap(), ["internal", "none"]);
        assert_eq!(c.skill.as_deref().unwrap(), ["skills/x/SKILL.md"]);
        let m = c.mission.unwrap();
        assert_eq!(
            (m.max_steps, m.max_minutes, m.max_tokens, m.verify_timeout),
            (Some(10), Some(5), Some(1000), Some(60))
        );
        assert_eq!(m.verify.as_deref(), Some("cargo test"));
        let lines = render(&parse(FULL, &p()).unwrap());
        assert!(lines.contains(&"model = \"m\"  [config]".to_string()));
        assert!(lines.contains(&"  - \"internal\"".to_string()));
        assert!(lines.contains(&"mission.max_tokens = 1000  [config]".to_string()));
    }

    #[test]
    fn compaction_keys_are_range_checked() {
        let c = parse(FULL, &p()).unwrap();
        assert_eq!(
            (c.context_window, c.compact_at, c.no_compaction),
            (Some(131072), Some(0.75), Some(false))
        );
        for bad in [
            "compact_at = 0.0\n",
            "compact_at = 1.5\n",
            "compact_at = -0.2\n",
        ] {
            let e = parse(bad, &p()).unwrap_err();
            assert!(e.message.contains("compact_at"), "{bad} -> {e}");
        }
        assert!(parse("compact_at = 1.0\n", &p()).is_ok());
        assert!(parse("context_window = 0\n", &p()).is_err());
        assert!(parse("context_window = -1\n", &p()).is_err());
    }

    #[test]
    fn empty_file_is_valid() {
        assert_eq!(parse("", &p()).unwrap(), Config::default());
        assert!(render(&Config::default()).is_empty());
    }

    #[test]
    fn unknown_key_is_rejected_naming_it() {
        let e = parse("modle = \"x\"\n", &p()).unwrap_err();
        assert!(e.message.contains("modle"), "{e}");
        assert_eq!(e.line_col.map(|l| l.0), Some(1));
        let e = parse("[mission]\nmax_step = 1\n", &p()).unwrap_err();
        assert!(e.message.contains("max_step"), "{e}");
    }

    #[test]
    fn forbidden_secret_keys_are_hard_errors() {
        for src in [
            "api_key = \"x\"\n",
            "serve_token = \"x\"\n",
            "token = \"x\"\n",
            "my_secret = \"x\"\n",
            "db_password = \"x\"\n",
            "API_KEY = \"x\"\n",
            "Api-Key = \"x\"\n",
            "[mission]\ntoken = \"x\"\n",
            "[mission]\nverify_secret = \"x\"\n",
            "[a.b]\nPassword = \"x\"\n",
            "x = [{ ok = 1, auth_token = \"x\" }]\n",
        ] {
            let e = parse(src, &p()).unwrap_err();
            assert!(
                e.message
                    .contains("secrets must never live in a config file"),
                "{src:?} -> {e}"
            );
            assert!(e.line_col.is_some(), "{src:?} -> {e}");
        }
        let e = parse("[mission]\nverify_secret = \"x\"\n", &p()).unwrap_err();
        assert!(e.message.contains("verify_secret"));
    }

    #[test]
    fn mission_max_tokens_is_not_mistaken_for_a_secret() {
        assert!(parse("[mission]\nmax_tokens = 5\n", &p()).is_ok());
        // Only in its exact position: top-level it is a secret-like name.
        assert!(parse("max_tokens = 5\n", &p()).is_err());
    }

    #[test]
    fn wrong_type_is_rejected_with_line_col() {
        let e = parse("model = \"m\"\nplan_mode = \"yes\"\n", &p()).unwrap_err();
        assert_eq!(e.line_col.map(|l| l.0), Some(2), "{e}");
        assert!(e.to_string().starts_with("cfg.toml:2:"), "{e}");
        assert!(parse("[mission]\nmax_steps = -1\n", &p()).is_err());
        assert!(parse("[mission]\nmax_steps = \"3\"\n", &p()).is_err());
        assert!(parse("allow = \"read_file\"\n", &p()).is_err());
    }

    #[test]
    fn trust_presets_use_the_shared_rule() {
        assert!(parse("trust = [\"internal\", \"read_only\", \"none\"]\n", &p()).is_ok());
        assert!(parse("trust = [\"INTERNAL\"]\n", &p()).is_ok());
        let e = parse("trust = [\"internal\", \"internalx\"]\n", &p()).unwrap_err();
        assert!(e.message.contains("unknown trust preset"), "{e}");
        assert!(e.message.contains("internalx"), "{e}");
        assert_eq!(e.line_col.map(|l| l.0), Some(1));
        // Same function as the flag: identical message.
        let flag = crate::trust::expand_trust(&["internalx".into()]).unwrap_err();
        assert_eq!(e.message, flag.to_string());
    }

    #[test]
    fn malformed_toml_reports_line_and_col() {
        let e = parse("model = \"m\"\nallow = [\n", &p()).unwrap_err();
        assert!(e.line_col.is_some(), "{e}");
        let e = parse("model = \"m\"\nbad line here\n", &p()).unwrap_err();
        let s = e.to_string();
        assert!(s.starts_with("cfg.toml:2:"), "{s}");
    }

    #[test]
    fn oversized_file_is_rejected_without_full_read() {
        let d = tmpdir();
        let f = d.join("big.toml");
        let big = format!("system_prompt = \"{}\"\n", "a".repeat(70 * 1024));
        std::fs::write(&f, big).unwrap();
        let e = load(&f).unwrap_err();
        assert!(e.message.contains("larger than the 64 KiB limit"), "{e}");
        // Exactly at the cap is allowed by the size gate.
        let ok = d.join("ok.toml");
        let prefix = "system_prompt = \"";
        let pad = MAX_CONFIG_BYTES as usize - prefix.len() - 2;
        std::fs::write(&ok, format!("{prefix}{}\"\n", "b".repeat(pad))).unwrap();
        assert_eq!(
            std::fs::metadata(&ok).unwrap().len(),
            MAX_CONFIG_BYTES,
            "fixture must sit exactly at the cap"
        );
        assert!(load(&ok).is_ok());
        // The bounded reader never pulls more than cap+1 bytes.
        let huge = d.join("huge.toml");
        let f = std::fs::File::create(&huge).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
        let t = std::time::Instant::now();
        assert!(load(&huge).is_err());
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn non_utf8_is_rejected() {
        let d = tmpdir();
        let f = d.join("bin.toml");
        std::fs::write(&f, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load(&f).unwrap_err().message.contains("UTF-8"));
    }

    #[test]
    fn discovery_is_cwd_only_and_explicit_wins() {
        let d = tmpdir();
        assert_eq!(discover(&d, None), None);
        std::fs::create_dir_all(d.join(".tole")).unwrap();
        std::fs::write(d.join(".tole/config.toml"), "").unwrap();
        assert_eq!(discover(&d, None), Some(d.join(".tole/config.toml")));
        // No parent walk.
        let child = d.join("sub");
        std::fs::create_dir_all(&child).unwrap();
        assert_eq!(discover(&child, None), None);
        let other = d.join("other.toml");
        assert_eq!(discover(&d, Some(&other)), Some(other));
    }

    #[test]
    fn check_reports_absent_valid_and_missing_explicit() {
        let d = tmpdir();
        let none = |_: &Config| Annotations::new();
        let out = check(&d, None, None, &none).unwrap();
        assert!(out.contains("\nno .tole/config.toml in "), "{out}");
        let f = d.join("c.toml");
        std::fs::write(&f, "model = \"m\"\n").unwrap();
        let out = check(&d, Some(&f), None, &none).unwrap();
        assert!(out.contains("model = \"m\"  [config]"), "{out}");
        assert!(check(&d, Some(&d.join("nope.toml")), None, &none).is_err());
    }

    #[test]
    fn check_shows_both_layers() {
        let d = tmpdir();
        let none = |_: &Config| Annotations::new();
        let user = d.join("user.toml");
        let out = check(&d, None, Some(&user), &none).unwrap();
        assert!(out.starts_with("no user config at "), "{out}");
        std::fs::write(&user, "model = \"u\"\nskill = [\"g/SKILL.md\"]\n").unwrap();
        let proj = d.join("p.toml");
        std::fs::write(&proj, "model = \"p\"\n").unwrap();
        let out = check(&d, Some(&proj), Some(&user), &|c| {
            // The annotation sees the layered config.
            assert_eq!(c.model.as_deref(), Some("p"));
            assert!(c.user_keys.contains("skill") && !c.user_keys.contains("model"));
            Annotations::new()
        })
        .unwrap();
        assert!(out.starts_with("user config: "), "{out}");
        assert!(out.contains("model = \"u\"  [user config]"), "{out}");
        assert!(out.contains("model = \"p\"  [config]"), "{out}");
        // A broken user config is an error, like a broken project config.
        std::fs::write(&user, "api_key = \"x\"\n").unwrap();
        assert!(check(&d, None, Some(&user), &none).is_err());
    }

    #[test]
    fn project_layer_wins_key_by_key() {
        let user = parse(
            "model = \"u\"\nbase_url = \"http://u\"\nskill = [\"g\"]\nplan_mode = true\n\
             [mission]\nmax_steps = 9\nverify = \"u\"\n",
            &p(),
        )
        .unwrap();
        let project = parse(
            "model = \"p\"\nbase_url = \"  \"\nskill = []\nplan_mode = false\n\
             [mission]\nverify = \"p\"\n",
            &p(),
        )
        .unwrap();
        let c = layered(&project, &user);
        assert_eq!(c.model.as_deref(), Some("p"));
        // Blank string / empty list in the project count as unset.
        assert_eq!(c.base_url.as_deref(), Some("http://u"));
        assert_eq!(c.skill.as_deref().unwrap(), ["g"]);
        // An explicit project `false` is set: it wins over the user `true`.
        assert_eq!(c.plan_mode, Some(false));
        let m = c.mission.clone().unwrap();
        assert_eq!((m.max_steps, m.verify.as_deref()), (Some(9), Some("p")));
        let expected: BTreeSet<&str> = ["base_url", "skill", "mission.max_steps"].into();
        assert_eq!(c.user_keys, expected);
        // No layers at all: nothing set, no mission table.
        assert_eq!(
            layered(&Config::default(), &Config::default()),
            Config::default()
        );
        // One layer alone passes through.
        let only_user = layered(&Config::default(), &user);
        assert_eq!(only_user.model.as_deref(), Some("u"));
        assert!(only_user.user_keys.contains("model"));
    }

    #[test]
    fn long_system_prompt_is_truncated_for_display() {
        let c = parse(&format!("system_prompt = \"{}\"\n", "x".repeat(200)), &p()).unwrap();
        let line = &render(&c)[0];
        assert!(line.contains("… (200 chars)"), "{line}");
        assert!(line.len() < 150, "{line}");
    }
}
