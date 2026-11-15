//! Sandbox for the commands a model makes the host run (#748; design
//! `docs/sandbox-design.md` §7 and §9.2, owner decisions of 2026-11-14).
//!
//! First slice: `run_command` and `job_start`, the two model-driven tools.
//! The owner opts in per consumer in the user config
//! (`[sandbox.consumers] run_command = "off" | "optional" | "required"`,
//! default off). `verify`, the stop gates, cron scripts and deliveries keep
//! running as before; the types here are shaped so they can join later.
//!
//! The `command` profile: the workspace writable, with
//! `.git` (the whole directory) and `.tole` read-only on top (code planted
//! there would run later outside the sandbox), the tole data dir and `$HOME` absent, no
//! network unless the owner writes `network = "host"`, and the hardening
//! flags of every sandbox (see [`super::bwrap_args_for`]). The command is
//! passed to `bwrap` as argv after `--`, never through a shell string.
//!
//! What a launch keeps (each is tested):
//! - every bind is a path [`super::check_grant`] resolved just before the
//!   spawn; the protected sub-paths are resolved the same way;
//! - the environment is the ENV-1 scrubbed one, applied by the caller to the
//!   `bwrap` process itself (a secret reaches a command only through the
//!   `[secrets]` release for its tool, never through `--setenv`);
//! - the launcher gets rlimits (CPU, address space, processes, file size)
//!   and no inherited file descriptor above 2 (Linux only);
//! - policy: `optional` is `required` in an unattended run; with no working
//!   sandbox (including macOS and Windows) `required` refuses and
//!   `optional` runs unsandboxed with a warning when a human is attached.
//!
//! State is owner config set once per process, never per session: the
//! workspace travels with each call.

use super::{bwrap_args_for, check_grant, Availability, Env, Mode, Network, Profile, Workspace};
use std::ffi::OsString;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Paths of the workspace that later run code outside the sandbox: they stay
/// read-only inside it.
pub const PROTECTED: [&str; 2] = [".git", ".tole"];

/// The tools of this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumer {
    RunCommand,
    JobStart,
}

