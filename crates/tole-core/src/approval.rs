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

/// Suffix of a path-guarded allow pattern (#437): `write_file@workspace`
/// allows `write_file` only when its `path` input passes
/// [`workspace_guard_ok`] — so files that another tool may later EXECUTE
/// (`.git/hooks/*`, `.envrc`, `.vscode/`, `.github/workflows/`, build and
/// task files, scripts) keep prompting.
pub const WORKSPACE_GUARD: &str = "@workspace";

/// File names (any directory, case-insensitive) that build tools, task
/// runners, package managers or agents (tole included) execute or load as
/// instructions. A write to one keeps prompting under the guard.
const EXECUTED_FILE_NAMES: &[&str] = &[
    "build.rs",
    "cargo.toml",
    "makefile",
    "gnumakefile",
    "justfile",
    "taskfile.yml",
    "taskfile.yaml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "conftest.py",
    "noxfile.py",
    "tox.ini",
    "rakefile",
    "gemfile",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "pom.xml",
    "cmakelists.txt",
    "meson.build",
    "dockerfile",
    "docker-compose.yml",
    "compose.yaml",
    "skill.md",
    "agents.md",
    "claude.md",
];

/// Script extensions a write to which keeps prompting under the guard.
const SCRIPT_EXTENSIONS: &[&str] = &[".sh", ".bash", ".zsh", ".ps1", ".bat", ".cmd"];

/// Does `path` qualify for a [`WORKSPACE_GUARD`] pattern? Relative and
/// non-empty; no component starting with `.` (a lone `.` is fine; `..` is
/// not); not an [`EXECUTED_FILE_NAMES`] file; not a script. `\` counts as a
/// separator too, so a Windows-style path cannot hide a component. A
/// denylist of the common cases, not a sandbox: ordinary source files stay
/// writable.
pub fn workspace_guard_ok(path: &str) -> bool {
    let p = path.trim();
    if p.is_empty() || p.starts_with('/') || p.starts_with('\\') || p.contains(':') {
        return false;
    }
    let parts: Vec<&str> = p
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let Some(name) = parts.last().map(|n| n.to_ascii_lowercase()) else {
        return false;
    };
    parts.iter().all(|c| !c.starts_with('.'))
        && !EXECUTED_FILE_NAMES.contains(&name.as_str())
        && !SCRIPT_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

/// Does an ALLOW pattern cover this request? A plain pattern globs the
/// tool name; a [`WORKSPACE_GUARD`] pattern also requires the request's
/// `path` input to pass [`workspace_guard_ok`]. The one place both
/// approvers match allow patterns.
pub fn pattern_allows(pattern: &str, req: &ToolRequest<'_>) -> bool {
    match pattern.strip_suffix(WORKSPACE_GUARD) {
        Some(name) => {
            glob_match(name, req.tool)
                && req
                    .input
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(workspace_guard_ok)
        }
        None => glob_match(pattern, req.tool),
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
    /// `deny_only` mode: patterns DENY instead of allow (the default then
    /// governs non-matches, normally Allow).
    deny_mode: bool,
}

impl Default for AllowlistApprover {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            default: Decision::Deny,
            deny_mode: false,
        }
    }
}

impl AllowlistApprover {
    /// Build an approver whose `default` verdict governs everything.
    ///
    /// # Fail-closed semantics (#283)
    /// Patterns can only ever *widen* access when the default is
    /// [`Decision::Allow`] (where they are redundant). With
    /// `Decision::Deny` / `Decision::Ask` (Ask resolves to Deny in
    /// core) the default is the final verdict and the patterns are
    /// ignored: `new(vec!["bomb"], Decision::Deny)` DENIES "bomb". It
    /// used to allow listed tools, contradicting the configured default.
    /// `Destructive` is always denied regardless.
    ///
    /// To allow a specific set use [`allow_only`]; to deny a specific
    /// set use [`deny_only`].
    pub fn new(patterns: Vec<String>, default: Decision) -> Self {
        let patterns = if default == Decision::Allow {
            patterns
        } else {
            Vec::new()
        };
        Self {
            patterns,
            default,
            deny_mode: false,
        }
    }

    /// Allow exactly the listed glob patterns; deny everything else.
    /// The reading order matches the runtime behavior one-to-one.
    pub fn allow_only(patterns: Vec<String>) -> Self {
        Self {
            patterns,
            default: Decision::Deny,
            deny_mode: false,
        }
    }

