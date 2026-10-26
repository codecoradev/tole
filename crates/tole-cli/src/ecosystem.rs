//! The sibling binaries tole drives — cora (code intelligence) and uteke
//! (memory) — and the oldest version of each that tole supports (#422).
//!
//! Both are optional: tole probes them at startup and registers their
//! tools only when present. This module is the one place that knows the
//! supported floor; `tole doctor` reports against it.

use crate::tool_roster::binary_available;
use std::process::Command;
use std::time::Duration;

/// A sibling binary and the oldest version tole supports.
#[derive(Debug, Clone, Copy)]
pub struct Sibling {
    pub bin: &'static str,
    /// Oldest supported version, `(major, minor, patch)`.
    pub min: (u64, u64, u64),
    /// What tole uses it for.
    pub role: &'static str,
    /// Install / upgrade command.
    pub install: &'static str,
}

/// cora 0.19.0 adds the agent-as-reviewer MCP tools; uteke 0.21.1 is the
/// release whose rooms replace the deprecated namespaces.
pub const SIBLINGS: &[Sibling] = &[
    Sibling {
        bin: "cora",
        min: (0, 19, 0),
        role: "code intelligence and review (mcp_cora_*, cora_search)",
        install: "cargo install cora-code --locked",
    },
    Sibling {
        bin: "uteke",
        min: (0, 21, 1),
        role: "memory (uteke_*, --memory uteke)",
        install: "cargo install uteke-cli --locked",
    },
];

/// What the probe found for one sibling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Supported((u64, u64, u64)),
    TooOld((u64, u64, u64)),
    Missing,
    /// Present, but `--version` did not yield a version.
    Unknown(String),
}

/// The first `x.y.z` in `--version` output (`cora 0.19.0`, `uteke 0.21.1`,
/// `tool v1.2.3-rc1`).
pub fn parse_version(out: &str) -> Option<(u64, u64, u64)> {
    out.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        let core = word.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let v = (
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(v)
    })
}

/// Classify `--version` output against the floor (`None` = not installed).
pub fn classify(s: &Sibling, version_output: Option<&str>) -> Status {
    match version_output {
        None => Status::Missing,
        Some(out) => match parse_version(out) {
            Some(v) if v >= s.min => Status::Supported(v),
            Some(v) => Status::TooOld(v),
            None => Status::Unknown(out.lines().next().unwrap_or("").trim().to_string()),
        },
    }
}

/// Run `<bin> --version` when the binary is on PATH.
pub fn probe(s: &Sibling) -> Status {
    if !binary_available(s.bin) {
        return classify(s, None);
    }
    let mut cmd = Command::new(s.bin);
    cmd.arg("--version");
    match tole_core::subprocess::run_with_timeout(&mut cmd, Duration::from_secs(5)) {
        Ok(out) if out.status.success() => classify(s, Some(&String::from_utf8_lossy(&out.stdout))),
        Ok(out) => Status::Unknown(format!("--version exited {}", out.status)),
        Err(e) => Status::Unknown(e),
    }
}

fn fmt(v: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// One report line, in `tole doctor`'s `✓`/`✗` style. Never a blocker:
/// both siblings are optional.
pub fn line(s: &Sibling, status: &Status) -> String {
    let min = fmt(s.min);
    match status {
        Status::Supported(v) => format!("  ✓ {} {} (supported ≥ {min})", s.bin, fmt(*v)),
        Status::TooOld(v) => format!(
            "  ✗ {} {} is older than the supported {min} — upgrade: {}",
            s.bin,
            fmt(*v),
            s.install
        ),
        Status::Missing => format!(
            "  - {} not installed (optional: {}) — install: {}",
            s.bin, s.role, s.install
        ),
        Status::Unknown(why) => format!(
            "  ? {} version unknown ({why}); supported ≥ {min} — upgrade: {}",
            s.bin, s.install
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_version_output_of_both_siblings() {
        assert_eq!(
            parse_version("cora 0.19.0\nhttps://github.com/x"),
            Some((0, 19, 0))
        );
        assert_eq!(parse_version("uteke 0.21.1"), Some((0, 21, 1)));
        assert_eq!(parse_version("tool v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("tool 1.2"), None);
        assert_eq!(parse_version("tool 1.2.3.4"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn classifies_against_the_floor() {
        let s = SIBLINGS[1]; // uteke ≥ 0.21.1
        assert_eq!(classify(&s, None), Status::Missing);
        assert_eq!(
            classify(&s, Some("uteke 0.21.1")),
            Status::Supported((0, 21, 1))
        );
        assert_eq!(
            classify(&s, Some("uteke 0.22.0")),
            Status::Supported((0, 22, 0))
        );
        assert_eq!(
            classify(&s, Some("uteke 0.21.0")),
            Status::TooOld((0, 21, 0))
        );
        assert_eq!(
            classify(&s, Some("uteke dev build")),
            Status::Unknown("uteke dev build".into())
        );
    }

    #[test]
    fn too_old_names_the_upgrade_command() {
        let s = SIBLINGS[0];
        let l = line(&s, &Status::TooOld((0, 18, 0)));
        assert!(
            l.contains("cora 0.18.0 is older than the supported 0.19.0"),
            "{l}"
        );
        assert!(l.contains("cargo install cora-code --locked"), "{l}");
    }
}
