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
//! what the repository's config, hooks and attributes name. Hardening (#748:
//! the `.git` of a sandboxed command is read-only, so writes go through this
//! tool). The principle: with any command consumer on (`[sandbox.consumers]`),
//! NO host-side code execution is reachable from state a sandboxed command can
//! write, whenever and by whomever the repository was created (no trust in a
//! repository's identity or age).
//! - ALWAYS `-c core.fsmonitor=false` (performance only, no feature lost);
//! - with a consumer on:
//!   - `-c core.hooksPath=/dev/null` and `--no-verify` on commit: NO hook runs
//!     for a commit made through this tool, the owner's own pre-commit and
//!     commit-msg included (a human's own `git commit` still runs them);
//!   - `GIT_CONFIG_NOSYSTEM=1`, `--no-ext-diff --no-textconv` on diff, and
//!     `--ignore-submodules=all` on status and diff (so git never descends into
//!     a nested repository and its unvetted config);
//!   - before every op the repository is VETTED: git itself says which git
//!     directory it uses (`rev-parse`, same hardened options) and the tool
//!     refuses unless that is a plain `.git` directory at the work tree root
//!     (not bare, not a linked worktree or submodule gitdir file, no separate
//!     git dir); then that directory's `config` is read WITHOUT running anything
//!     (`git config --file`, no includes followed) and the tool refuses, naming
//!     the key and the way out, if it holds any key outside a small allowlist of
//!     inert ones (see [`inert_key`]): everything that can name a program or a
//!     file to run or include (`filter.*`, `core.fsmonitor`, `core.hooksPath`,
//!     `core.sshCommand`, `gpg.*`, `include.*`, `alias.*`, `core.worktree`,
//!     `extensions.*`, ...) is refused. This holds for a repository a sandboxed
//!     command created, replaced or rewrote at any time. `add` is also refused
//!     while the index holds a submodule entry (git would run git inside the
//!     nested repository, whose config nothing vets, and no option stops that).
//!
//!   With no consumer on, hooks and config behave as they always did.
//!
//!   Residuals (documented in the design note): a still-running sandboxed
//!   process (a `job_start` from an earlier turn, a long command) that writes
//!   the config between the vet and the spawn can get that SAME call to execute
//!   it (the spawned git re-reads the config after the vet; later calls
//!   refuse); the window is narrow and there is no cheap fix for wildcard keys.
//!   A sandbox-written `.gitattributes` or `.git/info/attributes` with
//!   `* filter=lfs` triggers the OWNER's global `filter.lfs.*` command (the
//!   program is the owner's, its input is attacker-influenced); `gc.autodetach`
//!   can leave a detached `git maintenance run --auto` that outlives the vet.
//!   An owner (global) config `include.path` that points at a workspace file is
//!   the owner's own choice and is not vetted; `GIT_CONFIG_NOSYSTEM=1` drops
//!   `/etc/gitconfig` (safe.directory, credential helpers, user.*), so a
//!   repository owned by another user can fail with "dubious ownership".

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
/// gates, cron scripts and deliveries, and any added later) runs in the sandbox,
/// which is when this tool hardens and vets (see the module doc).
fn consumers_on() -> bool {
    use crate::sandbox::command::{consumers, Consumer};
    let c = consumers();
    Consumer::ALL
        .iter()
        .any(|k| c.mode(*k) != crate::sandbox::Mode::Off)
}

/// The full argv after the program: the global `-c` options, then the op's
/// own argv. `hardened` is [`consumers_on`].
pub(crate) fn invocation(op: GitOp, argv: Vec<String>, hardened: bool) -> Vec<String> {
    let mut out: Vec<String> = vec!["-c".into(), "core.fsmonitor=false".into()];
    if hardened {
        out.push("-c".into());
        out.push("core.hooksPath=/dev/null".into());
    }
    let mut rest = argv.into_iter();
    if let Some(sub) = rest.next() {
        out.push(sub);
        if hardened {
            match op {
                GitOp::Diff => {
                    out.push("--no-ext-diff".into());
                    out.push("--no-textconv".into());
                    out.push("--ignore-submodules=all".into());
                }
                GitOp::Status => out.push("--ignore-submodules=all".into()),
                GitOp::Commit => out.push("--no-verify".into()),
                GitOp::Add => {}
            }
        }
    }
    out.extend(rest);
    out
}

