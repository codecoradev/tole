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
    /// The `tole setup <tool>` shortcut that runs `install` (#535), when one
    /// exists.
    pub setup: Option<&'static str>,
}

/// cora 0.19.0 adds the agent-as-reviewer MCP tools; uteke 0.21.1 is the
/// release whose rooms replace the deprecated namespaces.
pub const SIBLINGS: &[Sibling] = &[
    Sibling {
        bin: "cora",
        min: (0, 19, 0),
        role: "code intelligence and review (mcp_cora_*, cora_search)",
        install: "cargo install cora-code --locked",
        setup: Some("tole setup cora"),
    },
    Sibling {
        bin: "uteke",
        min: (0, 21, 1),
        role: "memory (uteke_*, --memory uteke)",
        install: "cargo install uteke-cli --locked",
        setup: None,
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

/// `tole doctor` WARN (#992): `CODECORA_HOME` moves uteke's whole home (the
/// store, the embedding model under `models/`, `uteke.toml`, logs) to
/// `<CODECORA_HOME>/uteke`. When the owner has a populated `~/.codecora/uteke`
/// and the new location does not exist yet, the memory looks lost and the
/// first recall would download the model again. `None` when there is nothing
/// to say: no `CODECORA_HOME` (or a relative one), an owner `UTEKE_HOME`, no
/// old home, or the new location already exists.
pub fn store_move_warning(
    codecora_home: Option<&str>,
    uteke_home: Option<&str>,
    home: Option<&std::path::Path>,
) -> Option<String> {
    let cc = std::path::Path::new(codecora_home.filter(|s| !s.is_empty())?);
    if !cc.is_absolute() || uteke_home.is_some_and(|s| !s.is_empty()) {
        return None;
    }
    let old = home?.join(".codecora").join("uteke");
    let new = cc.join("uteke");
    if old == new || !old.is_dir() || new.exists() {
        return None;
    }
    Some(format!(
        "  ! uteke: CODECORA_HOME is set, so uteke now uses {} (store, embedding model, \
         config), but {} exists and is not used; export UTEKE_HOME={} to keep it, or \
         copy or symlink it (at least its models/ directory) to the new location",
        new.display(),
        old.display(),
        old.display()
    ))
}

fn fmt(v: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// ` (or: tole setup cora)` when the sibling has a setup shortcut.
fn setup_hint(s: &Sibling) -> String {
    s.setup
        .map(|c| format!(" (or run `{c}`)"))
        .unwrap_or_default()
}

/// One report line, in `tole doctor`'s `✓`/`✗` style. Never a blocker:
/// both siblings are optional.
pub fn line(s: &Sibling, status: &Status) -> String {
    let min = fmt(s.min);
    let hint = setup_hint(s);
    match status {
        Status::Supported(v) => format!("  ✓ {} {} (supported ≥ {min})", s.bin, fmt(*v)),
        Status::TooOld(v) => format!(
            "  ✗ {} {} is older than the supported {min} — upgrade: {}{hint}",
            s.bin,
            fmt(*v),
            s.install
        ),
        Status::Missing => format!(
            "  - {} not installed (optional: {}) — install: {}{hint}",
            s.bin, s.role, s.install
        ),
        Status::Unknown(why) => format!(
            "  ? {} version unknown ({why}); supported ≥ {min} — upgrade: {}{hint}",
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
        assert!(l.contains("tole setup cora"), "{l}");
    }

    #[test]
    fn only_cora_has_a_setup_shortcut() {
        assert!(line(&SIBLINGS[0], &Status::Missing).contains("tole setup cora"));
        assert!(!line(&SIBLINGS[1], &Status::Missing).contains("tole setup"));
    }

    #[test]
    fn store_move_warns_only_when_the_old_home_would_be_orphaned() {
        let root = std::env::temp_dir().join(format!("tole-eco-992-{}", std::process::id()));
        let home = root.join("home");
        let cc = root.join("cc");
        std::fs::create_dir_all(home.join(".codecora/uteke")).unwrap();
        std::fs::create_dir_all(&cc).unwrap();
        let (h, c) = (Some(home.as_path()), cc.to_str());
        let w = store_move_warning(c, None, h).expect("warns");
        assert!(w.contains("UTEKE_HOME="), "{w}");
        assert!(w.contains(&cc.join("uteke").display().to_string()), "{w}");
        // An owner UTEKE_HOME, no CODECORA_HOME, a relative one: silent.
        assert!(store_move_warning(c, Some("/x"), h).is_none());
        assert!(store_move_warning(None, None, h).is_none());
        assert!(store_move_warning(Some(""), None, h).is_none());
        assert!(store_move_warning(Some("rel"), None, h).is_none());
        // The new home exists: nothing is lost.
        std::fs::create_dir_all(cc.join("uteke")).unwrap();
        assert!(store_move_warning(c, None, h).is_none());
        // No old home to lose.
        let bare = root.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::remove_dir_all(cc.join("uteke")).unwrap();
        assert!(store_move_warning(c, None, Some(bare.as_path())).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
