//! `git` CLI tool (A2): light git operations — status, diff, add, commit.
//! `Risk::Write` for everything (conservative: even reads go through the
//! approver; per-op risk is a future refinement, see gh.rs note).
//!
//! Scope note: `status` and `diff` take no pathspec and run with cwd = the
//! workdir, so they cover the whole repository containing it (in a monorepo
//! subdirectory that includes sibling directories). By design; the jail
//! (#247) applies to `add` pathspecs. See `docs/threat-model.md`.
//!
//! Same argv-whitelist discipline as `gh.rs`: an op enum maps to a fixed
//! argv shape, every free-form value is validated (no leading dash, sane
//! charset), and anything unexpected is refused *before* spawning.
//! `push` and everything network/history-mutating is intentionally NOT
//! whitelisted — push stays a human action.
//!
//! This tool runs on the HOST, outside any command sandbox, and git executes
//! what the repository's config and hooks name. Hardening (#748, the `.git` of
//! a sandboxed command is read-only, so writes go through this tool):
//! - ALWAYS `-c core.fsmonitor=false` (performance only, no feature lost: a
//!   `core.fsmonitor` command in a repository config never runs here);
//! - when any command consumer is on (`[sandbox.consumers]`), also
//!   `-c core.hooksPath=<repo>/.git/hooks`, `GIT_CONFIG_NOSYSTEM=1`, and
//!   `--no-ext-diff --no-textconv` for `diff`; and the tool REFUSES every op
//!   unless a real `.git` directory enclosing the workdir was already there
//!   when the tool was constructed at start-up (a repository a sandboxed
//!   command created or replaced since is not trusted: its hooks, its
//!   `filter.*.clean` and `gpg.program` in a config it wrote would run on the
//!   host) and refuses a workdir that is itself a bare repository (git would
//!   use it instead of the enclosing repository). The pinned `.git/hooks` is
//!   read-only and owner-trusted only because of that snapshot (`.git` is
//!   read-only for a sandboxed command, #849). With no consumer on, hooks and
//!   the rest behave as they always did.
//!
//!   Residuals (owner choices, documented in the design note): the owner's own
//!   hooks run workspace content (lint, make, npm test, a pre-commit framework
//!   that runs files a sandboxed command may have modified); an owner config
//!   `include.path` that points at a workspace file lets that file define
//!   `filter.*.clean`, `gpg.program` or `core.sshCommand`, which then run on
//!   the host through add and commit; `GIT_CONFIG_NOSYSTEM=1` drops
//!   `/etc/gitconfig` (safe.directory, credential helpers, user.*, policy), so
//!   a repository owned by another user can fail with "dubious ownership".

use crate::subprocess::run_with_timeout;
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Git ops may trigger hooks (the owner's installed pre-commit review bots
/// are common on this host's global template, and still run when a command
/// consumer is on: see the module doc). LLM-backed hooks routinely exceed the
/// 30s generic subprocess ceiling, so git gets its own budget.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Allowed `git` subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOp {
    Status,
    Diff,
    Add,
    Commit,
}

