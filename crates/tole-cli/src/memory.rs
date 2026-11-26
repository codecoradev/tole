//! The memory loop's backend and room resolution.

use anyhow::Result;
use tole_cli::tool_roster::binary_available;

/// Memory backend resolution: `chosen` is the already-resolved backend
/// name (`--memory` flag > `TOLE_MEMORY` env > project config, see
/// `config_apply::Settings`); `uteke` is the only backend. The room follows
/// the ecosystem `repo-<dir>` convention unless `TOLE_MEMORY_NAMESPACE`
/// (kept under its old name) overrides it. A missing uteke binary degrades to a warning + no-op
/// (the same probe contract as the uteke tools).
#[cfg(feature = "shell-tools")]
pub(crate) fn resolve_memory(
    chosen: Option<&str>,
    profile_room: Option<&str>,
) -> Result<Option<tole_core::memory::MemoryConfig>> {
    let chosen = chosen
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(backend) = chosen else {
        return Ok(None);
    };
    if backend != "uteke" {
        anyhow::bail!("unsupported memory backend {backend:?} (only 'uteke' is available)");
    }
    if !binary_available("uteke") {
        eprintln!(
            "tole: memory backend 'uteke' requested but the binary is missing — memory loop disabled"
        );
        return Ok(None);
    }
    let mut cfg = tole_core::memory::MemoryConfig::for_cwd("uteke", &std::env::current_dir()?);
    cfg.room = memory_room(
        &cfg.room,
        profile_room,
        std::env::var(tole_core::child_env::MEMORY_ROOM)
            .ok()
            .as_deref(),
    );
    Ok(Some(cfg))
}

/// The memory loop's room: a child's mailbox (TOLE_MEMORY_NAMESPACE) >
/// the selected profile's room (#584) > the `repo-<dir>` default.
#[cfg(feature = "shell-tools")]
fn memory_room(default: &str, profile_room: Option<&str>, env_room: Option<&str>) -> String {
    fn pick(r: Option<&str>) -> Option<&str> {
        r.map(str::trim).filter(|r| !r.is_empty())
    }
    match pick(env_room).or(pick(profile_room)) {
        Some(r) => tole_core::memory::sanitize_namespace(r),
        None => default.to_string(),
    }
}

#[cfg(all(test, feature = "shell-tools"))]
mod memory_room_tests {
    /// #584: a profile moves the memory loop to its own room, but a child's
    /// mailbox room (the result channel back to its parent) always wins.
    #[test]
    fn child_mailbox_beats_profile_room_beats_repo_default() {
        let r = super::memory_room;
        assert_eq!(r("repo-tole", None, None), "repo-tole");
        assert_eq!(r("repo-tole", Some("profile-coder"), None), "profile-coder");
        assert_eq!(r("repo-tole", Some("  "), None), "repo-tole");
        assert_eq!(
            r("repo-tole", Some("profile-coder"), Some("agent-a-1")),
            "agent-a-1"
        );
        assert_eq!(r("repo-tole", None, Some("-bad room")), "_-bad_room");
    }
}
