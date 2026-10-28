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
    /// The resolved write target from `Tool::approval_target` (#437):
    /// relative to the canonical jail root, symlinks followed. `None` for
    /// tools that do not report one.
    pub target: Option<String>,
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
    // Windows ignores trailing dots and spaces (`Makefile.` is `Makefile`),
    // and an 8.3 short name (`MAKEFI~1`) can alias any long one: normalize
    // the first, refuse the second.
    let raw: Vec<&str> = p
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if raw.contains(&"..") {
        return false;
    }
    let parts: Vec<&str> = raw.iter().map(|c| c.trim_end_matches(['.', ' '])).collect();
    // A component made only of dots/spaces (`...`, `. .`) is no name.
    if parts.iter().any(|c| c.is_empty()) {
        return false;
    }
    if parts.iter().any(|c| has_short_name_marker(c)) {
        return false;
    }
    let Some(name) = parts.last().map(|n| n.to_ascii_lowercase()) else {
        return false;
    };
    parts.iter().all(|c| !c.starts_with('.'))
        && !EXECUTED_FILE_NAMES.contains(&name.as_str())
        && !SCRIPT_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

/// `~` followed by a digit: a Windows 8.3 short-name alias.
fn has_short_name_marker(c: &str) -> bool {
    c.as_bytes()
        .windows(2)
        .any(|w| w[0] == b'~' && w[1].is_ascii_digit())
}

/// Prefix of a DENY pattern in an allow list (#438): `!run_command:rm *`.
/// A deny wins over every allow, `--yes` included, and travels wherever
/// allow patterns travel (config `allow`, child agents, ACP, serve).
pub const DENY_PREFIX: char = '!';

/// Prefix of a command pattern (#438): `run_command:git status *` matches
/// `run_command` calls by their argv, not just the tool name.
pub const COMMAND_PATTERN_PREFIX: &str = "run_command:";

/// Programs that run ANOTHER program from their arguments. A deny
/// pattern cannot see through them (`env rm`, `sh -c 'rm …'`), so while
/// any `run_command` deny is configured a call to one of these always
/// goes to the human (and is denied where nobody can be asked), even
/// under `--yes`.
const COMMAND_WRAPPERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "env", "sudo", "doas", "xargs", "busybox", "nohup",
    "nice", "timeout", "command", "exec", "time", "watch", "su", "runuser", "chroot", "setsid",
    "stdbuf", "unbuffer", "parallel",
];

/// The argv a `run_command` call will execute: the SAME splitter the tool
/// uses (no shell), so what is matched is what runs. `None` for another
/// tool, a missing / unsplittable command, or a build without
/// `run_command`.
fn command_argv(req: &ToolRequest<'_>) -> Option<Vec<String>> {
    if req.tool != "run_command" {
        return None;
    }
    #[cfg(feature = "shell-tools")]
    {
        let line = req.input.get("command")?.as_str()?;
        crate::run_command::RunCommandTool::split_argv(line)
            .ok()
            .filter(|a| !a.is_empty())
    }
    #[cfg(not(feature = "shell-tools"))]
    {
        None
    }
}

/// The last path component of a program (`/usr/bin/rm` → `rm`).
fn program_name(arg0: &str) -> &str {
    arg0.rsplit(['/', '\\']).next().unwrap_or(arg0)
}

/// Match a command pattern's tokens against an argv. Each token is a
/// [`glob_match`] over ONE argument; a final lone `*` token stands for
/// any remaining arguments (zero or more), otherwise the counts must be
/// equal. For an ALLOW the program token is compared literally with
/// `argv[0]` (no globbing, no path tricks: `run_command:*` allows
/// nothing); for a DENY it is a glob over `argv[0]` or its file name,
/// so `/bin/rm` cannot slip past `rm *`.
#[cfg(feature = "shell-tools")]
fn argv_matches(pattern: &str, argv: &[String], deny: bool) -> bool {
    let Ok(pat) = crate::run_command::RunCommandTool::split_argv(pattern) else {
        return false;
    };
    let Some((p0, rest_pat)) = pat.split_first() else {
        return false;
    };
    let (a0, rest) = (&argv[0], &argv[1..]);
    let program_ok = if deny {
        glob_match(p0, a0) || glob_match(p0, program_name(a0))
    } else {
        p0 == a0
    };
    if !program_ok {
        return false;
    }
    let (fixed, open) = match rest_pat.split_last() {
        Some((last, head)) if last == "*" => (head, true),
        _ => (rest_pat, false),
    };
    if rest.len() < fixed.len() || (!open && rest.len() != fixed.len()) {
        return false;
    }
    fixed.iter().zip(rest).all(|(p, a)| glob_match(p, a))
}