/// Whether a key of the repository's own `config` is inert: it cannot name a
/// program or a file for git to run or include. An ALLOWLIST (anything unknown
/// is refused): git keeps adding keys that name commands. Keys are compared
/// case-insensitively (a subsection is case-sensitive in git; folding it only
/// makes this stricter-or-equal for the names allowed here).
pub(crate) fn inert_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let (sub, var) = match rest.rsplit_once('.') {
        Some((sub, var)) => (Some(sub), var),
        None => (None, rest),
    };
    match (section, sub, var) {
        (
            "core",
            None,
            "repositoryformatversion"
            | "filemode"
            | "bare"
            | "logallrefupdates"
            | "ignorecase"
            | "precomposeunicode"
            | "symlinks"
            | "autocrlf"
            | "eol"
            | "safecrlf"
            | "quotepath",
        ) => true,
        ("user", None, "name" | "email") => true,
        ("init", None, "defaultbranch") => true,
        ("pull", None, "rebase" | "ff") => true,
        ("push", None, "default" | "autosetupremote") => true,
        ("fetch", None, "prune") => true,
        ("merge", None, "ff") => true,
        ("color", None, "ui") => true,
        ("status", None, "showuntrackedfiles") => true,
        ("diff", None, "renames" | "algorithm" | "context") => true,
        // Only used by fetch and push, which this tool never runs.
        ("remote", Some(_), "url" | "fetch" | "pushurl") => true,
        ("branch", Some(_), "remote" | "merge") => true,
        _ => false,
    }
}

/// Light git tool. cwd = the repo checkout (jail root for paths is the
/// same cwd the file tools use).
#[derive(Debug, Clone)]
pub struct GitTool {
    pub bin: PathBuf,
    pub workdir: Option<PathBuf>,
    /// Extra environment for every git process the tool spawns (embedders,
    /// tests): applied after the scrub, never read from the process.
    env: Vec<(String, String)>,
}

impl GitTool {
    pub fn new() -> Self {
        Self {
            bin: PathBuf::from("git"),
            workdir: None,
            env: Vec::new(),
        }
    }

