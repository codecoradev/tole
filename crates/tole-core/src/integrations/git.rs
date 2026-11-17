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
//!   - the git process gets an ALLOWLIST environment (`hardened_env`), not the
//!     tole process's: locale, `TZ`, `TERM`, `HOME` from the account's passwd
//!     entry (never `$HOME`) and `GIT_TERMINAL_PROMPT=0`; `GIT_AUTHOR_*`,
//!     `GIT_COMMITTER_*`, `USER`, `LOGNAME` and `TMPDIR` are dropped (identity
//!     comes from config or passwd, git uses `/tmp`). The git binary and its
//!     `PATH` are FIXED (`trusted_launch`): `git` is looked up in
//!     `/usr/local/bin`, `/usr/bin`, `/bin` and `/opt/homebrew/bin` only,
//!     never in the inherited `PATH` (a sandboxed command that starts tole
//!     sets it) and never under the workdir, the temp dir or a home; that
//!     list is also the `PATH` for git's helpers; an embedder with git
//!     elsewhere uses `GitTool::with_bin` with an absolute path, the CLI
//!     has no setting (link git into a trusted directory). No descriptor
//!     above stderr is inherited. `GIT_CONFIG_GLOBAL|SYSTEM|COUNT|KEY_n|
//!     VALUE_n|PARAMETERS`, `XDG_CONFIG_HOME`, `GIT_EXEC_PATH`, `GIT_ATTR_SOURCE`,
//!     `GIT_PAGER`, `GIT_SSH*`, ... inherited from a tole started by a
//!     sandboxed command cannot reach git: the vet reads one file, so any
//!     other way to give git config would bypass it;
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
//!   A program the owner's own (global) config names is looked up in the fixed
//!   `PATH`; `HOME` comes from the passwd database, so a process run with
//!   another `$HOME` uses the account's global config (the sandbox plan refuses
//!   a grant that holds that home).
//!   An owner (global) config `include.path` that points at a workspace file is
//!   the owner's own choice and is not vetted; `GIT_CONFIG_NOSYSTEM=1` drops
//!   `/etc/gitconfig` (safe.directory, credential helpers, user.*), so a
//!   repository owned by another user can fail with "dubious ownership".

use crate::subprocess::run_with_timeout;
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::ffi::OsString;
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

/// The directories a hardened git process searches for programs, in order. FIXED:
/// the inherited `PATH` is never used, because a tole started by a sandboxed
/// command inherits whatever `PATH` that command set (a directory it can write
/// would then supply `git`, `sh`, `gpg`, `ssh` or any helper git spawns). An
/// owner whose git lives elsewhere gives the tool an absolute `bin`
/// ([`GitTool::with_bin`]; its directory is added) or links it into one of these.
const TRUSTED_DIRS: [&str; 4] = ["/usr/local/bin", "/usr/bin", "/bin", "/opt/homebrew/bin"];

/// The directories no fixed search directory may be under: the workdir, the
/// temp dir, `$HOME` and the account home (a sandboxed command can write
/// there), canonicalized. The filesystem root is never one of them, whichever
/// of them names it (`HOME=/`, a passwd home of `/` in a container, a workdir
/// of `/`): every path is under `/`, so it would empty the trusted set and
/// leave the tool unusable. A home of `/` is simply no usable home.
fn avoid_dirs(
    workdir: Option<PathBuf>,
    tmp: PathBuf,
    home: Option<PathBuf>,
    account_home: Option<PathBuf>,
) -> Vec<PathBuf> {
    [workdir, Some(tmp), home, account_home]
        .into_iter()
        .flatten()
        .map(|p| crate::paths::canon_lenient(&p))
        .filter(|p| p.parent().is_some())
        .collect()
}