/// Does a POSITIVE pattern (no [`DENY_PREFIX`]) match this request?
/// - `run_command:<argv pattern>`: `run_command` calls whose argv matches
///   ([`argv_matches`]);
/// - `<tool>@workspace`: the tool, and its RESOLVED write target
///   ([`ToolRequest::target`], symlinks followed — never the raw `path`
///   text) passes [`workspace_guard_ok`]; no target = no match;
/// - otherwise a [`glob_match`] over the tool name.
fn pattern_matches(pattern: &str, req: &ToolRequest<'_>, deny: bool) -> bool {
    if let Some(cmd) = pattern.strip_prefix(COMMAND_PATTERN_PREFIX) {
        #[cfg(feature = "shell-tools")]
        return command_argv(req).is_some_and(|argv| argv_matches(cmd, &argv, deny));
        // No run_command in this build: a command pattern matches nothing.
        #[cfg(not(feature = "shell-tools"))]
        {
            let _ = cmd;
            return false;
        }
    }
    match pattern.strip_suffix(WORKSPACE_GUARD) {
        // A deny ignores the guard: denying more is the safe side.
        Some(name) if deny => glob_match(name, req.tool),
        Some(name) => {
            glob_match(name, req.tool) && req.target.as_deref().is_some_and(workspace_guard_ok)
        }
        None => glob_match(pattern, req.tool),
    }
}

/// Does an ALLOW pattern cover this request? Deny patterns never allow.
pub fn pattern_allows(pattern: &str, req: &ToolRequest<'_>) -> bool {
    !pattern.starts_with(DENY_PREFIX) && pattern_matches(pattern, req, false)
}

/// Does a DENY pattern (`!…`) cover this request?
pub fn pattern_denies(pattern: &str, req: &ToolRequest<'_>) -> bool {
    pattern
        .strip_prefix(DENY_PREFIX)
        .is_some_and(|p| pattern_matches(p, req, true))
}

/// What an allow list says about one request, before any default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternVerdict {
    /// A deny pattern matched: refuse without asking.
    Deny,
    /// Only a human may decide (a command wrapper while a `run_command`
    /// deny is configured); refuse where nobody can be asked.
    Ask,
    /// An allow pattern matched (and no deny did).
    Allow,
    /// Nothing matched: the approver's own default applies.
    NoMatch,
}