impl Consumer {
    /// The tool's name, as in `[sandbox.consumers]` and `[secrets]`.
    pub fn name(self) -> &'static str {
        match self {
            Consumer::RunCommand => "run_command",
            Consumer::JobStart => "job_start",
        }
    }

    /// A detached job outlives the call by design; everything else dies
    /// with tole.
    fn die_with_parent(self) -> bool {
        self != Consumer::JobStart
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Resource limits of a sandboxed command (rlimits, per process; Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_CPU`, seconds of CPU time.
    pub cpu_secs: u64,
    /// `RLIMIT_AS`, MiB of address space.
    pub memory_mb: u64,
    /// `RLIMIT_NPROC`, processes and threads. The kernel counts them per
    /// user namespace, so the sandbox's own are what is counted.
    pub processes: u64,
    /// `RLIMIT_FSIZE`, MiB of the largest file a process may write.
    pub file_size_mb: u64,
}

pub const CPU_SECS_RANGE: RangeInclusive<u64> = 1..=86_400;
pub const MEMORY_MB_RANGE: RangeInclusive<u64> = 256..=1_048_576;
pub const PROCESSES_RANGE: RangeInclusive<u64> = 16..=65_536;
pub const FILE_SIZE_MB_RANGE: RangeInclusive<u64> = 1..=1_048_576;

impl Default for Limits {
    fn default() -> Self {
        Limits {
            cpu_secs: 3_600,
            memory_mb: 16_384,
            processes: 4_096,
            file_size_mb: 4_096,
        }
    }
}

impl Limits {
    /// Every value inside its range; the error names the config key.
    pub fn check(&self) -> Result<(), String> {
        for (key, v, r) in [
            ("cpu_secs", self.cpu_secs, &CPU_SECS_RANGE),
            ("memory_mb", self.memory_mb, &MEMORY_MB_RANGE),
            ("processes", self.processes, &PROCESSES_RANGE),
            ("file_size_mb", self.file_size_mb, &FILE_SIZE_MB_RANGE),
        ] {
            if !r.contains(&v) {
                return Err(format!(
                    "sandbox.consumers.{key} must be {} to {}, got {v}",
                    r.start(),
                    r.end()
                ));
            }
        }
        Ok(())
    }
}

/// What the owner says about the sandbox of model commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consumers {
    pub run_command: Mode,
    pub job_start: Mode,
    /// Network of the `command` profile: none, or the host's.
    pub network: Network,
    pub limits: Limits,
}

impl Default for Consumers {
    fn default() -> Self {
        Consumers {
            run_command: Mode::Off,
            job_start: Mode::Off,
            network: Network::None,
            limits: Limits::default(),
        }
    }
}

impl Consumers {
    pub fn mode(&self, c: Consumer) -> Mode {
        match c {
            Consumer::RunCommand => self.run_command,
            Consumer::JobStart => self.job_start,
        }
    }
}

/// Owner config, set once per process, the same for every session.
static CONSUMERS: OnceLock<Consumers> = OnceLock::new();

/// Sets the process's consumer settings (the host does this once, from its
/// config). The same value again is fine; a different one is refused, so an
/// embedder cannot change the sandbox of running sessions.
pub fn set_consumers(c: Consumers) -> Result<(), String> {
    if *CONSUMERS.get_or_init(|| c) == c {
        Ok(())
    } else {
        Err("the sandbox consumers are already set for this process and differ".into())
    }
}

/// The process's consumer settings: everything off when none were set.
pub fn consumers() -> Consumers {
    CONSUMERS.get().copied().unwrap_or_default()
}

/// The built-in `command` profile: workspace writable (the protected paths
/// are re-bound read-only by the launch), network as the owner chose.
pub fn command_profile(network: Network) -> Profile {
    Profile {
        workspace: Workspace::Write,
        network,
        read: Vec::new(),
        write: Vec::new(),
        protect: Vec::new(),
    }
}

/// What to do with one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandLaunch {
    /// Run as before. `warning` is set when the owner asked for a sandbox
    /// (`optional`) and there is none.
    Direct { warning: Option<String> },
    /// Run `program args` (the `bwrap` command line) with these limits.
    Sandboxed {
        program: PathBuf,
        args: Vec<OsString>,
        limits: Limits,
    },
    /// Do not run; the reason names the remedy.
    Refused(String),
}

/// Why a command could not be wrapped.
enum Fail {
    /// The program is not there: no sandbox policy can change that.
    Missing(String),
    /// The sandbox cannot be built here (no backend, a refused path).
    Unsandboxable(String),
}

/// The executable `name` in the first absolute `PATH` directory that has it.
/// A relative `PATH` entry is skipped: it would mean something else inside
/// the sandbox than outside.
fn find_program(name: &str, path: Option<&str>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    path?
        .split(':')
        .filter(|d| Path::new(d).is_absolute())
        .map(|d| Path::new(d).join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Most nested gitdirs (`.gitmodules` paths) a
/// launch will protect; more is refused, not skipped.
const MAX_NESTED: usize = 64;

/// What to mount read-only over a RESOLVED workspace so the paths that run
/// code later outside the sandbox cannot be changed from inside
/// ([`Profile::protect`]): the whole top-level `.git` directory, `.tole`, and
/// the gitdirs inside the workspace that can be named cheaply and
/// deterministically (below).
///
/// Locking a few leaves of a writable `.git` is not enough (#849): a mount
/// point cannot be renamed, but its ancestor can (`mv .git .g && cp -a .g
/// .git`), and a writable `.git` is redirected with `commondir` and the like to
/// a planted gitdir whose `hooks` and `core.fsmonitor` a later `git commit`
/// outside the sandbox runs; a leaf that does not exist yet can be created
/// inside. So `.git` is bound read-only as a whole: it cannot be renamed,
/// removed, replaced or written. The cost is that git commands that write
/// (`add`, `commit`, `checkout`, `branch`, `stash`, `gc`, `config`, ...) fail
/// with "Read-only file system" inside a sandboxed command; reads work.
///
/// The same reasoning covers a nested gitdir, which the owner's git runs from
/// the outer repository too (`git status` in the outer repo fires a nested
/// `core.fsmonitor`):
/// - a `.git` gitdir FILE (top level, submodule, worktree) is bound read-only
///   itself, and so is the directory it points to when that is inside the
///   workspace (outside it, it is not in the sandbox's view at all);
/// - every `path` of `.gitmodules` (at most
///   [`MAX_NESTED`]): its `.git`, file or directory, the same way.
///
/// A symbolic link anywhere in that chain (`.git`, `.git/hooks`, `.git/config`,
/// `.tole`, `.gitmodules`, a submodule path, `.tole-worktrees`, a gitdir
/// target), a path that escapes the workspace, an unreadable or too large
/// `.gitmodules` or more than [`MAX_NESTED`] entries is an error and the launch
/// follows the consumer's policy: refused when required. NOT protected: a
/// nested repository that is in neither `.gitmodules` nor `.tole-worktrees`
/// (no workspace scan), and a path that does not exist at launch (bubblewrap
/// would have to create the mount point in the real workspace, and an empty
/// read-only `.git` there would break repository discovery); see the design
/// note, residual.
pub fn protect_plan(ws: &Path) -> Result<Vec<PathBuf>, String> {
    use std::fs::symlink_metadata;
    let mut protect: Vec<PathBuf> = Vec::new();
    // Whether `p` exists, refusing a link.
    let exists = |p: &Path| -> Result<bool, String> {
        match symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => Err(format!(
                "{} is a symbolic link, which a mount cannot protect (the command could replace \\
                 it or write through it); make it a real path",
                p.display()
            )),
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("cannot inspect {} ({e})", p.display())),
        }
    };
    // `.git` at `git` (top level or nested): the file or directory, then the
    // gitdir a file points to.
    let add_git = |git: &Path, out: &mut Vec<PathBuf>| -> Result<(), String> {
        if !exists(git)? {
            return Ok(());
        }
        let meta =
            symlink_metadata(git).map_err(|e| format!("cannot inspect {} ({e})", git.display()))?;
        if meta.is_dir() {
            check_gitdir_content(git, false)?;
        } else if meta.is_file() {
            if multi_linked(&meta) {
                return Err(format!(
                    "{} has several hard links; a write through another name would change it",
                    git.display()
                ));
            }
            if let Some(target) = gitdir_target(ws, git)? {
                // A target inside an already read-only directory (a submodule's
                // gitdir under `.git/modules`) cannot have been redirected since.
                let covered = out.iter().any(|p| target.starts_with(p));
                check_gitdir_content(&target, covered)?;
                if !covered {
                    out.push(target);
                }
            }
        }
        if !out.iter().any(|p| git.starts_with(p)) {
            out.push(git.to_path_buf());
        }
        Ok(())
    };
    add_git(&ws.join(".git"), &mut protect)?;
    let tole = ws.join(".tole");
    if exists(&tole)? {
        protect.push(tole);
    }
    let mut nested: Vec<PathBuf> = Vec::new();
    // Submodule paths.
    let modules = ws.join(".gitmodules");
    if exists(&modules)? {
        for rel in gitmodules_paths(&modules)? {
            let mut p = ws.to_path_buf();
            for c in rel.components() {
                p.push(c);
                exists(&p)?;
            }
            // A listed submodule that is not initialized has no `.git` to bind,
            // and a command could create one (`git init sub`, a planted
            // `core.fsmonitor`) that the owner's `git status` in the outer
            // repository then runs: refused until it is initialized or removed.
            let initialized =
                std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) && exists(&p.join(".git"))?;
            if !initialized {
                return Err(format!(
                    "the submodule {} is listed in .gitmodules but is not initialized (no .git \
                     in it), and the sandbox cannot protect a .git a command would create: \
                     initialize it (git submodule update --init) or remove it",
                    rel.display()
                ));
            }
            // A submodule with submodules of its own: their gitdirs are not
            // named by this plan (only the top-level `.gitmodules` is read), so
            // the layout is refused rather than half protected.
            if exists(&p.join(".gitmodules"))? {
                return Err(format!(
                    "the submodule {} has its own .gitmodules (nested submodules), which the \
                     sandbox cannot protect; refused",
                    rel.display()
                ));
            }
            nested.push(p.join(".git"));
        }
    }
    // A linked worktree whose `.git` file lies inside the workspace, but not
    // under `.tole-worktrees` (which is bound whole), cannot be protected: it is
    // found through the registry of the gitdir in effect, and refused.
    refuse_linked_worktrees(ws)?;
    // The file that names the submodules is itself bound read-only: it is the
    // input of this plan, so a command that could rewrite it would take a
    // submodule out of the next launch's plan.
    if exists(&modules)? {
        if std::fs::symlink_metadata(&modules).is_ok_and(|m| multi_linked(&m)) {
            return Err(format!(
                "{} has several hard links; a write through another name would change it",
                modules.display()
            ));
        }
        protect.push(modules);
    }
    // Child-agent worktrees: the whole directory, read-only, so no entry can be
    // removed or replaced and every worktree `.git` file in it is covered.
    let wts = ws.join(".tole-worktrees");
    if exists(&wts)? {
        protect.push(wts);
    }
    if nested.len() > MAX_NESTED {
        return Err(format!("more than {MAX_NESTED} submodules to protect"));
    }
    for git in nested {
        add_git(&git, &mut protect)?;
    }
    // The workspace is resolved, so each of these is already its own resolved
    // path; keep only what is inside it.
    protect.retain(|p| p.starts_with(ws));
    Ok(protect)
}

