//! Host wiring for the child-agent handoff (#383): a child started with a
//! role finds the role and the handoff path in its environment and gets the
//! `handoff_submit` tool. Outside such a child this is a no-op.

use tole_core::handoff::HandoffSubmitTool;

pub const ENV_ROLE: &str = tole_core::child_env::ROLE;
pub const ENV_HANDOFF: &str = tole_core::child_env::HANDOFF;

/// The `handoff_submit` tool for this process, when it is a role child.
/// Both variables must be present and the role must parse; anything else
/// (including a role set without a path) registers nothing.
pub fn tool_from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<HandoffSubmitTool> {
    let (role, path) = tole_core::child_env::role(env)?;
    Some(HandoffSubmitTool::new(path, role))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::tool::Tool;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn a_role_child_gets_the_tool_and_nothing_else_does() {
        let t = tool_from_env(&env(&[(ENV_ROLE, "fix"), (ENV_HANDOFF, "/x/handoff.json")]));
        assert_eq!(
            t.map(|t| t.name().to_string()),
            Some("handoff_submit".into())
        );
        assert!(tool_from_env(&env(&[])).is_none());
        assert!(
            tool_from_env(&env(&[(ENV_ROLE, "fix")])).is_none(),
            "no path"
        );
        assert!(tool_from_env(&env(&[(ENV_ROLE, "boss"), (ENV_HANDOFF, "/x")])).is_none());
        assert!(tool_from_env(&env(&[(ENV_ROLE, "fix"), (ENV_HANDOFF, " ")])).is_none());
    }
}