impl GitOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "status" => Some(GitOp::Status),
            "diff" => Some(GitOp::Diff),
            "add" => Some(GitOp::Add),
            "commit" => Some(GitOp::Commit),
            _ => None,
        }
    }

    /// Build the argv after `git`. Free-form values validated:
    /// - paths: non-empty, no leading dash (a path arg like `--exec`
    ///   could redirect git behavior)
    /// - message: non-empty, no leading dash (git reads it as a value,
    ///   but defense in depth)
    fn argv(&self, input: &Value) -> Result<Vec<String>, String> {
        fn opt_str<'a>(input: &'a Value, k: &str) -> Option<&'a str> {
            input
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        }
        fn no_dash(k: &str, v: &str) -> Result<String, String> {
            if v.starts_with('-') {
                return Err(format!("git: '{k}' must not start with '-': {v:?}"));
            }
            Ok(v.to_string())
        }
        // Status/Diff: no pathspec by design — repo-wide, see module docs.
        match self {
            GitOp::Status => Ok(vec!["status".into(), "--short".into(), "--branch".into()]),
            GitOp::Diff => {
                // --staged only when explicitly asked; default = worktree diff.
                let mut argv = vec!["diff".to_string()];
                if input.get("staged").and_then(Value::as_bool) == Some(true) {
                    argv.push("--staged".into());
                }
                Ok(argv)
            }
            GitOp::Add => {
                // Paths must stay inside the workdir subtree: no absolute
                // paths, no `..` components (git would happily stage them
                // when the repository root sits above the workdir, e.g. a
                // subdirectory of a monorepo). This is the jail the file
                // tools enforce via canonicalize; for add, lexical
                // rejection is airtight because git resolves the path
                // itself afterwards.
                let mut paths = Vec::new();
                let Some(arr) = input.get("paths").and_then(Value::as_array) else {
                    return Err("git: 'paths' array is required for add".into());
                };
                if arr.is_empty() {
                    return Err("git: 'paths' must not be empty".into());
                }
                for p in arr {
                    let Some(p) = p.as_str().map(str::trim).filter(|v| !v.is_empty()) else {
                        return Err("git: every path must be a non-empty string".into());
                    };
                    if p.starts_with('/') || p.starts_with('\\') {
                        return Err(format!(
                            "git: 'path' must be relative to the workspace: {p:?}"
                        ));
                    }
                    // Issue #247 (rescan #99): a pathspec starting with
                    // ':' is git pathspec MAGIC — ':/' or ':(top)'
                    // anchors at the repository ROOT, escaping the
                    // workdir jail the same way an absolute path would.
                    if p.starts_with(':') {
                        return Err(format!(
                            "git: 'path' must be relative to the workspace (pathspec magic ':' is not allowed): {p:?}"
                        ));
                    }
                    // Windows drive-letter absolute paths (`C:\...`,
                    // `C:/...`) slip past the separator checks above
                    // (cora full-scan #30) — the second byte `:` marks a
                    // drive. The lexical jail must be host-OS-honest:
                    // tole mobile targets Android, but a workspace synced
                    // to/from a Windows host must not become an escape.
                    if p.len() > 1 && p.as_bytes()[1] == b':' {
                        return Err(format!(
                            "git: 'path' must be relative to the workspace (drive-letter path): {p:?}"
                        ));
                    }
                    if p.split(['/', '\\']).any(|c| c == "..") {
                        return Err(format!(
                            "git: 'path' must not escape the workspace (..): {p:?}"
                        ));
                    }
                    paths.push(no_dash("path", p)?);
                }
                // The one sanctioned catch-all: the whole tree, same as
                // a human typing `git add .` in the repo root.
                if paths.len() == 1 && paths[0] == "." {
                    return Ok(vec!["add".into(), ".".into()]);
                }
                let mut argv = vec!["add".to_string(), "--".into()];
                argv.extend(paths);
                Ok(argv)
            }
            GitOp::Commit => {
                let msg = opt_str(input, "message")
                    .ok_or_else(|| "git: missing or empty 'message'".to_string())?;
                if msg.len() > 500 {
                    return Err("git: 'message' too long (max 500 chars)".into());
                }
                let mut argv = vec!["commit".to_string(), "--".into()];
                // `--` then a `-m` would be parsed as a path. Instead we
                // validate and pass the message after -m BEFORE any path
                // separator — rebuild: commit -m MSG [-- paths...]
                argv.clear();
                argv.push("commit".into());
                argv.push("-m".into());
                argv.push(no_dash("message", msg)?);
                Ok(argv)
            }
        }
    }
}

/// Whether any command consumer (`run_command`, `job_start`, verify, stop
/// gates, and any added later) runs in the sandbox, which is when this tool
/// hardens its invocation beyond `core.fsmonitor`.
fn consumers_on() -> bool {
    use crate::sandbox::command::{consumers, Consumer};
    let c = consumers();
    Consumer::ALL
        .iter()
        .any(|k| c.mode(*k) != crate::sandbox::Mode::Off)
}

/// The `.git` directory of the nearest enclosing repository when it is a
/// REAL directory (not a file or a link: a link or a gitdir file could lead
/// anywhere), else `None`. Its `hooks` is the directory pinned when hardened.
fn real_git_dir(workdir: Option<&Path>) -> Option<PathBuf> {
    let start = match workdir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    let start = std::fs::canonicalize(start).ok()?;
    for dir in start.ancestors() {
        let git = dir.join(".git");
        match std::fs::symlink_metadata(&git) {
            Ok(m) if m.is_dir() => return Some(git),
            Ok(_) => return None,
            Err(_) => continue,
        }
    }
    None
}

