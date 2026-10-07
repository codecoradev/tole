//! Update check (issue #220): notify + self-upgrade, fleet-parity with
//! uteke (`update_check.rs`) and cora (`upgrade.rs`).
//!
//! Tier 1 — startup notification: cache-first (24 h), `/releases/latest`
//! 302-redirect primary (no API rate limit), API fallback, network
//! failures silently swallowed (a banner is never a critical path).
//! Opt-out: `TOLE_NO_UPDATE_CHECK=1`.
//!
//! Tier 2 — `tole upgrade` lives in tole-cli (runs `cargo install`).
//! This module only answers "is there a newer version?".

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

const REPO: &str = "codecoradev/tole";
/// Seconds before re-checking GitHub (24 hours).
const CACHE_TTL: u64 = 86_400;

#[derive(Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    latest: String,
}

/// Result of an update check.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub latest: String,
    pub current: String,
}

impl UpdateInfo {
    /// True when `latest` is strictly newer than `current` (numeric
    /// major.minor.patch compare; non-numeric segments fail closed).
    pub fn is_update_available(&self) -> bool {
        is_newer(&self.latest, &self.current)
    }

    /// One-line stderr banner.
    pub fn banner(&self) -> String {
        format!(
            "Update available: {} (currently v{}) — run `tole upgrade`, or see https://github.com/{}/releases/tag/{}",
            self.latest,
            self.current,
            REPO,
            self.latest
        )
    }
}

/// std-only home resolution (matches `skills.rs`; no `dirs` dep).
fn codecora_home() -> Option<std::path::PathBuf> {
    std::env::var("CODECORA_HOME")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .map(std::path::PathBuf::from)
                .ok()
        })
        .map(|h| h.join(".codecora").join("tole"))
}

fn cache_path() -> Option<std::path::PathBuf> {
    codecora_home().map(|d| d.join("update-cache.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache() -> Option<String> {
    let path = cache_path()?;
    let data = std::fs::read_to_string(path).ok()?;
    let cache: Cache = serde_json::from_str(&data).ok()?;
    if now_secs().saturating_sub(cache.checked_at) < CACHE_TTL {
        Some(cache.latest)
    } else {
        None
    }
}

fn write_cache(latest: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = Cache {
        checked_at: now_secs(),
        latest: latest.to_string(),
    };
    let _ = std::fs::write(path, serde_json::to_string(&cache).unwrap_or_default());
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Public read of the compile-time version (for `tole upgrade` output).
pub fn current_version_for_upgrade() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Numeric major.minor.patch compare; anything unparsable fails closed
/// (no false "update available" from a weird tag).
fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut it = v.split('.');
    let major: u64 = it.next()?.parse().ok()?;
    let minor: u64 = it.next()?.parse().ok()?;
    let patch: u64 = it.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Public (crates.io vs running binary) for `tole upgrade`.
pub fn is_newer_public(latest: &str, current: &str) -> bool {
    is_newer(latest, current)
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Synchronous network check: `/releases/latest` 302 first (no API
/// rate limit), `api.github.com` fallback. Updates the cache.
pub fn check_network() -> Result<UpdateInfo, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent();

    // Primary: the redirect target of /releases/latest IS the tag URL.
    // max_redirects(0) is REQUIRED — ureq's default follows the 302 and
    // returns the final HTML page, defeating the Location parse (cora).
    let no_redirect_agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .max_redirects(0)
        .build()
        .new_agent();
    let head = no_redirect_agent
        .head(&format!("https://github.com/{REPO}/releases/latest"))
        .call();
    let latest = match head {
        Ok(res) if res.status().is_redirection() => res
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .and_then(|loc| loc.rsplit('/').next())
            .map(str::to_string),
        Ok(_) => None,
        Err(_) => None,
    }
    .or_else(|| {
        // Fallback: API (rate-limited; last resort).
        let res = agent
            .get(&format!(
                "https://api.github.com/repos/{REPO}/releases/latest"
            ))
            .call()
            .ok()?;
        let mut body = String::new();
        res.into_body().as_reader().read_to_string(&mut body).ok()?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        v.get("tag_name")?.as_str().map(str::to_string)
    })
    .ok_or_else(|| "no latest version resolved".to_string())?;

    write_cache(&latest);
    Ok(UpdateInfo {
        latest,
        current: current_version().to_string(),
    })
}

/// Latest version published on crates.io (the distribution source of
/// truth for `tole upgrade`; GitHub releases carry no binaries).
/// Requires a User-Agent per crates.io policy.
pub fn latest_on_crates_io() -> Result<String, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .new_agent();
    let res = agent
        .get("https://crates.io/api/v1/crates/tole-cli")
        .header("User-Agent", "tole-upgrade (codecoradev/tole)")
        .call()
        .map_err(|e| format!("crates.io unreachable: {e}"))?;
    let mut text = String::new();
    res.into_body()
        .as_reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("crates.io read failed: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("crates.io response malformed: {e}"))?;
    v["crate"]["max_version"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "crates.io response missing max_version".to_string())
}

/// Cache-first check (no network on the fast path).
pub fn check_cached() -> Option<UpdateInfo> {
    let latest = read_cache()?;
    Some(UpdateInfo {
        latest,
        current: current_version().to_string(),
    })
}

/// Startup notification: banner from cache when fresh; otherwise spawn
/// a background network check that banners if newer. Returns the join
/// handle when a thread was spawned (caller decides whether to join —
/// detaching is fine, the banner is best-effort). `TOLE_NO_UPDATE_CHECK=1`
/// disables entirely.
pub fn check_and_notify() -> Option<std::thread::JoinHandle<()>> {
    if std::env::var("TOLE_NO_UPDATE_CHECK")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return None;
    }
    if let Some(info) = check_cached() {
        if info.is_update_available() {
            eprintln!("\n{}\n", info.banner());
        }
        return None;
    }
    let handle = std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(|| {
            if let Some(info) = check_network().ok().filter(|i| i.is_update_available()) {
                eprintln!("\n{}\n", info.banner());
            }
        });
    });
    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare() {
        assert!(is_newer("v0.7.0", "0.6.0"));
        assert!(is_newer("0.7.1", "0.7.0"));
        assert!(!is_newer("0.6.0", "0.6.0"));
        assert!(!is_newer("0.5.9", "0.6.0"));
        // unparsable fails closed (no false positives)
        assert!(!is_newer("nightly", "0.6.0"));
        assert!(!is_newer("v0.7.0-rc.1", "0.6.0"));
    }

    #[test]
    fn banner_formats() {
        let info = UpdateInfo {
            latest: "v0.7.0".into(),
            current: "0.6.0".into(),
        };
        let b = info.banner();
        assert!(b.contains("0.7.0") && b.contains("0.6.0") && b.contains("tole upgrade"));
    }

    #[test]
    fn parse_host_ignores_codecora_home_absence() {
        // codecora_home() falls back to HOME/USERPROFILE; on any system
        // with a home this resolves somewhere writable.
        if std::env::var("HOME").is_ok() || std::env::var("USERPROFILE").is_ok() {
            assert!(cache_path().is_some());
        }
    }
}