/// Most entries of a `hooks` directory looked at.
const MAX_HOOKS: usize = 256;

/// Whether `m` is a regular file with more than one name (a hard link).
#[cfg(unix)]
fn multi_linked(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    m.is_file() && m.nlink() > 1
}

#[cfg(not(unix))]
fn multi_linked(_m: &std::fs::Metadata) -> bool {
    false
}

/// What the host's git runs inside a gitdir DIRECTORY, checked before it is
/// bound read-only. A read-only mount stops writes by path, not by inode, so
/// the places that lead git to code must not have another way in: `hooks` and
/// `config` are not links; `config` and the files of `hooks` have no second
/// hard link (a name made earlier, while the path was unprotected, would stay
/// writable); no entry of `hooks` is a link (it would lead to a writable place
/// in the workspace). Bounded; anything else fails the launch.
fn check_gitdir_content(dir: &Path, covered: bool) -> Result<(), String> {
    use std::fs::symlink_metadata;
    // `commondir` (and `config.worktree`) send git to ANOTHER directory for its
    // config and hooks; that directory, inside the workspace, would be writable.
    // Not followed: refused. (Inside an already read-only directory, such as
    // `.git/worktrees/<id>`, the file is the owner's and cannot have changed.)
    if !covered {
        for redirect in ["commondir", "config.worktree"] {
            let p = dir.join(redirect);
            if symlink_metadata(&p).is_ok() {
                return Err(format!(
                    "{} redirects git to another directory ({redirect}), which the sandbox \
                     cannot protect; refused",
                    dir.display()
                ));
            }
        }
    }
    let link = |p: &Path| {
        format!(
            "{} is a symbolic link, which a mount cannot protect (the command could replace \
             it or write through it); make it a real path",
            p.display()
        )
    };
    let twin = |p: &Path| {
        format!(
            "{} has several hard links; a write through another name would change it",
            p.display()
        )
    };
    for leaf in ["hooks", "config"] {
        let p = dir.join(leaf);
        match symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return Err(link(&p)),
            Ok(m) if multi_linked(&m) => return Err(twin(&p)),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot inspect {} ({e})", p.display())),
        }
    }
    let hooks = dir.join("hooks");
    if hooks.is_dir() {
        let entries = std::fs::read_dir(&hooks)
            .map_err(|e| format!("cannot read {} ({e})", hooks.display()))?;
        for (i, e) in entries.enumerate() {
            let e = e.map_err(|e| format!("cannot read {} ({e})", hooks.display()))?;
            if i >= MAX_HOOKS {
                return Err(format!(
                    "{} has more than {MAX_HOOKS} entries",
                    hooks.display()
                ));
            }
            let m = e
                .metadata()
                .map_err(|e| format!("cannot inspect {} ({e})", hooks.display()))?;
            if m.file_type().is_symlink() {
                return Err(link(&e.path()));
            }
            if multi_linked(&m) {
                return Err(twin(&e.path()));
            }
        }
    }
    Ok(())
}