/// The full argv after the program: the global `-c` options, then the op's
/// own argv. `hardened` is [`consumers_on`]; `hooks_dir` the pinned hooks
/// directory (`None`: `/dev/null`).
pub(crate) fn invocation(
    op: GitOp,
    argv: Vec<String>,
    hardened: bool,
    hooks_dir: Option<&Path>,
) -> Vec<String> {
    let mut out: Vec<String> = vec!["-c".into(), "core.fsmonitor=false".into()];
    if hardened {
        let hooks = hooks_dir.map_or_else(|| "/dev/null".to_string(), |d| d.display().to_string());
        out.push("-c".into());
        out.push(format!("core.hooksPath={hooks}"));
    }
    let mut rest = argv.into_iter();
    if let Some(sub) = rest.next() {
        out.push(sub);
        if hardened && op == GitOp::Diff {
            out.push("--no-ext-diff".into());
            out.push("--no-textconv".into());
        }
    }
    out.extend(rest);
    out
}

/// Whether `dir` looks like a bare repository (`HEAD`, `objects/`, `refs/`) with
/// no `.git` of its own: git run there uses it as the repository, not the
/// enclosing one.
fn looks_bare(dir: &Path) -> bool {
    dir.join("HEAD").is_file()
        && dir.join("objects").is_dir()
        && dir.join("refs").is_dir()
        && std::fs::symlink_metadata(dir.join(".git")).is_err()
}

/// Light git tool. cwd = the repo checkout (jail root for paths is the
/// same cwd the file tools use).
#[derive(Debug, Clone)]
pub struct GitTool {
    pub bin: PathBuf,
    pub workdir: Option<PathBuf>,
    /// The real `.git` directory that enclosed the workdir when the tool was
    /// built (at start-up, before any sandboxed command ran): the only
    /// repository trusted when a command consumer is on (see the module doc).
    startup_git: Option<PathBuf>,
}

impl GitTool {
    pub fn new() -> Self {
        Self {
            bin: PathBuf::from("git"),
            workdir: None,
            startup_git: real_git_dir(None),
        }
    }

    /// The `.git` directory to trust for this call when a command consumer is
    /// on, or the reason to refuse: the workdir is a bare repository, no
    /// repository existed at start-up, or it is not the one found now (swapped
    /// for a link, a file or another directory).
    fn trusted_git_dir(&self) -> Result<PathBuf, String> {
        let wd = self.workdir.as_deref();
        if let Some(d) = wd {
            if looks_bare(d) {
                return Err(format!(
                    "git: refused: {} looks like a bare repository (HEAD, objects, refs) with no \
                     .git, and git would use it instead of the enclosing repository; a command \
                     sandbox is on, so the git tool does not run in it",
                    d.display()
                ));
            }
        }
        match (&self.startup_git, real_git_dir(wd)) {
            (Some(at_start), Some(now)) if *at_start == now => Ok(now),
            (Some(_), _) => Err(
                "git: refused: the .git directory of this workspace is not the one that was \
                 there when the session started (it was replaced, or is no longer a real \
                 directory); a command sandbox is on, so the git tool does not run in it"
                    .into(),
            ),
            (None, _) => Err(
                "git: refused: no git repository was present when the session started, and a \
                 command sandbox is on: a repository a sandboxed command created could carry \
                 hooks, filters and config that would run on the host through this tool. Create \
                 the repository yourself, outside the sandbox, and restart the session"
                    .into(),
            ),
        }
    }

    pub fn with_bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.bin = bin.into();
        self
    }

    /// Sets the workdir and takes the start-up snapshot of its repository
    /// (call this while building the tool, before any command runs).
    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.startup_git = real_git_dir(Some(&dir));
        self.workdir = Some(dir);
        self
    }

    pub fn command_line(&self, input: &Value) -> Result<String, String> {
        let op = parse_op(input)?;
        let argv = op.argv(input)?;
        let mut parts = vec![self.bin.display().to_string()];
        parts.extend(argv.iter().map(|a| shquote(a)));
        Ok(parts.join(" "))
    }
}

fn parse_op(input: &Value) -> Result<GitOp, String> {
    input
        .get("op")
        .and_then(Value::as_str)
        .and_then(GitOp::parse)
        .ok_or_else(|| "git: 'op' must be one of status | diff | add | commit".to_string())
}

impl Default for GitTool {
    git_default!();
}

