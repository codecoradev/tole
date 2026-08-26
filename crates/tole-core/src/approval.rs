//! E4 — approval gate.
//!
//! `AllowlistApprover` decides from a static config: deterministic,
//! testable, lives in core. `InteractiveApprover` (y/N prompt) is a host
//! concern and is injected via the trait from the CLI crate.

use serde::{Deserialize, Serialize};

/// Decision on a Write/Destructive tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Execute without asking.
    Allow,
    /// Ask the human (host renders the prompt).
    Ask,
    /// Refuse outright.
    Deny,
}

/// Approval verdict after the host has resolved any human interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
}

/// Config-driven approver: a tool-name allowlist plus the default for
/// everything not listed. `Decision::Ask` maps to `Deny` inside core runs
/// (no human present) — the host maps it to an interactive prompt.
#[derive(Debug, Clone)]
pub struct AllowlistApprover {
    allowed_tools: Vec<String>,
    default: Decision,
}

impl Default for AllowlistApprover {
    fn default() -> Self {
        Self {
            allowed_tools: Vec::new(),
            default: Decision::Deny,
        }
    }
}

impl AllowlistApprover {
    pub fn new(allowed_tools: Vec<String>, default: Decision) -> Self {
        Self {
            allowed_tools,
            default,
        }
    }

    /// Core-side decision: `Ask` collapses to `Deny` (the host is
    /// responsible for turning `Ask` into a prompt before reaching core).
    pub fn decide(&self, tool: &str) -> Verdict {
        if self.allowed_tools.iter().any(|t| t == tool) {
            return Verdict::Allow;
        }
        match self.default {
            Decision::Allow => Verdict::Allow,
            Decision::Ask | Decision::Deny => Verdict::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_allows_listed_tools() {
        let a = AllowlistApprover::new(vec!["write_file".into()], Decision::Deny);
        assert_eq!(a.decide("write_file"), Verdict::Allow);
        assert_eq!(a.decide("rm_rf"), Verdict::Deny);
    }

    #[test]
    fn ask_collapses_to_deny_in_core() {
        let a = AllowlistApprover::new(vec![], Decision::Ask);
        assert_eq!(a.decide("anything"), Verdict::Deny);
    }

    #[test]
    fn default_allow_permits_unlisted() {
        let a = AllowlistApprover::new(vec![], Decision::Allow);
        assert_eq!(a.decide("anything"), Verdict::Allow);
    }
}