/// The gitdir in effect for `ws`: the top-level `.git` directory, or the
/// directory a `.git` gitdir file names (inside the workspace or not, it is read
/// on the host). `None` when there is none or the target does not exist.
fn effective_gitdir(ws: &Path) -> Result<Option<PathBuf>, String> {
    let git = ws.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&git) else {
        return Ok(None);
    };
    if meta.is_dir() {
        return Ok(Some(git));
    }
    if !meta.is_file() {
        return Ok(None);
    }
    let text = read_bounded(&git, 4096)?;
    let Some(rel) = text.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
        return Ok(None);
    };
    let rel = Path::new(rel.trim());
    let abs = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        ws.join(rel)
    };
    let dir = lexical(&abs);
    Ok(std::fs::symlink_metadata(&dir)
        .is_ok_and(|m| m.is_dir())
        .then_some(dir))
}

/// Refuses a workspace that has a linked worktree (`git worktree add`) whose
/// `.git` file lies inside the workspace but not under `.tole-worktrees`. The
/// registry is `<gitdir>/worktrees/<id>/gitdir`, a back-pointer to that file.
/// A `worktrees` that is a link or not a directory, a back-pointer that is a
/// link or not a regular file, more than [`MAX_NESTED`] entries or an oversized
/// pointer are refused too, never skipped; a stale pointer (the path no longer
/// exists) and a worktree outside the workspace (not in the sandbox's view) are
/// fine.
fn refuse_linked_worktrees(ws: &Path) -> Result<(), String> {
    use std::fs::symlink_metadata;
    let Some(gitdir) = effective_gitdir(ws)? else {
        return Ok(());
    };
    let registry = gitdir.join("worktrees");
    match symlink_metadata(&registry) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot inspect {} ({e})", registry.display())),
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "{} is a link or not a directory; the linked worktrees it should list cannot \
                 be checked",
                registry.display()
            ))
        }
    }
    let entries = std::fs::read_dir(&registry)
        .map_err(|e| format!("cannot read {} ({e})", registry.display()))?;
    let tole_wts = ws.join(".tole-worktrees");
    for (i, e) in entries.enumerate() {
        let e = e.map_err(|e| format!("cannot read {} ({e})", registry.display()))?;
        if i >= MAX_NESTED {
            return Err(format!(
                "{} lists more than {MAX_NESTED} linked worktrees",
                registry.display()
            ));
        }
        let back = e.path().join("gitdir");
        match symlink_metadata(&back) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot inspect {} ({e})", back.display())),
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                return Err(format!(
                    "{} is a link or not a regular file; the worktree it names cannot be checked",
                    back.display()
                ))
            }
        }
        let text = read_bounded(&back, 4096)?;
        let named = Path::new(text.trim());
        let wt_git = lexical(&if named.is_absolute() {
            named.to_path_buf()
        } else {
            e.path().join(named)
        });
        if wt_git.starts_with(ws)
            && !wt_git.starts_with(&tole_wts)
            && symlink_metadata(&wt_git).is_ok()
        {
            return Err(format!(
                "the linked worktree at {} lies inside the workspace outside .tole-worktrees, \
                 and the sandbox cannot protect its .git file; refused (remove it with git \
                 worktree remove, or keep worktrees outside the workspace)",
                wt_git.display()
            ));
        }
    }
    Ok(())
}