/// The `PATH` directories of a hardened git: the directory of an absolute
/// `bin`, then each of [`TRUSTED_DIRS`] that exists and is not under `avoid`.
fn search_dirs(bin: &Path, avoid: &[PathBuf]) -> Vec<PathBuf> {
    let usable = |d: &Path| -> bool {
        d.is_absolute()
            && std::fs::canonicalize(d)
                .is_ok_and(|c| c.is_dir() && !avoid.iter().any(|a| c.starts_with(a)))
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if bin.is_absolute() {
        if let Some(parent) = bin.parent() {
            dirs.push(parent.to_path_buf());
        }
    }
    dirs.extend(TRUSTED_DIRS.iter().map(PathBuf::from).filter(|d| usable(d)));
    dirs
}

/// The environment a hardened git process gets: an ALLOWLIST taken from
/// `parent` (the process environment), plus `PATH` (the fixed `path`),
/// `HOME` (the account's own home, see [`crate::paths::account_home`]; the
/// process's `HOME` is NOT used), `GIT_CONFIG_NOSYSTEM` and
/// `GIT_TERMINAL_PROMPT=0`. A denylist cannot keep up: git(1) and
/// git-config(1) document dozens of variables that name a program, a config
/// file or a directory (`GIT_CONFIG_GLOBAL|SYSTEM|COUNT|KEY_n|VALUE_n|PARAMETERS`,
/// `XDG_CONFIG_HOME`, `GIT_EXEC_PATH`, `GIT_TEMPLATE_DIR`, `GIT_ATTR_SOURCE`,
/// `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`,
/// `GIT_REPLACE_REF_BASE`, `GIT_NAMESPACE`, `GIT_PROXY_COMMAND`, `GIT_SSH*`,
/// `GIT_ASKPASS`, `GIT_EDITOR`, `GIT_PAGER`, `GIT_EXTERNAL_DIFF`, and the
/// loader's `LD_PRELOAD` and `LD_LIBRARY_PATH`, ...), and a tole process
/// started by a sandboxed command inherits whatever that command set. Kept:
/// locale, time zone and `TERM` only. Dropped on purpose: `GIT_AUTHOR_*`,
/// `GIT_COMMITTER_*`, `USER` and `LOGNAME` (the identity comes from config or
/// the passwd entry only) and `TMPDIR`/`TEMP`/`TMP` (git falls back to `/tmp`).
pub(crate) fn hardened_env(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    home: Option<PathBuf>,
    path: OsString,
) -> Vec<(OsString, OsString)> {
    fn kept(name: &str) -> bool {
        matches!(name, "LANG" | "LANGUAGE" | "TZ" | "TERM") || name.starts_with("LC_")
    }
    let mut out: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(k, _)| k.to_str().is_some_and(kept))
        .collect();
    out.push(("PATH".into(), path));
    if let Some(home) = home {
        out.push(("HOME".into(), home.into_os_string()));
    }
    out.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    out.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    out
}

