//! E6 — approval gates: the `Approver` trait + glob allowlist.
//!
//! The core provides the policy *engine*; impls may live anywhere. Core
//! ships [`AllowlistApprover`] (config-driven, deterministic, Tier A
//! testable); the host (CLI) injects an interactive y/N approver via the
//! same trait. `Ask` decisions collapse to `Deny` inside non-interactive
//! runs — the host is responsible for turning `Ask` into a prompt before
//! core sees a verdict.
//!
//! Structural rule (PRD risk table): a `Destructive` tool can never be
//! auto-allowed by a static pattern. It is enforced in three layers, each
//! at its natural home:
//!
//! 1. registration: `ToolRegistry` refuses Destructive tools unless the
//!    wired approver is `interactive()`;
//! 2. [`AllowlistApprover`]: `Destructive` ⇒ `Deny`, even on pattern match;
//! 3. host impls: prompt for Destructive regardless of their patterns.

use crate::tool::Risk;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// A host-reviewable description of one pending tool call: everything a
/// human needs to make an informed Allow/Deny choice.
#[derive(Debug, Clone)]
pub struct ToolRequest<'a> {
    /// Registry name of the tool.
    pub tool: &'a str,
    /// Risk tier of the tool.
    pub risk: Risk,
    /// Raw tool input (JSON).
    pub input: &'a Value,
    /// One-line human summary from `Tool::describe`.
    pub description: String,
}

/// The approval policy engine. The turn loop consults this for every
/// non-ReadOnly call; implementations decide from config, a prompt,
/// or anything else the host dreams up.
///
/// Contract:
/// - `decide` must be deterministic for a given request *within one
///   process run* (durable logs reference approvals, replays re-ask).
/// - Implementations MUST NOT auto-allow `Destructive` calls without a
///   human in the loop (see module docs).
pub trait Approver: Send + Sync {
    /// Final verdict for one call. `Ask`-style policies must resolve to
    /// Allow/Deny here — the loop has no prompting machinery.
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict;

    /// True when a human is on the other end (e.g. a CLI prompt).
    /// Gates Destructive tool registration.
    fn interactive(&self) -> bool {
        false
    }
}

/// Minimal glob for tool-name patterns: `*` matches any run of chars
/// (including none), `?` matches exactly one. No character classes —
/// tool names are `[a-z0-9_]`, this is enough.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Config-driven approver: glob patterns over tool names plus the
/// default for everything not matched. Deterministic, Tier A testable,
/// lives in core. `Destructive` is always denied — patterns cannot opt
/// a Destructive tool into auto-allow (PRD).
#[derive(Debug, Clone)]
pub struct AllowlistApprover {
    patterns: Vec<String>,
    default: Decision,
}

impl Default for AllowlistApprover {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            default: Decision::Deny,
        }
    }
}

impl AllowlistApprover {
    pub fn new(patterns: Vec<String>, default: Decision) -> Self {
        Self { patterns, default }
    }

    fn decision(&self, req: &ToolRequest<'_>) -> Verdict {
        if req.risk == Risk::Destructive {
            return Verdict::Deny;
        }
        if self.patterns.iter().any(|p| glob_match(p, req.tool)) {
            return Verdict::Allow;
        }
        match self.default {
            Decision::Allow => Verdict::Allow,
            Decision::Ask | Decision::Deny => Verdict::Deny,
        }
    }
}

impl Approver for AllowlistApprover {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        self.decision(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str, risk: Risk) -> ToolRequest<'_> {
        ToolRequest {
            tool,
            risk,
            input: &Value::Null,
            description: String::new(),
        }
    }

    #[test]
    fn allowlist_allows_listed_tools() {
        let a = AllowlistApprover::new(vec!["write_file".into()], Decision::Deny);
        assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Allow);
        assert_eq!(a.decide(&req("rm_rf", Risk::Write)), Verdict::Deny);
    }

    #[test]
    fn ask_collapses_to_deny_in_core() {
        let a = AllowlistApprover::new(vec![], Decision::Ask);
        assert_eq!(a.decide(&req("anything", Risk::Write)), Verdict::Deny);
    }

    #[test]
    fn default_allow_permits_unlisted() {
        let a = AllowlistApprover::new(vec![], Decision::Allow);
        assert_eq!(a.decide(&req("anything", Risk::Write)), Verdict::Allow);
    }

    // --- glob patterns (E6 AC) ------------------------------------------

    #[test]
    fn glob_exact_and_star() {
        assert!(glob_match("cora_search", "cora_search"));
        assert!(!glob_match("cora_search", "cora_searchx"));
        assert!(glob_match("read_*", "read_file"));
        assert!(glob_match("read_*", "read_"));
        assert!(!glob_match("read_*", "write_file"));
        assert!(glob_match("*", "anything_at_all"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("file?", "file1"));
        assert!(!glob_match("file?", "file12"));
        assert!(!glob_match("file?", "file"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("*_file_*", "a_file_b"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b*c", "aXcYb"));
        assert!(glob_match("**", "x"));
    }

    #[test]
    fn glob_empty_pattern_and_text() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn allowlist_patterns_match_by_glob() {
        let a = AllowlistApprover::new(vec!["read_*".into()], Decision::Deny);
        assert_eq!(a.decide(&req("read_file", Risk::Write)), Verdict::Allow);
        assert_eq!(a.decide(&req("reads", Risk::Write)), Verdict::Deny);
    }

    // --- Destructive cannot be allowlisted (PRD) ------------------------

    #[test]
    fn destructive_is_denied_even_on_pattern_match() {
        let a = AllowlistApprover::new(vec!["*".into()], Decision::Allow);
        assert_eq!(a.decide(&req("rm_rf", Risk::Destructive)), Verdict::Deny);
        // Write is still allowlistable.
        assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Allow);
    }

    #[test]
    fn allowlist_is_not_interactive() {
        let a = AllowlistApprover::default();
        assert!(!a.interactive());
    }
}