/// `p` with `.` and `..` resolved by text (no file system access).
fn lexical(p: &Path) -> PathBuf {
    let mut norm = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                norm.pop();
            }
            std::path::Component::CurDir => {}
            c => norm.push(c),
        }
    }
    norm
}

/// The directory a gitdir FILE points to, when it is inside the workspace and
/// exists: `Ok(None)` for a target outside (not in the sandbox's view) or
/// missing, an error when the file cannot be read or the path passes through a
/// link.
fn gitdir_target(ws: &Path, file: &Path) -> Result<Option<PathBuf>, String> {
    let shown = file.display();
    let text = read_bounded(file, 4096)?;
    let Some(rel) = text.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
        return Ok(None);
    };
    let rel = Path::new(rel.trim());
    let abs = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        file.parent().unwrap_or(ws).join(rel)
    };
    // `..` and `.` resolved by text, to compare with the real path below.
    let norm = lexical(&abs);
    if !norm.starts_with(ws) {
        return Ok(None);
    }
    match std::fs::canonicalize(&norm) {
        Ok(real) if real == norm => Ok(Some(norm)),
        Ok(_) => Err(format!(
            "the gitdir {} points to ({}) passes through a symbolic link",
            shown,
            norm.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "the gitdir {shown} points to ({}) is inside the workspace and does not exist; \
             a command could create it",
            norm.display()
        )),
        Err(e) => Err(format!("cannot resolve the gitdir of {shown} ({e})")),
    }
}