/// Whether `p` is a regular file with an execute bit.
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Marks every file descriptor above stderr close-on-exec in the child (Linux
/// 5.11+, `close_range` with `CLOSE_RANGE_CLOEXEC`; an older kernel leaves them
/// as they are), so git does not inherit a pipe or a socket this process was
/// handed by whatever started it. Stdin is `/dev/null` and stdout and stderr
/// are pipes (see `run_with_timeout`). The flag, not a close, keeps the
/// standard library's own exec-error pipe working.
#[cfg(target_os = "linux")]
fn close_inherited_fds(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only makes one raw syscall, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                !0 as libc::c_uint,
                CLOSE_RANGE_CLOEXEC,
            );
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn close_inherited_fds(_cmd: &mut Command) {}

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

    /// Sets the program git runs as. With a command consumer on it must be an
    /// absolute path, used as given (an embedder's choice); its directory is
    /// then also searched for the helpers git spawns.
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

    /// With a command consumer on: the git binary and the `PATH` it gets, both
    /// FIXED (see [`TRUSTED_DIRS`]). A directory under the workdir, the temp
    /// dir, `$HOME` or the account home is never searched (a sandboxed
    /// command can write there; a home of `/` counts as no home, see
    /// [`avoid_dirs`]), and a relative `bin` is refused.
    fn trusted_launch(&self) -> Result<(PathBuf, OsString), String> {
        let avoid = avoid_dirs(
            self.workdir
                .clone()
                .or_else(|| std::env::current_dir().ok()),
            std::env::temp_dir(),
            std::env::var_os("HOME").map(PathBuf::from),
            crate::paths::account_home(),
        );
        let dirs = search_dirs(&self.bin, &avoid);
        let program = if self.bin.is_absolute() {
            self.bin.clone()
        } else if self.bin.components().count() == 1 {
            dirs.iter()
                .map(|d| d.join(&self.bin))
                .find(|p| is_executable(p))
                .ok_or_else(|| {
                    format!(
                        "git: refused: no `{}` in the trusted directories ({}) and a command \
                         sandbox is on (the inherited PATH is not used: a sandboxed command can \
                         set it); install git there, link it into one of them, or give the tool \
                         an absolute `bin`",
                        self.bin.display(),
                        TRUSTED_DIRS.join(", ")
                    )
                })?
        } else {
            return Err(format!(
                "git: refused: the git program `{}` is a relative path and a command sandbox is on; \
                 use an absolute path",
                self.bin.display()
            ));
        };
        let path = std::env::join_paths(&dirs)
            .map_err(|e| format!("git: refused: cannot build a PATH for git: {e}"))?;
        Ok((program, path))
    }

    /// A git process with the workdir. Without a consumer (`hardened` false)
    /// the environment is the process's, scrubbed of secrets and of the git
    /// variables that relocate the repository. With one, the program and its
    /// `PATH` are fixed ([`Self::trusted_launch`]) and git gets an ALLOWLIST
    /// environment ([`hardened_env`]): nothing else in the process environment
    /// reaches it, so no variable git documents (the config-locating family
    /// `GIT_CONFIG_*`, `GIT_EXEC_PATH`, `GIT_ATTR_SOURCE`, `GIT_PAGER`,
    /// `GIT_SSH*`, ...) can redirect it to state a sandboxed command wrote, and
    /// no file descriptor beyond stdio is inherited.
    fn command(&self, hardened: bool) -> Result<Command, String> {
        let mut cmd;
        if hardened {
            let (program, path) = self.trusted_launch()?;
            cmd = Command::new(program);
            cmd.env_clear();
            for (k, v) in hardened_env(std::env::vars_os(), crate::paths::account_home(), path) {
                cmd.env(k, v);
            }
            close_inherited_fds(&mut cmd);
        } else {
            cmd = Command::new(&self.bin);
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
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(dir) = &self.workdir {
            cmd.current_dir(dir);
        }
        Ok(cmd)
    }

    /// Runs a read-only probe (`rev-parse`, `config --file`) with the hardened
    /// options and returns its stdout.
    fn probe(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = self.command(true)?;
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
        let mut cmd = self.command(hardened)?;
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

    /// A descriptor this process was handed (a pipe or socket from whatever started
    /// it) is not inherited by the hardened git process.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_hardened_git_process_inherits_no_extra_descriptor() {
        use std::os::fd::AsRawFd;
        let f = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: dup of an open descriptor; the copy has no close-on-exec flag.
        let fd = unsafe { libc::dup(f.as_raw_fd()) };
        assert!(fd > 2);
        let listing = |close: bool| -> Vec<String> {
            let mut c = Command::new("/bin/sh");
            c.arg("-c")
                .arg("for f in /proc/$$/fd/*; do echo ${f##*/}; done");
            if close {
                close_inherited_fds(&mut c);
            }
            let out = c.output().unwrap();
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        };
        assert!(
            listing(false).contains(&fd.to_string()),
            "control: an ordinary spawn inherits the descriptor"
        );
        assert!(
            !listing(true).contains(&fd.to_string()),
            "the descriptor must not reach the hardened child"
        );
        // SAFETY: closing the duplicate made above.
        unsafe { libc::close(fd) };
    }

    /// A home of `/` (`HOME=/`, a service account or container whose passwd home
    /// is `/`) is no usable home: it must neither empty the fixed search
    /// directories (everything is under `/`) nor widen anything.
    #[test]
    fn a_home_of_root_neither_empties_nor_widens_the_trusted_dirs() {
        let none: Vec<PathBuf> = search_dirs(Path::new("git"), &[]);
        assert!(!none.is_empty(), "this host has none of {TRUSTED_DIRS:?}");
        let root = || Some(PathBuf::from("/"));
        let avoid = avoid_dirs(
            Some(PathBuf::from("/")),
            std::env::temp_dir(),
            root(),
            root(),
        );
        assert!(
            avoid.iter().all(|a| a.parent().is_some()),
            "root in the avoid set: {avoid:?}"
        );
        // A temp dir of `/` is dropped too.
        let tmp_root = avoid_dirs(None, PathBuf::from("/"), None, None);
        assert!(
            tmp_root.is_empty(),
            "root tmp in the avoid set: {tmp_root:?}"
        );
        // Same fixed set as with no home at all: not emptied, not widened.
        assert_eq!(search_dirs(Path::new("git"), &avoid), {
            let bare = avoid_dirs(None, std::env::temp_dir(), None, None);
            search_dirs(Path::new("git"), &bare)
        });
        // The other entries still keep their effect.
        let first = std::fs::canonicalize(&none[0]).unwrap();
        let under = avoid_dirs(None, first.clone(), root(), None);
        assert!(search_dirs(Path::new("git"), &under)
            .iter()
            .all(|d| { std::fs::canonicalize(d).is_ok_and(|c| !c.starts_with(&first)) }));
    }

    /// With a consumer on, git gets an allowlist environment: every variable that
    /// relocates its config or names a program is dropped, `HOME` is the
    /// account's, never the process's.
    #[test]
    fn the_hardened_environment_is_an_allowlist() {
        let parent: Vec<(OsString, OsString)> = [
            ("PATH", "/work/space/bin:/tmp/x:.::rel"),
            ("LANG", "C"),
            ("LC_ALL", "C"),
            ("TZ", "UTC"),
            ("TERM", "dumb"),
            ("TMPDIR", "/hostile/tmp"),
            ("TEMP", "/hostile/tmp"),
            ("USER", "mallory"),
            ("LOGNAME", "mallory"),
            ("GIT_AUTHOR_NAME", "mallory"),
            ("GIT_COMMITTER_EMAIL", "m@x"),
            ("HOME", "/hostile"),
            ("XDG_CONFIG_HOME", "/hostile/xdg"),
            ("GIT_CONFIG_GLOBAL", "/hostile/g"),
            ("GIT_CONFIG_SYSTEM", "/hostile/s"),
            ("GIT_CONFIG", "/hostile/c"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "filter.x.clean"),
            ("GIT_CONFIG_VALUE_0", "/hostile/x"),
            ("GIT_CONFIG_PARAMETERS", "'a'='b'"),
            ("GIT_EXEC_PATH", "/hostile"),
            ("GIT_TEMPLATE_DIR", "/hostile"),
            ("GIT_ATTR_SOURCE", "HEAD"),
            ("GIT_NAMESPACE", "x"),
            ("GIT_OBJECT_DIRECTORY", "/hostile"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/hostile"),
            ("GIT_REPLACE_REF_BASE", "refs/x/"),
            ("GIT_PROXY_COMMAND", "/hostile"),
            ("GIT_SSH", "/hostile"),
            ("GIT_SSH_COMMAND", "/hostile"),
            ("GIT_ASKPASS", "/hostile"),
            ("GIT_EDITOR", "/hostile"),
            ("GIT_PAGER", "/hostile"),
            ("GIT_EXTERNAL_DIFF", "/hostile"),
            ("GIT_DIR", "/hostile"),
            ("EDITOR", "/hostile"),
            ("PAGER", "/hostile"),
            ("OPENAI_API_KEY", "sk-secret"),
            ("LD_PRELOAD", "/hostile.so"),
            ("LD_LIBRARY_PATH", "/hostile"),
            ("GCONV_PATH", "/hostile"),
            ("LOCPATH", "/hostile"),
        ]
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
        let env = hardened_env(
            parent,
            Some(PathBuf::from("/home/owner")),
            OsString::from("/usr/bin:/bin"),
        );
        let get = |k: &str| {
            env.iter()
                .filter(|(n, _)| n == k)
                .map(|(_, v)| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        for kept in ["LANG", "LC_ALL", "TZ", "TERM"] {
            assert_eq!(get(kept).len(), 1, "{kept} is kept");
        }
        assert_eq!(
            get("PATH"),
            ["/usr/bin:/bin"],
            "PATH is the fixed one, never the inherited"
        );
        assert_eq!(get("HOME"), ["/home/owner"], "HOME comes from the account");
        assert_eq!(get("GIT_CONFIG_NOSYSTEM"), ["1"]);
        assert_eq!(get("GIT_TERMINAL_PROMPT"), ["0"]);
        for (k, _) in &env {
            let k = k.to_str().unwrap();
            assert!(
                matches!(
                    k,
                    "PATH"
                        | "LANG"
                        | "LC_ALL"
                        | "TZ"
                        | "TERM"
                        | "HOME"
                        | "GIT_CONFIG_NOSYSTEM"
                        | "GIT_TERMINAL_PROMPT"
                ),
                "{k} must not reach git"
            );
        }
        // No account home: no HOME at all (no global config), never the process's.
        let env = hardened_env(
            vec![(OsString::from("HOME"), OsString::from("/hostile"))],
            None,
            OsString::from("/usr/bin"),
        );
        assert!(env.iter().all(|(k, _)| k != "HOME"));
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