impl Tool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn summary(&self) -> String {
        "Light version control in the workspace: status, diff, add, commit. No push; a human pushes.".into()
    }

    fn describe(&self, input: &Value) -> String {
        self.command_line(input).unwrap_or_else(|e| e)
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["status", "diff", "add", "commit"],
                    "description": "Light git operation. push is NOT available — a human pushes."
                },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Paths for add (relative to repo root; use [\".\"] for all changes)" },
                "message": { "type": "string", "description": "Commit message (required for commit, max 500 chars)" },
                "staged": { "type": "boolean", "description": "diff --staged when true (default false)" }
            },
            "required": ["op"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let op = parse_op(&input)?;
        let argv = op.argv(&input)?;
        // #282: reject NUL up front; otherwise spawn fails with a
        // misleading "is it on PATH?" error.
        if argv.iter().any(|a| a.contains('\0')) {
            return Err("git: arguments must not contain NUL bytes".into());
        }
        let hardened = consumers_on();
        let hooks_dir = if hardened {
            Some(self.trusted_git_dir()?.join("hooks"))
        } else {
            None
        };
        let mut cmd = Command::new(&self.bin);
        crate::subprocess::scrub_env_for(&mut cmd, "git");
        if hardened {
            // No system-wide gitconfig: its includes and hooks are not the owner's.
            cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        }
        cmd.args(invocation(op, argv, hardened, hooks_dir.as_deref()));
        if let Some(dir) = &self.workdir {
            cmd.current_dir(dir);
        }
        let out = run_with_timeout(&mut cmd, GIT_TIMEOUT)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let snippet: String = stderr.chars().take(500).collect();
            return Err(format!("git exited {}: {}", out.status, snippet));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        // A diff never shows the lines of a sensitive file (#598 part b).
        let stdout = crate::sensitive::redact_diff(&stdout);
        let (body, truncated) = cap_stdout(stdout.trim());
        Ok(json!({
            "stdout": body,
            "truncated": truncated,
        }))
    }
}

/// Stdout cap (issue #230): a `git diff` on a large working set can
/// produce megabytes — that floods the model context. Same policy as
/// the web tools' 20 000-char result cap: keep the head, flag the cut.
const STDOUT_CAP_CHARS: usize = 20_000;

fn cap_stdout(s: &str) -> (String, bool) {
    let count = s.chars().count();
    if count <= STDOUT_CAP_CHARS {
        return (s.to_string(), false);
    }
    let mut out: String = s.chars().take(STDOUT_CAP_CHARS).collect();
    out.push('…');
    (out, true)
}