/// The whole text of `file`, which must be at most `max` bytes: a larger file
/// is an error, never read in part (git on the host parses all of it, so a part
/// would let what lies past the cut go unprotected).
fn read_bounded(file: &Path, max: u64) -> Result<String, String> {
    use std::io::Read;
    let shown = file.display();
    // A FIFO or a device is not read (opening a FIFO blocks until a writer
    // shows up): only a regular file, and opened without blocking.
    let meta = std::fs::symlink_metadata(file).map_err(|e| format!("cannot read {shown} ({e})"))?;
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let f = opts
        .open(file)
        .map_err(|e| format!("cannot read {shown} ({e})"))?;
    let mut text = String::new();
    // One byte more than `max` tells a file that fits from one that does not,
    // whatever its size was when it was inspected.
    f.take(max + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read {shown} ({e})"))?;
    if text.len() as u64 > max {
        return Err(format!(
            "{shown} is larger than {max} bytes; not read in part"
        ));
    }
    Ok(text)
}

/// The `path` values of a `.gitmodules` file, relative and inside the
/// workspace (no root, no `..`), at most [`MAX_NESTED`] entries, in a file of
/// at most 64 KiB (a larger one is refused).
///
/// Git parses `.gitmodules` as a config file (comments after a value, quoting,
/// escapes, line continuation, case-insensitive names), and a plan that read it
/// differently from git would leave a registered submodule unprotected (the
/// file is writable from the sandbox: `path = sub # note` is `sub` for git).
/// So only a plain subset is accepted and anything else refuses the launch:
/// blank lines, whole-line `#`/`;` comments, `[submodule "name"]` headers (the
/// name without `"` or `\`), and `key = value` lines inside one whose value has
/// no `#`, `;`, `"` or `\`.
fn gitmodules_paths(file: &Path) -> Result<Vec<PathBuf>, String> {
    let shown = file.display();
    let unsupported = |n: usize, why: &str| {
        format!(
            "{shown}:{n}: {why}; the sandbox reads only plain `[submodule \"name\"]` sections \
             with `key = value` lines (no comment after a value, quote or escape) and refuses \
             anything else, because git would read it differently"
        )
    };
    let text = read_bounded(file, 64 * 1024)?;
    let mut out = Vec::new();
    let mut in_submodule = false;
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest
                .strip_suffix(']')
                .and_then(|r| r.split_once(' '))
                .filter(|(sec, _)| sec.eq_ignore_ascii_case("submodule"))
                .and_then(|(_, q)| q.strip_prefix('"')?.strip_suffix('"'));
            match name {
                Some(name) if !name.contains(['"', '\\']) => in_submodule = true,
                _ => {
                    return Err(unsupported(
                        n,
                        "a section header that is not `[submodule \"name\"]`",
                    ))
                }
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(unsupported(n, "a line that is not `key = value`"));
        };
        if !in_submodule {
            return Err(unsupported(n, "a key outside a submodule section"));
        }
        let (k, v) = (k.trim(), v.trim());
        if v.contains(['#', ';', '"', '\\']) {
            return Err(unsupported(
                n,
                "a value with a comment character, quote or escape",
            ));
        }
        if !k.eq_ignore_ascii_case("path") {
            continue;
        }
        let p = Path::new(v);
        let bad = v.is_empty()
            || p.is_absolute()
            || p.components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)));
        if bad {
            return Err(format!(
                ".gitmodules names the path {v:?}, which is empty, absolute or leaves the workspace"
            ));
        }
        out.push(p.to_path_buf());
        if out.len() > MAX_NESTED {
            return Err(format!("{shown} names more than {MAX_NESTED} submodules"));
        }
    }
    Ok(out)
}

fn build(
    consumers: &Consumers,
    consumer: Consumer,
    env: &Env,
    program: &str,
    args: &[String],
) -> Result<Vec<OsString>, Fail> {
    let (home, data_dir) = (env.home.as_deref(), env.data_dir.as_deref());
    let grant = |p: &Path| check_grant(p, home, data_dir).map_err(Fail::Unsandboxable);
    let Some(ws) = env.workspace.as_deref() else {
        return Err(Fail::Unsandboxable("no workspace to bind".into()));
    };
    let ws = grant(ws)?;
    if program.contains('/') {
        return Err(Fail::Missing(format!(
            "a sandboxed command runs a program by its bare name, got {program:?}"
        )));
    }
    let found = find_program(program, env.path.as_deref()).ok_or_else(|| {
        Fail::Missing(format!(
            "{} is not on PATH (failed to spawn the sandboxed command)",
            program
        ))
    })?;
    // A program outside the system dirs: its dir is bound read-only, as
    // the resolved path (and run through it) unless the workspace has it.
    let mut exe = found.clone();
    let mut prefix = None;
    if let (Some(dir), Some(file)) = (found.parent(), found.file_name()) {
        if !["/usr", "/bin", "/sbin", "/lib"]
            .iter()
            .any(|s| dir.starts_with(s))
        {
            let dir = grant(dir)?;
            exe = dir.join(file);
            if !dir.starts_with(&ws) {
                prefix = Some(dir);
            }
        }
    }
    let mut profile = command_profile(consumers.network);
    profile.protect = protect_plan(&ws).map_err(Fail::Unsandboxable)?;
    Ok(bwrap_args_for(
        &profile,
        Some(&ws),
        prefix.as_deref(),
        &exe,
        args,
        consumer.die_with_parent(),
    ))
}

