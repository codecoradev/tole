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

use crate::action_summary::ActionSummary;
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
    /// The structured, code-derived summary of the call (#1041), redacted and
    /// bounded. `None` when the request was built outside the registry; use
    /// [`ToolRequest::action_summary`] to always get one. Never an input to
    /// the allow/deny decision: the risk tier stays the authority.
    pub summary: Option<ActionSummary>,
}

impl ToolRequest<'_> {
    /// The summary every approval surface shows: the registry's, or the
    /// neutral "use the tool <name>" for a request built without one.
    pub fn action_summary(&self) -> ActionSummary {
        self.summary.clone().unwrap_or_else(|| {
            ActionSummary::unknown_with_description(self.tool, self.input, &self.description)
                .finalized(&crate::secrets::process_secrets())
        })
    }
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

/// The program as a DENY compares it: file name, ASCII-lowercased, a
/// Windows executable extension dropped — so `/bin/rm`, `RM` and
/// `rm.exe` are all `rm`.
fn deny_program(arg0: &str) -> String {
    let name = program_name(arg0).to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    name
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
        glob_match(&deny_program(p0), &deny_program(a0))
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

/// Does a PARENT allow pattern cover a CHILD allow pattern, i.e. can the
/// child never be allowed more than the parent (#174, #438)? Deny
/// patterns never cover anything. A child `run_command:` pattern is
/// checked per argument: the same literal program, each child argument
/// matched by the parent's argument at that position, extra arguments only
/// under a parent's trailing lone `*`, and a child's own trailing `*` only
/// under one. A parent tool-name pattern covering `run_command` covers
/// every child command pattern — unless the parent list holds a command
/// deny, which [`list_covers`] handles. Anything else is a glob over the
/// pattern text, with [`token_covers`] refusing a child glob wider than
/// the parent's.
pub fn pattern_covers(parent: &str, child: &str) -> bool {
    if parent.starts_with(DENY_PREFIX) || child.starts_with(DENY_PREFIX) {
        return false;
    }
    match (
        parent.strip_prefix(COMMAND_PATTERN_PREFIX),
        child.strip_prefix(COMMAND_PATTERN_PREFIX),
    ) {
        (Some(p), Some(c)) => command_pattern_covers(p, c),
        // A command pattern never covers a tool-wide (or other) pattern.
        (Some(_), None) => false,
        (None, Some(_)) => !parent.ends_with(WORKSPACE_GUARD) && glob_match(parent, "run_command"),
        (None, None) => token_covers(parent, child),
    }
}

/// Does `parent` (a glob) cover every text the child glob `child` can
/// match? A child without `*` / `?` is plain text: a glob match decides.
/// A child WITH them is covered only by the identical glob or a lone `*`
/// — anything finer would need real glob containment, and a wrong "yes"
/// widens a child (`a?c` must not cover `a*c`).
fn token_covers(parent: &str, child: &str) -> bool {
    if child.contains(['*', '?']) {
        parent == child || parent == "*"
    } else {
        glob_match(parent, child)
    }
}

/// Does a PARENT allow LIST cover one CHILD allow pattern (#438)? Like
/// [`pattern_covers`] over the parent's patterns, with one rule that needs
/// the whole list: under a parent `run_command` deny, the parent runs a
/// command unattended only through an explicit `run_command:` allow
/// ([`pattern_verdict`] fails closed), so a child command pattern may only
/// be covered by such a parent pattern — never by a bare `run_command`,
/// `run_*` or `*`.
pub fn list_covers(parent: &[String], child: &str) -> bool {
    let command_deny = parent.iter().any(|p| {
        p.strip_prefix(DENY_PREFIX)
            .is_some_and(|q| q.starts_with(COMMAND_PATTERN_PREFIX))
    });
    let child_is_command = child.starts_with(COMMAND_PATTERN_PREFIX);
    parent.iter().any(|pp| {
        (!(command_deny && child_is_command) || pp.starts_with(COMMAND_PATTERN_PREFIX))
            && pattern_covers(pp, child)
    })
}

#[cfg(feature = "shell-tools")]
fn command_pattern_covers(parent: &str, child: &str) -> bool {
    use crate::run_command::RunCommandTool;
    let (Ok(p), Ok(c)) = (
        RunCommandTool::split_argv(parent),
        RunCommandTool::split_argv(child),
    ) else {
        return false;
    };
    let split = |v: &[String]| -> (Vec<String>, bool) {
        match v.split_last() {
            Some((last, head)) if last == "*" => (head.to_vec(), true),
            _ => (v.to_vec(), false),
        }
    };
    let ((pf, p_open), (cf, c_open)) = (split(&p), split(&c));
    let (Some(p0), Some(c0)) = (pf.first(), cf.first()) else {
        return false;
    };
    if p0 != c0 || (c_open && !p_open) {
        return false;
    }
    if (p_open && cf.len() < pf.len()) || (!p_open && cf.len() != pf.len()) {
        return false;
    }
    pf.iter()
        .zip(&cf)
        .skip(1)
        .all(|(pt, ct)| token_covers(pt, ct))
}

/// Without `run_command` in the build only an identical pattern covers.
#[cfg(not(feature = "shell-tools"))]
fn command_pattern_covers(parent: &str, child: &str) -> bool {
    parent == child
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
            // Fail closed: a deny list can never name every way to reach a
            // program (interpreters, `find -exec`, `git -c`, `make`, ...).
            // While a command deny is set, a command runs unattended ONLY
            // when an explicit `run_command:` allow names it — a bare
            // `run_command` allow or `--yes` is not enough — and never when
            // its program is a known wrapper.
            let wrapper = COMMAND_WRAPPERS.contains(&deny_program(&argv[0]).as_str());
            let explicit = patterns
                .iter()
                .any(|p| p.starts_with(COMMAND_PATTERN_PREFIX) && pattern_allows(p, req));
            if wrapper || !explicit {
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
    ///
    /// A `run_command:` pattern here denies EVERY `run_command` call (#438):
    /// a deny list cannot name all the ways to reach a program (`env rm`,
    /// `sh -c …`), and this mode has no allow patterns that could name
    /// safe commands — the same fail-closed rule [`pattern_verdict`]
    /// applies to allow lists. Deny `run_command` by tool name to say so
    /// explicitly, or use an allow list with `run_command:` allows.
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
            let unprefixed =
                |p: &'_ String| -> String { p.strip_prefix(DENY_PREFIX).unwrap_or(p).to_string() };
            let matched = self
                .patterns
                .iter()
                .any(|p| pattern_matches(&unprefixed(p), req, true));
            // Fail closed (see `deny_only`): any command deny refuses every
            // run_command call.
            let command_deny = req.tool == "run_command"
                && self
                    .patterns
                    .iter()
                    .any(|p| unprefixed(p).starts_with(COMMAND_PATTERN_PREFIX));
            if matched || command_deny {
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
