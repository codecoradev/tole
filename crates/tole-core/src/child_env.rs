//! The child-agent launch contract (#171, #380, #383): the environment a
//! parent's `agent_start` gives a child `tole run`, and the one place the
//! child reads it back. The parent writes it with [`ChildLaunch`]; the
//! child host reads it with [`depth`], [`sibling`] and [`role`], so a
//! renamed variable cannot compile on one side and silently break the
//! other.
//!
//! Not behind `shell-tools`: an embedder that never spawns children still
//! reads the contract when it runs as one.

use crate::handoff::Role;
use std::ffi::OsString;
use std::path::PathBuf;

/// Child depth; `1` in a child (the structural depth cap: a child gets
/// no agent tools and may not respawn tole).
pub const DEPTH: &str = "TOLE_AGENT_DEPTH";
/// The child's agent id (`a-<hex>-<hex>-<hex>`).
pub const AGENT_ID: &str = "TOLE_AGENT_ID";
/// The touch log shared by the siblings of one parent (#380).
pub const TOUCH_LOG: &str = "TOLE_AGENT_TOUCH_LOG";
/// The child's handoff role (#383), when it has one.
pub const ROLE: &str = "TOLE_AGENT_ROLE";
/// Where a role child writes its handoff document (#383).
pub const HANDOFF: &str = "TOLE_AGENT_HANDOFF";
/// The memory backend; children always run the uteke memory loop, which
/// is how their result reaches the parent's mailbox.
pub const MEMORY: &str = "TOLE_MEMORY";
/// The uteke room of the memory loop; for a child, its mailbox room
/// (`agent-<id>`). The name predates rooms.
pub const MEMORY_ROOM: &str = "TOLE_MEMORY_NAMESPACE";

/// Everything a parent passes to one child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLaunch {
    /// The child's agent id.
    pub id: String,
    /// The touch log shared with its siblings.
    pub touch_log: PathBuf,
    /// The mailbox room the child's settle summary lands in.
    pub mailbox_room: String,
    /// The handoff role and the handoff file, for a role child.
    pub role: Option<(Role, PathBuf)>,
}

impl ChildLaunch {
    /// The environment variables to set on the child process.
    pub fn vars(&self) -> Vec<(&'static str, OsString)> {
        let mut vars = vec![
            (DEPTH, OsString::from("1")),
            (AGENT_ID, OsString::from(&self.id)),
            (TOUCH_LOG, self.touch_log.clone().into_os_string()),
            (MEMORY_ROOM, OsString::from(&self.mailbox_room)),
            (MEMORY, OsString::from("uteke")),
        ];
        if let Some((role, handoff)) = &self.role {
            vars.push((ROLE, OsString::from(role.as_str())));
            vars.push((HANDOFF, handoff.clone().into_os_string()));
        }
        vars
    }
}

/// This process's child depth: `0` when not a child (or unparseable).
pub fn depth(env: &dyn Fn(&str) -> Option<String>) -> u32 {
    env(DEPTH).and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// This child's agent id and the shared touch log (#380). `None` when this
/// is not a tracked child, or when the id is not a well-formed agent id:
/// it ends up in the shared log, so it is never trusted blindly.
pub fn sibling(env: &dyn Fn(&str) -> Option<String>) -> Option<(String, PathBuf)> {
    let agent = env(AGENT_ID)?.trim().to_string();
    let log = env(TOUCH_LOG)?.trim().to_string();
    if !crate::sibling_watch::valid_agent_id(&agent) || log.is_empty() {
        return None;
    }
    Some((agent, PathBuf::from(log)))
}

/// This child's handoff role and handoff file (#383). Both must be present
/// and the role must parse; a role without a path is no role.
pub fn role(env: &dyn Fn(&str) -> Option<String>) -> Option<(Role, PathBuf)> {
    let role = Role::parse(env(ROLE)?.trim())?;
    let path = env(HANDOFF)?.trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some((role, PathBuf::from(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(vars: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string_lossy().into_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn launch(role: Option<(Role, PathBuf)>) -> ChildLaunch {
        ChildLaunch {
            id: "a-1f-2-0".into(),
            touch_log: PathBuf::from("/agents/touch.log"),
            mailbox_room: "agent-a-1f-2-0".into(),
            role,
        }
    }

    /// The contract round trip: what the parent writes is exactly what the
    /// child reads back.
    #[test]
    fn a_child_reads_back_what_its_parent_launched() {
        let env = env_of(launch(Some((Role::Fix, PathBuf::from("/agents/a/handoff.json")))).vars());
        assert_eq!(depth(&env), 1);
        assert_eq!(
            sibling(&env),
            Some(("a-1f-2-0".into(), PathBuf::from("/agents/touch.log")))
        );
        assert_eq!(
            role(&env),
            Some((Role::Fix, PathBuf::from("/agents/a/handoff.json")))
        );
        assert_eq!(env(MEMORY).as_deref(), Some("uteke"));
        assert_eq!(env(MEMORY_ROOM).as_deref(), Some("agent-a-1f-2-0"));

        let plain = env_of(launch(None).vars());
        assert_eq!(role(&plain), None, "no role without a role launch");
    }

    #[test]
    fn a_parent_process_reads_as_no_child() {
        let none = |_: &str| None;
        assert_eq!(depth(&none), 0);
        assert_eq!(sibling(&none), None);
        assert_eq!(role(&none), None);
    }

    #[test]
    fn malformed_values_are_not_trusted() {
        let bad = |k: &str| match k {
            DEPTH => Some("one".into()),
            AGENT_ID => Some("../../etc".into()),
            TOUCH_LOG => Some("/x".into()),
            ROLE => Some("boss".into()),
            HANDOFF => Some("/x".into()),
            _ => None,
        };
        assert_eq!(depth(&bad), 0);
        assert_eq!(sibling(&bad), None);
        assert_eq!(role(&bad), None);
        let blank_handoff = |k: &str| match k {
            ROLE => Some("fix".into()),
            HANDOFF => Some("  ".into()),
            _ => None,
        };
        assert_eq!(role(&blank_handoff), None);
    }
}