/// Decides one command launch. Pure: the caller passes what it found.
pub fn plan_command(
    consumers: &Consumers,
    avail: &Availability,
    unattended: bool,
    env: &Env,
    consumer: Consumer,
    command: (&str, &[String]),
) -> CommandLaunch {
    let mode = consumers.mode(consumer);
    if mode == Mode::Off {
        return CommandLaunch::Direct { warning: None };
    }
    let name = consumer.name();
    let built = match avail {
        Availability::Usable { bwrap, .. } => {
            build(consumers, consumer, env, command.0, command.1).map(|a| (bwrap.clone(), a))
        }
        Availability::Unavailable(why) => Err(Fail::Unsandboxable(why.clone())),
    };
    match built {
        Ok((program, args)) => CommandLaunch::Sandboxed {
            program,
            args,
            limits: consumers.limits,
        },
        Err(Fail::Missing(why)) => CommandLaunch::Refused(format!("{name}: {why}")),
        Err(Fail::Unsandboxable(why)) => match mode {
            Mode::Optional if !unattended => CommandLaunch::Direct {
                warning: Some(format!(
                    "{name}: no sandbox ({why}); running unsandboxed (sandbox.consumers.{name} is optional)"
                )),
            },
            Mode::Optional => CommandLaunch::Refused(format!(
                "{name}: no sandbox ({why}); an unattended run does not run an unsandboxed \
                 command (install bubblewrap or set sandbox.consumers.{name} = \"off\")"
            )),
            _ => CommandLaunch::Refused(format!(
                "{name}: not run, sandbox required but unavailable ({why}); install bubblewrap \
                 or set sandbox.consumers.{name} = \"optional\" or \"off\" in the user config"
            )),
        },
    }
}

/// [`plan_command`] with this process's settings, backend, `PATH` and data
/// dir. `workspace` is the session's own, passed with every call.
pub fn launch_command(
    consumer: Consumer,
    workspace: &Path,
    program: &str,
    args: &[String],
) -> CommandLaunch {
    let c = consumers();
    if c.mode(consumer) == Mode::Off {
        return CommandLaunch::Direct { warning: None };
    }
    let env = Env {
        path: std::env::var("PATH").ok(),
        home: std::env::var_os("HOME").map(PathBuf::from),
        data_dir: crate::paths::tole_data_dir(),
        workspace: Some(workspace.to_path_buf()),
    };
    plan_command(
        &c,
        &super::detect(),
        super::unattended(),
        &env,
        consumer,
        (program, args),
    )
}

/// The process to spawn for `program args` run by `consumer` in `workspace`:
/// the plain command when the sandbox is off, else `bwrap ... -- program
/// args` with the launcher hardened. The caller scrubs the environment and
/// sets stdio as it always did. An error is the refusal, ready to show.
pub fn command_for(
    consumer: Consumer,
    workspace: &Path,
    program: &str,
    args: &[String],
) -> Result<Command, String> {
    match launch_command(consumer, workspace, program, args) {
        CommandLaunch::Direct { warning } => {
            if let Some(w) = warning {
                warn_once(consumer, &w);
            }
            let mut c = Command::new(program);
            c.args(args);
            Ok(c)
        }
        CommandLaunch::Sandboxed {
            program,
            args,
            limits,
        } => {
            // The plan was made a moment ago; make sure what it protects did
            // not change (a path that appeared, a link swapped in) before the
            // command line is handed out.
            recheck_plan(workspace, &args)
                .map_err(|why| format!("{}: not run, {why}", consumer.name()))?;
            let mut c = Command::new(program);
            c.args(args);
            harden(&mut c, limits);
            Ok(c)
        }
        CommandLaunch::Refused(why) => Err(why),
    }
}

/// Plans the protected paths of `workspace` again and requires every one to be
/// bound read-only in `args` (the argument list built from the first plan): a
/// path that appeared since, or a link swapped in for a directory (which makes
/// the plan an error), refuses the launch. What remains is the window between
/// this check and the mount in `bwrap`, which only a same-user process racing
/// the path can use (design note, section 5).
pub(crate) fn recheck_plan(workspace: &Path, args: &[OsString]) -> Result<(), String> {
    let ws = std::fs::canonicalize(workspace).map_err(|e| {
        format!(
            "the workspace {} cannot be resolved ({e})",
            workspace.display()
        )
    })?;
    for p in protect_plan(&ws)? {
        let bound = args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == p.as_os_str() && w[2] == p.as_os_str());
        if !bound {
            return Err(format!(
                "{} appeared or changed while the sandbox was being set up",
                p.display()
            ));
        }
    }
    Ok(())
}

/// A fallback to no sandbox is warned about once per process and tool, not
/// on every call (a chatty line per command would drown a terminal).
fn warn_once(consumer: Consumer, text: &str) {
    static WARNED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];
    if !WARNED[consumer.index()].swap(true, Ordering::SeqCst) {
        eprintln!("tole: {text}");
    }
}