/// THE allow-list evaluation both approvers use (#438): deny first, then
/// the wrapper rule, then allow.
pub fn pattern_verdict(patterns: &[String], req: &ToolRequest<'_>) -> PatternVerdict {
    if patterns.iter().any(|p| pattern_denies(p, req)) {
        return PatternVerdict::Deny;
    }
    let command_deny = patterns.iter().any(|p| {
        p.strip_prefix(DENY_PREFIX)
            .is_some_and(|q| q.starts_with(COMMAND_PATTERN_PREFIX))
    });
    if command_deny {
        if let Some(argv) = command_argv(req) {
            let name = program_name(&argv[0]).to_ascii_lowercase();
            if COMMAND_WRAPPERS.contains(&name.as_str()) {
                return PatternVerdict::Ask;
            }
        }
    }
    if patterns.iter().any(|p| pattern_allows(p, req)) {
        return PatternVerdict::Allow;
    }
    PatternVerdict::NoMatch
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
        if self.deny_mode {
            // deny_only: every pattern denies (a `!` is redundant here);
            // non-matches fall to the default.
            let matched = self
                .patterns
                .iter()
                .any(|p| pattern_matches(p.strip_prefix(DENY_PREFIX).unwrap_or(p), req, true));
            if matched {
                return Verdict::Deny;
            }
        } else {
            match pattern_verdict(&self.patterns, req) {
                // Nobody to ask here: Ask is a refusal.
                PatternVerdict::Deny | PatternVerdict::Ask => return Verdict::Deny,
                PatternVerdict::Allow => return Verdict::Allow,
                PatternVerdict::NoMatch => {}
            }
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
            target: None,
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
            "Makefile.",
            "Makefile. .",
            "MAKEFI~1",
            "docs/BUILD~1.RS",
            ".git./hooks/x",
        ] {
            assert!(!workspace_guard_ok(bad), "{bad}");
        }
        let guarded = AllowlistApprover::allow_only(vec!["write_file@workspace".into()]);
        let input = serde_json::json!({"path": "out.md"});
        let call = |target: Option<&str>| {
            guarded.decide(&ToolRequest {
                tool: "write_file",
                risk: Risk::Write,
                // The raw input never decides: the guard reads the target.
                input: &input,
                description: String::new(),
                target: target.map(str::to_string),
            })
        };
        assert_eq!(call(Some("out.md")), Verdict::Allow);
        assert_eq!(call(Some(".git/hooks/x")), Verdict::Deny);
        assert_eq!(call(None), Verdict::Deny, "no target, no guard pass");
        // The guard pattern never matches another tool.
        assert_eq!(
            guarded.decide(&req("edit_file", Risk::Write)),
            Verdict::Deny
        );
        // As a DENY pattern it denies the tool outright.
        let deny = AllowlistApprover::deny_only(vec!["write_file@workspace".into()]);
        assert_eq!(deny.decide(&req("write_file", Risk::Write)), Verdict::Deny);
    }

    // ------------------------------------------- command patterns (#438)

    #[cfg(feature = "shell-tools")]
    fn cmd_verdict(patterns: &[&str], line: &str) -> PatternVerdict {
        let input = serde_json::json!({ "command": line });
        let req = ToolRequest {
            tool: "run_command",
            risk: Risk::Write,
            input: &input,
            description: String::new(),
            target: None,
        };
        let pats: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        pattern_verdict(&pats, &req)
    }

    #[cfg(feature = "shell-tools")]
    #[test]
    fn command_allow_patterns_match_the_argv_that_runs() {
        use PatternVerdict::*;
        let allow = ["run_command:git status *", "run_command:docker ps"];
        assert_eq!(cmd_verdict(&allow, "git status"), Allow);
        assert_eq!(cmd_verdict(&allow, "git status --porcelain -b"), Allow);
        assert_eq!(cmd_verdict(&allow, "docker ps"), Allow);
        // Exact arity without a trailing `*`.
        assert_eq!(cmd_verdict(&allow, "docker ps -a"), NoMatch);
        assert_eq!(cmd_verdict(&allow, "git push"), NoMatch);
        // No shell: `;` is just an argument, never a second command, and
        // the program token is literal (no path or glob tricks).
        assert_eq!(cmd_verdict(&allow, "git status; rm -rf /"), NoMatch);
        // Spaced out it matches, but runs only `git` with literal args
        // `;`, `rm`, `-rf`, `/` — nothing is chained.
        assert_eq!(cmd_verdict(&allow, "git status ; rm -rf /"), Allow);
        assert_eq!(
            cmd_verdict(&["run_command:echo *"], "rm -rf / ; echo"),
            NoMatch
        );
        assert_eq!(cmd_verdict(&allow, "/usr/bin/git status"), NoMatch);
        assert_eq!(cmd_verdict(&["run_command:*"], "rm -rf /"), NoMatch);
        // Per-argument globs.
        assert_eq!(
            cmd_verdict(
                &["run_command:cargo test -p tole-*"],
                "cargo test -p tole-cli"
            ),
            Allow
        );
        // A command pattern never matches another tool, and an
        // unterminated quote never matches.
        assert_eq!(cmd_verdict(&allow, "git status 'oops"), NoMatch);
        assert_eq!(
            AllowlistApprover::allow_only(vec!["run_command:git *".into()])
                .decide(&req("git", Risk::Write)),
            Verdict::Deny
        );
    }

    #[cfg(feature = "shell-tools")]
    #[test]
    fn deny_wins_sees_through_paths_and_sends_wrappers_to_a_human() {
        use PatternVerdict::*;
        let pats = ["run_command", "!run_command:rm *"];
        assert_eq!(cmd_verdict(&pats, "ls -la"), Allow);
        assert_eq!(cmd_verdict(&pats, "rm -rf build"), Deny);
        assert_eq!(cmd_verdict(&pats, "/bin/rm -rf build"), Deny);
        assert_eq!(
            cmd_verdict(&pats, "rm"),
            Deny,
            "trailing * allows zero args"
        );
        // Wrappers can hide the real program: ask, never auto-allow.
        assert_eq!(cmd_verdict(&pats, "env rm -rf build"), Ask);
        assert_eq!(cmd_verdict(&pats, "sh -c 'rm -rf build'"), Ask);
        assert_eq!(cmd_verdict(&pats, "/usr/bin/sudo ls"), Ask);
        // Without a command deny, wrappers follow the normal rules.
        assert_eq!(cmd_verdict(&["run_command"], "env ls"), Allow);
        // A tool-name deny.
        assert_eq!(cmd_verdict(&["*", "!run_command"], "ls"), Deny);
        // Headless (nobody to ask): Ask is a refusal.
        let a = AllowlistApprover::allow_only(pats.iter().map(|p| p.to_string()).collect());
        let input = serde_json::json!({ "command": "env rm x" });
        let r = ToolRequest {
            tool: "run_command",
            risk: Risk::Write,
            input: &input,
            description: String::new(),
            target: None,
        };
        assert_eq!(a.decide(&r), Verdict::Deny);
        // deny_only treats `run_command:` patterns command-wise too.
        let d = AllowlistApprover::deny_only(vec!["run_command:rm *".into()]);
        assert_eq!(d.decide(&r), Verdict::Allow, "env is not rm in deny_only");
        let input = serde_json::json!({ "command": "rm x" });
        let r = ToolRequest { input: &input, ..r };
        assert_eq!(d.decide(&r), Verdict::Deny);
    }

    #[test]
    fn deny_patterns_never_allow() {
        assert!(!pattern_allows(
            "!write_file",
            &req("write_file", Risk::Write)
        ));
        assert!(pattern_denies("!write_*", &req("write_file", Risk::Write)));
        let a = AllowlistApprover::allow_only(vec!["*".into(), "!write_file".into()]);
        assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Deny);
        assert_eq!(a.decide(&req("edit_file", Risk::Write)), Verdict::Allow);
    }
}
