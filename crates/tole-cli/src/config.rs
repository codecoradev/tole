//! Project config file `.tole/config.toml` (#208, part 1 of 3): LOAD +
//! VALIDATE only. Nothing here is applied to sessions — flags, env and
//! runtime behavior are untouched until a later part wires precedence in.
//!
//! The file is untrusted input (it lives in a cloned repo), so the loader
//! is strict: a 64 KiB bounded read, a pre-check that refuses secret-like
//! keys at any depth, trust-preset names validated by the SAME function
//! the `--trust` flag uses, and `deny_unknown_fields` so typos fail
//! loudly. Pure functions only — cwd and paths are parameters, no global
//! state.

use serde::Deserialize;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use toml::de::{DeTable, DeValue};
use toml::Spanned;

/// Hard cap on the config file size (64 KiB).
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Project-relative location of the config file.
pub const CONFIG_REL_PATH: &str = ".tole/config.toml";

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
    pub mission: Option<MissionConfig>,
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
                    "forbidden key `{name}`: secrets must never live in a project file \
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
    toml::from_str::<Config>(src).map_err(toml_err)
}

/// Read (bounded) and validate the config file at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let src = read_source(path)?;
    parse(&src, path)
}

/// Longest `system_prompt` rendered by `config check`, in chars.
const PROMPT_DISPLAY_CHARS: usize = 80;

fn quote(s: &str) -> String {
    format!("{s:?}")
}

/// Render `key = value  [config]` lines for every key that is set. List
/// values go one entry per line; unset keys are not listed.
pub fn render(cfg: &Config) -> Vec<String> {
    const SRC: &str = "config";
    let mut out = Vec::new();
    let scalar = |out: &mut Vec<String>, key: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("{key} = {v}  [{SRC}]"));
        }
    };
    let list = |out: &mut Vec<String>, key: &str, v: &Option<Vec<String>>| {
        if let Some(items) = v {
            out.push(format!("{key} = [{} entries]  [{SRC}]", items.len()));
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

/// `tole config check`: returns the text to print, or the error. `cwd`
/// and `explicit` are parameters (no global state).
pub fn check(cwd: &Path, explicit: Option<&Path>) -> Result<String, ConfigError> {
    let Some(path) = discover(cwd, explicit) else {
        return Ok(format!("no {CONFIG_REL_PATH} in {}", cwd.display()));
    };
    let cfg = load(&path)?;
    let shown = path.canonicalize().unwrap_or(path);
    let mut lines = vec![format!("config: {}", shown.display())];
    let rendered = render(&cfg);
    if rendered.is_empty() {
        lines.push("(valid; no keys set)".to_string());
    } else {
        lines.extend(rendered);
    }
    lines.push("note: the config is validated only — it is not applied to sessions yet.".into());
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
                    .contains("secrets must never live in a project file"),
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
        let out = check(&d, None).unwrap();
        assert!(out.starts_with("no .tole/config.toml in "), "{out}");
        let f = d.join("c.toml");
        std::fs::write(&f, "model = \"m\"\n").unwrap();
        let out = check(&d, Some(&f)).unwrap();
        assert!(out.contains("model = \"m\"  [config]"), "{out}");
        assert!(check(&d, Some(&d.join("nope.toml"))).is_err());
    }

    #[test]
    fn long_system_prompt_is_truncated_for_display() {
        let c = parse(&format!("system_prompt = \"{}\"\n", "x".repeat(200)), &p()).unwrap();
        let line = &render(&c)[0];
        assert!(line.contains("… (200 chars)"), "{line}");
        assert!(line.len() < 150, "{line}");
    }
}