    pub fn with_bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.bin = bin.into();
        self
    }

    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(dir.into());
        self
    }

    /// Adds an environment variable to every git process (for example `HOME`,
    /// `GIT_CONFIG_GLOBAL`), after the scrub.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// A git process with the scrubbed environment and the workdir; `hardened`
    /// adds `GIT_CONFIG_NOSYSTEM=1` (no system-wide gitconfig: its includes and
    /// hooks are not the owner's).
    fn command(&self, hardened: bool) -> Command {
        let mut cmd = Command::new(&self.bin);
        crate::subprocess::scrub_env_for(&mut cmd, "git");
        for k in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_CEILING_DIRECTORIES",
        ] {
            cmd.env_remove(k);
        }
        if hardened {
            cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(dir) = &self.workdir {
            cmd.current_dir(dir);
        }
        cmd
    }

    /// Runs a read-only probe (`rev-parse`, `config --file`) with the hardened
    /// options and returns its stdout.
    fn probe(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = self.command(true);
        cmd.args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ]);
        cmd.args(args);
        let out = run_with_timeout(&mut cmd, GIT_TIMEOUT)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let snippet: String = stderr.chars().take(300).collect();
            return Err(format!(
                "git: refused: could not vet the repository ({}): {}",
                out.status,
                snippet.trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// With a command consumer on: ask git which git directory it really uses,
    /// require a plain `.git` directory at the work tree root, and refuse any
    /// key in its config outside [`inert_key`] (see the module doc).
    fn vet_repository(&self, op: GitOp) -> Result<(), String> {
        const WAY_OUT: &str = "a command sandbox is on and a sandboxed command can write \
             repository state; run git yourself outside the tool, or remove the key";
        let layout = |why: String| {
            format!(
                "git: refused: {why}; the git tool only runs in a plain repository (a `.git` \
                 directory at the work tree root: not bare, not a linked worktree, a submodule \
                 or a separate git dir) because it cannot vet any other layout while a command \
                 sandbox is on. Run git yourself outside the tool"
            )
        };
        let first = self.probe(&["rev-parse", "--is-bare-repository", "--absolute-git-dir"])?;
        let mut lines = first.lines();
        if lines.next().map(str::trim) != Some("false") {
            return Err(layout("the repository git uses is bare".into()));
        }
        let gitdir = PathBuf::from(lines.next().unwrap_or("").trim());
        let second = self.probe(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--show-toplevel",
        ])?;
        let mut lines = second.lines();
        let common = PathBuf::from(lines.next().unwrap_or("").trim());
        let top = PathBuf::from(lines.next().unwrap_or("").trim());
        let canon = |p: &Path| std::fs::canonicalize(p).ok();
        let plain = std::fs::symlink_metadata(top.join(".git")).is_ok_and(|m| m.is_dir())
            && canon(&gitdir).is_some()
            && canon(&gitdir) == canon(&top.join(".git"))
            && canon(&common) == canon(&gitdir);
        if !plain {
            return Err(layout(format!(
                "git uses {} for the work tree {}",
                gitdir.display(),
                top.display()
            )));
        }
        let config = gitdir.join("config");
        let listing = self.probe(&[
            "config",
            "--file",
            &config.display().to_string(),
            "--list",
            "-z",
        ])?;
        for entry in listing.split('\0').filter(|e| !e.is_empty()) {
            let key = entry.split('\n').next().unwrap_or(entry);
            if !inert_key(key) {
                return Err(format!(
                    "git: refused: {} sets `{key}`, which can name a program or a file for git to \
                     run on the host; {WAY_OUT}",
                    config.display()
                ));
            }
        }
        // `git add` runs git INSIDE a gitlink (submodule) that is dirty, and no
        // option stops it: the nested repository's own config, which nothing
        // vets, would run (`filter.*.clean`). The index is in the `.git` a
        // sandboxed command may have written, so a gitlink there is not the
        // owner's. Status, diff and commit do not descend (`--ignore-submodules`).
        if op == GitOp::Add {
            let staged = self.probe(&["ls-files", "--stage", "-z"])?;
            if let Some(entry) = staged.split('\0').find(|e| e.starts_with("160000 ")) {
                let path = entry.split('\t').nth(1).unwrap_or("?");
                return Err(format!(
                    "git: refused: the index holds a submodule entry (`{path}`); `git add` would \
                     run git inside it, with a config nothing vets, while a command sandbox is on. \
                     Run `git add` yourself outside the tool"
                ));
            }
        }
        Ok(())
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
        if hardened {
            self.vet_repository(op)?;
        }
        let mut cmd = self.command(hardened);
        cmd.args(invocation(op, argv, hardened));
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

    /// #748: only `core.fsmonitor=false` without a command consumer; hooks
    /// off, `--no-verify` and the submodule/diff options with one.
    #[test]
    fn the_invocation_is_hardened_only_when_a_command_consumer_is_on() {
        let commit = s(&["commit", "-m", "x"]);
        assert_eq!(
            invocation(GitOp::Commit, commit.clone(), false),
            s(&["-c", "core.fsmonitor=false", "commit", "-m", "x"])
        );
        assert_eq!(
            invocation(GitOp::Commit, commit, true),
            s(&[
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--no-verify",
                "-m",
                "x"
            ])
        );
        assert_eq!(
            invocation(GitOp::Diff, s(&["diff", "--staged"]), true),
            s(&[
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=all",
                "--staged"
            ])
        );
        assert_eq!(
            invocation(GitOp::Diff, s(&["diff"]), false),
            s(&["-c", "core.fsmonitor=false", "diff"])
        );
        let status = invocation(GitOp::Status, s(&["status", "--short"]), true);
        assert!(status.contains(&"--ignore-submodules=all".to_string()));
        assert!(!status.contains(&"--no-ext-diff".to_string()));
        assert_eq!(
            invocation(GitOp::Add, s(&["add", "."]), true)[4..],
            s(&["add", "."])
        );
    }

    /// The allowlist admits what `git init`, `git clone` and a user name hold
    /// and refuses every key that can name a program or a file.
    #[test]
    fn only_inert_config_keys_are_allowed() {
        for ok in [
            "core.repositoryformatversion",
            "core.filemode",
            "core.bare",
            "core.logallrefupdates",
            "Core.IgnoreCase",
            "user.name",
            "user.email",
            "remote.origin.url",
            "remote.origin.fetch",
            "branch.main.remote",
            "branch.feature/x.merge",
            "init.defaultBranch",
        ] {
            assert!(inert_key(ok), "{ok} should be inert");
        }
        for bad in [
            "core.fsmonitor",
            "core.hookspath",
            "core.sshcommand",
            "core.pager",
            "core.editor",
            "core.askpass",
            "core.gitproxy",
            "core.worktree",
            "core.attributesfile",
            "core.excludesfile",
            "gpg.program",
            "gpg.format",
            "commit.gpgsign",
            "tag.gpgsign",
            "user.signingkey",
            "credential.helper",
            "filter.lfs.clean",
            "diff.x.textconv",
            "diff.external",
            "diff.tool",
            "merge.x.driver",
            "mergetool.x.cmd",
            "alias.st",
            "include.path",
            "includeif.gitdir:/x.path",
            "submodule.x.update",
            "extensions.worktreeconfig",
            "remote.origin.vcs",
            "remote.origin.uploadpack",
            "remote.origin.receivepack",
            "url.x.insteadof",
            "sequence.editor",
            "web.browser",
            "trailer.x.cmd",
            "protocol.ext.allow",
            "pager.diff",
            "interactive.difffilter",
            "sendemail.smtpserver",
            "lfs.url",
            "core",
            "",
        ] {
            assert!(!inert_key(bad), "{bad} must be refused");
        }
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
