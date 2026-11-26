//! Parsing and validating config text: the forbidden-key walk and the range checks.

use super::layering::gate_strictness;
use super::profile_config::check_profile;
use super::secrets_config::{check_secret, check_sensitive_glob};
use super::{
    curator_thresholds, read_source, Config, ConfigError, MAX_CRON_CONCURRENT, MAX_CURATOR_DAYS,
};
use std::path::Path;
use toml::de::{DeTable, DeValue};
use toml::Spanned;

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

/// Does this key name look like a secret? Case-insensitive substring
/// match over `secret`, `password`, `token`, `api_key` (`-` ≙ `_`).
/// The schema keys that legitimately contain such a word are
/// `mission.max_tokens` and `cron.daily_token_cap` (budgets, not
/// credentials); they are exempted by exact position in
/// [`check_forbidden`], never by name alone.
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
        let allowed = matches!(
            (parent, name),
            (Some("mission"), "max_tokens")
                | (Some("cron"), "daily_token_cap")
                | (None, "secrets")
                // `[secrets]` keys are secret NAMES; their values are checked
                // below and by `check_secret` (never a value).
                | (Some("secrets"), _)
        );
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
    if let Some(level) = &cfg.reasoning_effort {
        tole_core::openai::parse_reasoning_effort(level)
            .map_err(|e| range(format!("reasoning_effort: {e}")))?;
    }
    if let Some(v) = &cfg.review_gate {
        if gate_strictness(v).is_none() {
            return Err(range(format!(
                "review_gate must be off, warn or block, got {v:?}"
            )));
        }
    }
    if cfg.context_window == Some(0) {
        return Err(range(
            "context_window must be a positive number of tokens".into(),
        ));
    }
    if let Some(n) = cfg
        .discussions
        .as_ref()
        .and_then(|d| d.request_timeout_secs)
    {
        if !crate::config_apply::discussion_timeout_in_range(n) {
            return Err(range(format!(
                "discussions.request_timeout_secs must be {}, got {n}",
                crate::config_apply::discussion_timeout_bounds()
            )));
        }
    }
    if let Some(n) = cfg.provider.as_ref().and_then(|p| p.request_timeout_secs) {
        if !crate::config_apply::chat_timeout_in_range(n) {
            return Err(range(format!(
                "provider.request_timeout_secs must be {}, got {n}",
                crate::config_apply::chat_timeout_bounds()
            )));
        }
    }
    for (name, p) in cfg.profiles.iter().flatten() {
        check_profile(name, p).map_err(range)?;
    }
    for (name, b) in cfg.secrets.iter().flatten() {
        check_secret(name, b).map_err(range)?;
    }
    for g in cfg.sensitive_files.iter().flatten() {
        check_sensitive_glob(g).map_err(range)?;
    }
    if let Some(a) = &cfg.redaction_allowlist {
        tole_core::secrets::shape::check_allowlist(a)
            .map_err(|e| range(format!("redaction_allowlist: {e}")))?;
    }
    if let Some(r) = &cfg.router {
        crate::routing::compile(r).map_err(range)?;
    }
    if let Some(n) = cfg.cron.as_ref().and_then(|c| c.max_concurrent) {
        if !(1..=MAX_CRON_CONCURRENT).contains(&n) {
            return Err(range(format!(
                "cron.max_concurrent must be 1 to {MAX_CRON_CONCURRENT}, got {n}"
            )));
        }
    }
    if let Some(s) = &cfg.sandbox {
        s.policy().map_err(range)?;
        s.consumers().map_err(range)?;
    }
    if let Some(c) = &cfg.curator {
        for (key, v) in [
            ("stale_days", c.stale_days),
            ("archive_days", c.archive_days),
            ("grace_days", c.grace_days),
        ] {
            if v.is_some_and(|n| n > MAX_CURATOR_DAYS) {
                return Err(range(format!(
                    "curator.{key} must be at most {MAX_CURATOR_DAYS}"
                )));
            }
        }
        // The effective pair, defaults filled in, so one key alone cannot
        // leave `archive_days` at or below `stale_days`.
        let th = curator_thresholds(Some(c));
        th.check().map_err(|e| range(format!("curator: {e}")))?;
    }
    if let Some(d) = &cfg.delegation {
        let ceiling = tole_core::delegation::MAX_DEPTH_CEILING;
        if d.max_depth.is_some_and(|n| !(1..=ceiling).contains(&n)) {
            return Err(range(format!(
                "delegation.max_depth must be 1 to {ceiling}"
            )));
        }
        let zero = [
            ("max_live_processes", d.max_live_processes.map(u64::from)),
            ("goal_budget", d.goal_budget),
            ("goal_steps", d.goal_steps.map(u64::from)),
            ("goal_minutes", d.goal_minutes),
        ];
        if let Some((k, _)) = zero.iter().find(|(_, v)| *v == Some(0)) {
            return Err(range(format!("delegation.{k} must be at least 1")));
        }
        d.resource_thresholds().map_err(range)?;
        let max = tole_core::delegation_approvals::MAX_APPROVAL_SECS as u64;
        let (total, level) = (d.approval_secs, d.approval_level_secs);
        for (k, v) in [("approval_secs", total), ("approval_level_secs", level)] {
            if v.is_some_and(|n| !(1..=max).contains(&n)) {
                return Err(range(format!("delegation.{k} must be 1 to {max}")));
            }
        }
        let default = tole_core::delegation_approvals::DEFAULT_APPROVAL_SECS as u64;
        if level.is_some_and(|l| l > total.unwrap_or(default)) {
            return Err(range(
                "delegation.approval_level_secs must not exceed approval_secs".into(),
            ));
        }
    }
    Ok(cfg)
}

/// Read (bounded) and validate the config file at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let src = read_source(path)?;
    parse(&src, path)
}