/// Limits and file-descriptor hygiene for the `bwrap` launcher, applied
/// between fork and exec. rlimits are inherited by everything inside, and a
/// process cannot raise a hard limit it lowered. Only async-signal-safe
/// calls run there. Linux only.
#[cfg(target_os = "linux")]
fn harden(cmd: &mut Command, limits: Limits) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only calls getrlimit, setrlimit, syscall and
    // fcntl (all async-signal-safe) on values it owns; no allocation, no
    // locks.
    unsafe {
        cmd.pre_exec(move || {
            set_limit(libc::RLIMIT_CPU as Resource, limits.cpu_secs)?;
            set_limit(
                libc::RLIMIT_AS as Resource,
                limits.memory_mb.saturating_mul(1 << 20),
            )?;
            set_limit(libc::RLIMIT_NPROC as Resource, limits.processes)?;
            set_limit(
                libc::RLIMIT_FSIZE as Resource,
                limits.file_size_mb.saturating_mul(1 << 20),
            )?;
            close_inherited_fds();
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn harden(_cmd: &mut Command, _limits: Limits) {}

/// Lowers the soft and hard limit of `resource` to `value`, never above what
/// the process already has. Fails closed: an error stops the spawn.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)]
fn set_limit(resource: Resource, value: u64) -> std::io::Result<()> {
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `cur` is a valid out-pointer.
    if unsafe { libc::getrlimit(resource, &mut cur) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let v = (value as libc::rlim_t).min(cur.rlim_cur).min(cur.rlim_max);
    let new = libc::rlimit {
        rlim_cur: v,
        rlim_max: v,
    };
    // SAFETY: `new` is a valid in-pointer.
    if unsafe { libc::setrlimit(resource, &new) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The type `getrlimit` takes for a resource: `u32` on glibc, `c_int` on
/// musl.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(all(target_os = "linux", not(target_env = "gnu")))]
type Resource = libc::c_int;

/// Marks every descriptor above 2 close-on-exec, so nothing tole's parent
/// leaked into tole reaches `bwrap` (which passes inherited descriptors on
/// into the sandbox). `close_range` with `CLOSE_RANGE_CLOEXEC` (Linux 5.11)
/// keeps Rust's own exec-status pipe working; older kernels get a bounded
/// `fcntl` sweep.
#[cfg(target_os = "linux")]
fn close_inherited_fds() {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 4;
    // SAFETY: plain syscalls on integers.
    unsafe {
        if libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_uint,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        ) == 0
        {
            return;
        }
        let mut cur = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let top = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut cur) == 0 {
            cur.rlim_cur.min(65_536) as libc::c_int
        } else {
            1_024
        };
        for fd in 3..top {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}

/// The `tole doctor` lines for model commands: names and states only.
/// A line starting `sandbox consumers: ERROR` (a required consumer with no
/// working sandbox) makes the doctor fail; an optional one only warns.
pub fn doctor_lines(c: &Consumers, avail: &Availability) -> Vec<String> {
    let word = |m: Mode| match m {
        Mode::Off => "off",
        Mode::Optional => "optional",
        Mode::Required => "required",
    };
    let mut out = Vec::new();
    if c.run_command == Mode::Off && c.job_start == Mode::Off {
        out.push(
            "sandbox consumers: run_command off, job_start off (model commands run unsandboxed; \
             set [sandbox.consumers] run_command / job_start in the user config to sandbox them)"
                .into(),
        );
    } else {
        let net = match c.network {
            Network::None => "none",
            Network::Host => "host (commands can reach the network)",
        };
        let l = c.limits;
        out.push(format!(
            "sandbox consumers: run_command {}, job_start {}, network {net}, limits cpu {}s \
             memory {}MB processes {} file {}MB",
            word(c.run_command),
            word(c.job_start),
            l.cpu_secs,
            l.memory_mb,
            l.processes,
            l.file_size_mb
        ));
        if let Availability::Unavailable(why) = avail {
            for k in [Consumer::RunCommand, Consumer::JobStart] {
                let (level, tail) = match c.mode(k) {
                    Mode::Off => continue,
                    Mode::Required => ("ERROR", "refused (required)"),
                    Mode::Optional => (
                        "WARNING",
                        "runs unsandboxed with a warning, refused in unattended runs (optional)",
                    ),
                };
                out.push(format!(
                    "sandbox consumers: {level} {} has no working sandbox ({why}); {tail}",
                    k.name()
                ));
            }
        }
    }
    out.push(
        "sandbox consumers: verify, turnend, cron scripts and deliveries are not sandboxed yet"
            .into(),
    );
    out
}

#[cfg(all(test, unix))]
#[path = "command_tests.rs"]
mod tests;