    /// Deny exactly the listed glob patterns; allow everything else.
    /// Destructive tools remain always-denied regardless of patterns.
    pub fn deny_only(patterns: Vec<String>) -> Self {
        Self {
            patterns,
            default: Decision::Allow,
            deny_mode: true,
        }
    }

    fn decision(&self, req: &ToolRequest<'_>) -> Verdict {
        if req.risk == Risk::Destructive {
            return Verdict::Deny;
        }
        let matched = if self.deny_mode {
            // A deny pattern ignores the guard: denying more is the safe side.
            self.patterns
                .iter()
                .any(|p| glob_match(p.strip_suffix(WORKSPACE_GUARD).unwrap_or(p), req.tool))
        } else {
            self.patterns.iter().any(|p| pattern_allows(p, req))
        };
        if self.deny_mode {
            // deny_only: patterns deny, non-matches fall to the default.
            if matched {
                return Verdict::Deny;
            }
        } else if matched {
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
        let a = AllowlistApprover::allow_only(vec!["write_file".into()]);
        assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Allow);
        assert_eq!(a.decide(&req("rm_rf", Risk::Write)), Verdict::Deny);
    }

    #[test]
    fn allow_only_is_unambiguous() {
        let a = AllowlistApprover::allow_only(vec!["read_*".into()]);
        assert_eq!(a.decide(&req("read_file", Risk::ReadOnly)), Verdict::Allow);
        assert_eq!(a.decide(&req("rm_rf", Risk::Write)), Verdict::Deny);
    }

    #[test]
    fn deny_only_denies_listed_and_allows_rest() {
        let a = AllowlistApprover::deny_only(vec!["rm_*".into()]);
        assert_eq!(a.decide(&req("rm_rf", Risk::Write)), Verdict::Deny);
        assert_eq!(a.decide(&req("read_file", Risk::ReadOnly)), Verdict::Allow);
    }

    /// #283: with a Deny/Ask default the verdict is final — listed
    /// tools are NOT allowed (fail closed). `allow_only` is the way to
    /// build an allowlist.
    #[test]
    fn new_with_deny_default_denies_listed_tools() {
        let a = AllowlistApprover::new(vec!["bomb".into()], Decision::Deny);
        assert_eq!(a.decide(&req("bomb", Risk::Write)), Verdict::Deny);
        assert_eq!(a.decide(&req("other", Risk::Write)), Verdict::Deny);
        let a = AllowlistApprover::new(vec!["bomb".into()], Decision::Ask);
        assert_eq!(a.decide(&req("bomb", Risk::Write)), Verdict::Deny);
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
        let a = AllowlistApprover::allow_only(vec!["read_*".into()]);
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

    #[test]
    fn workspace_guard_keeps_dot_paths_and_escapes_prompting() {
        for ok in [
            "report.md",
            "docs/out.md",
            "./a/b.txt",
            "a/./b",
            "src/main.rs",
        ] {
            assert!(workspace_guard_ok(ok), "{ok}");
        }
        for bad in [
            "",
            ".git/hooks/pre-commit",
            "sub/.git/config",
            ".envrc",
            ".vscode/tasks.json",
            ".tole/config.toml",
            "../x",
            "a/../b",
            "/etc/passwd",
            "\\\\server\\share",
            "C:\\x",
            "a\\.git\\hooks",
            ".",
            "build.rs",
            "crates/x/build.rs",
            "Makefile",
            "sub/package.json",
            "Cargo.toml",
            "skills/x/SKILL.md",
            "AGENTS.md",
            "scripts/deploy.sh",
            "run.PS1",
        ] {
            assert!(!workspace_guard_ok(bad), "{bad}");
        }
        let guarded = AllowlistApprover::allow_only(vec!["write_file@workspace".into()]);
        let call = |input: Value| {
            guarded.decide(&ToolRequest {
                tool: "write_file",
                risk: Risk::Write,
                input: &input,
                description: String::new(),
            })
        };
        assert_eq!(call(serde_json::json!({"path": "out.md"})), Verdict::Allow);
        assert_eq!(
            call(serde_json::json!({"path": ".git/hooks/x"})),
            Verdict::Deny
        );
        assert_eq!(
            call(serde_json::json!({})),
            Verdict::Deny,
            "no path, no guard pass"
        );
        // The guard pattern never matches another tool.
        assert_eq!(
            guarded.decide(&req("edit_file", Risk::Write)),
            Verdict::Deny
        );
        // As a DENY pattern it denies the tool outright.
        let deny = AllowlistApprover::deny_only(vec!["write_file@workspace".into()]);
        assert_eq!(deny.decide(&req("write_file", Risk::Write)), Verdict::Deny);
    }
}