/// Minimal shlex-style quoting for the audit string (same rules as gh.rs).
fn shquote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// The macro trick avoids a Default impl that silently diverges from new().
macro_rules! git_default {
    () => {
        fn default() -> Self {
            Self::new()
        }
    };
}
use git_default;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classified_write() {
        assert_eq!(GitTool::new().risk(), Risk::Write);
    }

    #[test]
    fn add_jail_rejects_absolute_and_drive_letter_paths() {
        let t = GitTool::new();
        // POSIX absolute + backslash root + Windows drive letters (cora
        // full-scan #30: `C:\evil` passed both separator checks).
        for bad in [
            "/etc/passwd",
            "\\Windows\\evil",
            "C:\\Windows\\evil",
            "C:/Users/evil",
        ] {
            let err = t
                .command_line(&json!({"op":"add","paths":[bad]}))
                .unwrap_err();
            assert!(
                err.contains("relative to the workspace"),
                "{bad}: got {err}"
            );
        }
        // Plain relative paths still pass.
        assert!(t
            .command_line(&json!({"op":"add","paths":["src/main.rs"]}))
            .is_ok());
        // A filename with a colon NOT at position 1 stays legal.
        assert!(t
            .command_line(&json!({"op":"add","paths":["weird:name.txt"]}))
            .is_ok());
        // Issue #247: pathspec magic cannot anchor at the repo root.
        for bad in [":/", ":(top)", ":(top,magic)src/x"] {
            let err = t
                .command_line(&json!({"op":"add","paths":[bad]}))
                .unwrap_err();
            assert!(err.contains("pathspec magic"), "{bad}: got {err}");
        }
    }

    #[test]
    fn status_argv() {
        let t = GitTool::new();
        assert_eq!(
            t.command_line(&json!({"op":"status"})).unwrap(),
            "git status --short --branch"
        );
    }

    #[test]
    fn diff_argv() {
        let t = GitTool::new();
        assert_eq!(t.command_line(&json!({"op":"diff"})).unwrap(), "git diff");
        assert_eq!(
            t.command_line(&json!({"op":"diff","staged":true})).unwrap(),
            "git diff --staged"
        );
    }

    #[test]
    fn add_argv() {
        let t = GitTool::new();
        assert_eq!(
            t.command_line(&json!({"op":"add","paths":["."]})).unwrap(),
            "git add ."
        );
        assert_eq!(
            t.command_line(&json!({"op":"add","paths":["a.txt","src/b.rs"]}))
                .unwrap(),
            "git add -- a.txt src/b.rs"
        );
    }

    #[test]
    fn commit_argv() {
        let t = GitTool::new();
        assert_eq!(
            t.command_line(&json!({"op":"commit","message":"feat: x"}))
                .unwrap(),
            "git commit -m 'feat: x'"
        );
    }

    #[test]
    fn rejects_injection() {
        let t = GitTool::new();
        // unknown op
        assert!(t.command_line(&json!({"op":"push"})).is_err());
        assert!(t.command_line(&json!({"op":"remote"})).is_err());
        // leading-dash path
        assert!(t
            .command_line(&json!({"op":"add","paths":["--exec=/bin/sh"]}))
            .is_err());
        // jail escapes: absolute + parent traversal
        assert!(t
            .command_line(&json!({"op":"add","paths":["/etc/passwd"]}))
            .is_err());
        assert!(t
            .command_line(&json!({"op":"add","paths":["../sibling/file"]}))
            .is_err());
        assert!(t
            .command_line(&json!({"op":"add","paths":["safe/../../escape"]}))
            .is_err());
        // interior .. still caught after normalization by component check
        assert!(t
            .command_line(&json!({"op":"add","paths":["a/../b"]}))
            .is_err());
        // leading-dash message
        assert!(t
            .command_line(&json!({"op":"commit","message":"--amend"}))
            .is_err());
        // missing fields
        assert!(t.command_line(&json!({"op":"add"})).is_err());
        assert!(t.command_line(&json!({"op":"commit"})).is_err());
        // empty paths
        assert!(t.command_line(&json!({"op":"add","paths":[]})).is_err());
        // overlong message
        assert!(t
            .command_line(&json!({"op":"commit","message":"x".repeat(501)}))
            .is_err());
    }

    /// #282: a NUL byte in a model-supplied message/path must surface as a
    /// clear tool error (never a panic, never a misleading spawn error).
    #[test]
    fn nul_byte_in_argv_is_a_clear_error_not_a_panic() {
        let t = GitTool::new();
        for input in [
            json!({"op":"commit","message":"a\u{0}b"}),
            json!({"op":"add","paths":["a\u{0}b"]}),
        ] {
            let err = t.execute(input).unwrap_err();
            assert!(err.contains("NUL"), "unexpected error: {err}");
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// #748: only `core.fsmonitor=false` without a command consumer, the
    /// pinned hooks directory, `/dev/null` and the diff options with one.
    #[test]
    fn the_invocation_is_hardened_only_when_a_command_consumer_is_on() {
        let commit = s(&["commit", "-m", "x"]);
        assert_eq!(
            invocation(GitOp::Commit, commit.clone(), false, None),
            s(&["-c", "core.fsmonitor=false", "commit", "-m", "x"])
        );
        // The hardening never touches the op's own arguments.
        let hooks = Path::new("/ws/.git/hooks");
        assert_eq!(
            invocation(GitOp::Commit, commit.clone(), true, Some(hooks)),
            s(&[
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/ws/.git/hooks",
                "commit",
                "-m",
                "x"
            ])
        );
        // No real `.git` directory: hooks off.
        assert_eq!(
            invocation(GitOp::Commit, commit, true, None)[3],
            "core.hooksPath=/dev/null"
        );
        // `diff` also gets the driver options, `--staged` stays after them.
        assert_eq!(
            invocation(GitOp::Diff, s(&["diff", "--staged"]), true, Some(hooks)),
            s(&[
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/ws/.git/hooks",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--staged"
            ])
        );
        assert_eq!(
            invocation(GitOp::Diff, s(&["diff"]), false, None),
            s(&["-c", "core.fsmonitor=false", "diff"])
        );
        // The other ops are not diffs.
        assert!(
            !invocation(GitOp::Status, s(&["status", "--short"]), true, None)
                .contains(&"--no-ext-diff".to_string())
        );
    }

    /// The trusted directory is the REAL `.git` directory found from the
    /// workdir upward; a gitdir file, a link or no repository gives none.
    #[cfg(unix)]
    #[test]
    fn the_trusted_git_directory_needs_a_real_git_directory() {
        let d = crate::test_dir::TestDir::new("tole-git-pin");
        let root = std::fs::canonicalize(&*d).unwrap();
        // A monorepo subdirectory finds the enclosing repository.
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("repo/sub/deeper")).unwrap();
        assert_eq!(
            real_git_dir(Some(&root.join("repo/sub/deeper"))),
            Some(root.join("repo/.git"))
        );
        // A gitdir FILE (worktree, submodule): none.
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::write(root.join("wt/.git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(real_git_dir(Some(&root.join("wt"))), None);
        // A symbolic link: none.
        std::fs::create_dir_all(root.join("lk")).unwrap();
        std::os::unix::fs::symlink(root.join("repo/.git"), root.join("lk/.git")).unwrap();
        assert_eq!(real_git_dir(Some(&root.join("lk"))), None);
    }

    /// A directory with HEAD, objects and refs and no `.git` is a bare repository.
    #[test]
    fn a_bare_repository_is_recognised_only_without_a_git_entry() {
        let d = crate::test_dir::TestDir::new("tole-git-bare");
        assert!(!looks_bare(&d));
        std::fs::write(d.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(d.join("objects")).unwrap();
        std::fs::create_dir_all(d.join("refs")).unwrap();
        assert!(looks_bare(&d));
        std::fs::create_dir_all(d.join(".git")).unwrap();
        assert!(!looks_bare(&d), "a .git of its own makes it a work tree");
    }

    #[test]
    fn fake_git_binary_end_to_end() {
        let dir = crate::test_dir::TestDir::new("tole-git");
        let bin = dir.join("git");
        let log = dir.join("argv.log");
        crate::test_dir::write_executable(
            &bin,
            format!("#!/bin/sh\necho \"$@\" > {}\necho ok\n", log.display()),
        );
        let t = GitTool::new().with_bin(&bin);
        let out = t
            .execute(json!({"op":"commit","message":"feat: x"}))
            .unwrap();
        assert_eq!(out["stdout"], json!("ok"));
        let recorded = std::fs::read_to_string(&log).unwrap();
        // Only `core.fsmonitor=false` is added when no command consumer is on.
        assert_eq!(recorded.trim(), "-c core.fsmonitor=false commit -m feat: x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #230 regression: oversized git stdout is capped and flagged
    /// instead of flooding the model context.
    #[test]
    fn stdout_over_cap_is_truncated_with_flag() {
        let big = "x".repeat(crate::git::STDOUT_CAP_CHARS + 123);
        let (body, truncated) = cap_stdout(&big);
        assert!(truncated);
        assert_eq!(body.chars().count(), crate::git::STDOUT_CAP_CHARS + 1); // + ellipsis
        assert!(body.ends_with('…'));

        let small = "short output";
        let (body, truncated) = cap_stdout(small);
        assert!(!truncated);
        assert_eq!(body, small);
    }

    #[test]
    fn fake_git_binary_truncates_large_output_end_to_end() {
        let dir = crate::test_dir::TestDir::new("tole-git-cap");
        let bin = dir.join("git");
        crate::test_dir::write_executable(&bin, "#!/bin/sh\nprintf 'y%.0s' $(seq 1 25000)\n");
        let t = GitTool::new().with_bin(&bin);
        let out = t.execute(json!({"op":"status"})).unwrap();
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(
            out["stdout"].as_str().unwrap().chars().count(),
            crate::git::STDOUT_CAP_CHARS + 1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #598 part b: `git diff` never shows the lines of a sensitive file.
    #[cfg(unix)]
    #[test]
    fn a_diff_hides_the_lines_of_a_sensitive_file() {
        let dir = crate::test_dir::TestDir::new("tole-git-env");
        let bin = dir.join("git");
        crate::test_dir::write_executable(
            &bin,
            "#!/bin/sh\nprintf 'diff --git a/.env b/.env\\n--- a/.env\\n+++ b/.env\\n\
             @@ -1 +1 @@\\n-TOKEN=old-secret-1\\n+TOKEN=new-secret-2\\n'\n",
        );
        let out = GitTool::new()
            .with_bin(&bin)
            .execute(json!({"op":"diff"}))
            .unwrap();
        let s = out["stdout"].as_str().unwrap();
        assert!(!s.contains("secret"), "{s}");
        assert!(s.contains("+«redacted»"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
