//! Sandbox for third-party stdio MCP servers (#601, design
//! `docs/sandbox-design.md`, owner decisions of 2026-11-14).
//!
//! Linux only, through bubblewrap (`bwrap`) found on `PATH`. tole builds the
//! `bwrap` argument list itself from a [`Profile`] ([`bwrap_args`], pure and
//! unit-tested flag by flag) and launches `bwrap <args> -- <server>`. The
//! feature is OFF unless the owner's user config sets `[sandbox] mode`.
//!
//! Rules every launch keeps (they are tested one by one):
//! - bwrap's own environment is the scrubbed one (the caller applies the
//!   ENV-1 scrub to the `bwrap` process; bwrap's PID 1 keeps that block,
//!   so `--clearenv` would not help);
//! - no value ever goes through `--setenv` except `HOME` and `TMPDIR`,
//!   which name the private scratch dir (argv is world-readable);
//! - `--unshare-user --disable-userns`: no nested user namespace;
//! - `--cap-drop ALL`, which matters when tole runs as root;
//! - read-only root view, a private `/proc`, no network unless the
//!   profile says `host`.
//!
//! macOS and Windows have no sandbox (owner decision 4): [`detect`] says so
//! and `tole doctor` shows it. A server that needs one is not started
//! (`required`); `optional` runs unsandboxed with a warning, except in an
//! unattended run (cron, `serve`), where it is refused too (decision 2).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Oldest bwrap that has `--disable-userns`.
pub const MIN_BWRAP: (u32, u32) = (0, 8);

/// Where the private scratch dir (`$HOME` and `$TMPDIR` inside) lives.
const SCRATCH: &str = "/scratch";

/// What the owner says about the sandbox. `Off` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No sandbox: servers start as before.
    Off,
    /// Sandbox when it works; otherwise run unsandboxed with a warning,
    /// except in unattended runs, which refuse.
    Optional,
    /// Sandbox or do not start.
    Required,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" => Some(Mode::Off),
            "optional" => Some(Mode::Optional),
            "required" => Some(Mode::Required),
            _ => None,
        }
    }
}

/// How much of the workspace the sandbox sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    None,
    Read,
    Write,
}

/// Network of a sandbox: all or nothing (no host allowlist yet, decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// A private, empty network namespace.
    None,
    /// The host's network.
    Host,
}

/// What one sandboxed process may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub workspace: Workspace,
    pub network: Network,
    /// Extra read-only paths.
    pub read: Vec<PathBuf>,
    /// Extra writable paths.
    pub write: Vec<PathBuf>,
}

/// The built-in profiles: `server` (workspace read-only, no network) and
/// `strict` (nothing from the machine).
pub fn builtin_profile(name: &str) -> Option<Profile> {
    let (workspace, network) = match name {
        "server" => (Workspace::Read, Network::None),
        "strict" => (Workspace::None, Network::None),
        _ => return None,
    };
    Some(Profile {
        workspace,
        network,
        read: Vec::new(),
        write: Vec::new(),
    })
}

/// The process's sandbox settings, from the user config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub mode: Mode,
    pub profile_name: String,
    pub profile: Profile,
    /// Server names left unsandboxed (the owner's first-party servers).
    pub skip: Vec<String>,
}

/// Owner config, set once per process: the same for every session.
/// Per-session state (the workspace) is never kept here: it travels with
/// each launch (`McpServerConfig::workspace`).
static POLICY: OnceLock<Option<Policy>> = OnceLock::new();
/// Monotone: a `tole serve` process or a cron run is unattended for its
/// whole life, and nothing can switch it back off.
static UNATTENDED: AtomicBool = AtomicBool::new(false);

/// Sets the process's policy (the host does this once, from its config).
/// A second call with the same value is fine; a different one is refused,
/// so an embedder cannot silently change the sandbox of running sessions.
pub fn set_policy(p: Option<Policy>) -> Result<(), String> {
    let stored = POLICY.get_or_init(|| p.clone());
    if *stored == p {
        Ok(())
    } else {
        Err("the sandbox policy is already set for this process and differs".into())
    }
}

/// The process's policy, `None` when the sandbox is not configured.
pub fn policy() -> Option<Policy> {
    POLICY.get().cloned().flatten()
}

/// Marks this process as an unattended run (cron job, `tole serve`):
/// no human can answer, so `optional` counts as `required`.
pub fn mark_unattended() {
    UNATTENDED.store(true, Ordering::SeqCst);
}

pub fn unattended() -> bool {
    UNATTENDED.load(Ordering::SeqCst)
}

/// Whether a sandbox backend works on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// bwrap at this path, this version.
    Usable { bwrap: PathBuf, version: String },
    /// Not usable, and why.
    Unavailable(String),
}

/// `(major, minor)` of `bwrap --version` output (`bubblewrap 0.9.0`).
pub fn parse_version(out: &str) -> Option<(u32, u32, String)> {
    let v = out.split_whitespace().last()?;
    let mut it = v.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor, v.to_string()))
}

/// The first executable `name` on `path` (or `name` itself when it has a
/// slash).
pub fn which(name: &str, path: Option<&str>) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }
    path?
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

/// Checks the backend: `bwrap --version`, then a smoke run of
/// `bwrap --ro-bind / / true` that catches kernels and containers that
/// refuse user namespaces.
#[cfg(target_os = "linux")]
pub fn probe(bwrap: &Path) -> Availability {
    let out = match std::process::Command::new(bwrap).arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            return Availability::Unavailable(format!("bwrap --version failed ({})", o.status))
        }
        Err(e) => return Availability::Unavailable(format!("cannot run bwrap: {e}")),
    };
    let Some((major, minor, version)) = parse_version(&out) else {
        return Availability::Unavailable("cannot read the bwrap version".into());
    };
    if (major, minor) < MIN_BWRAP {
        return Availability::Unavailable(format!(
            "bubblewrap {version} is too old (0.8 or later has --disable-userns)"
        ));
    }
    let smoke = std::process::Command::new(bwrap)
        .args(["--ro-bind", "/", "/", "true"])
        .output();
    match smoke {
        Ok(o) if o.status.success() => Availability::Usable {
            bwrap: bwrap.to_path_buf(),
            version,
        },
        Ok(o) => Availability::Unavailable(format!(
            "bwrap cannot create a sandbox here (user namespaces refused?): {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Availability::Unavailable(format!("cannot run bwrap: {e}")),
    }
}

/// The backend of this machine, probed once per process.
pub fn detect() -> Availability {
    static CACHE: std::sync::OnceLock<Availability> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_uncached).clone()
}

#[cfg(target_os = "linux")]
fn detect_uncached() -> Availability {
    match which("bwrap", std::env::var("PATH").ok().as_deref()) {
        Some(p) => probe(&p),
        None => Availability::Unavailable(
            "bubblewrap (bwrap) is not on PATH; install the bubblewrap package".into(),
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn detect_uncached() -> Availability {
    Availability::Unavailable(
        "no sandbox on this platform (Linux only); servers run unsandboxed".into(),
    )
}

/// A profile entry or workspace the sandbox must never grant, judged on
/// the fully resolved path (symlinks, `..`, trailing slashes and relative
/// paths resolved; one that cannot be resolved is refused):
/// - `/`, or anything in `/proc`, `/sys` or `/dev`;
/// - `$HOME` or any ancestor of it (`/home`), since that would mount it.
///   Paths under `$HOME` (a project dir) are fine. When `$HOME` is not
///   known, an ancestor of `/home` or `/root` is refused instead;
/// - the tole data dir, anything inside it, or any ancestor of it.
///
/// Returns the RESOLVED path: the caller must bind that one, never the
/// original, so a symlink swapped in after the check cannot redirect the
/// bind. Names the offending entry on refusal.
pub fn check_grant(
    path: &Path,
    home: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let shown = path.display();
    let p = std::fs::canonicalize(path)
        .map_err(|e| format!("sandbox: cannot resolve {shown} ({e}); it is not granted"))?;
    if p == Path::new("/") || ["/proc", "/sys", "/dev"].iter().any(|d| p.starts_with(d)) {
        return Err(format!(
            "sandbox: {shown} cannot be granted (root, /proc, /sys or /dev)"
        ));
    }
    let holds_home = match home {
        Some(h) => crate::paths::canon_lenient(h).starts_with(&p),
        None => ["/home", "/root"]
            .iter()
            .any(|h| Path::new(h).starts_with(&p)),
    };
    if holds_home {
        return Err(format!(
            "sandbox: {shown} is or holds $HOME and cannot be granted"
        ));
    }
    if let Some(d) = data_dir {
        let d = crate::paths::canon_lenient(d);
        if p.starts_with(&d) || d.starts_with(&p) {
            return Err(format!(
                "sandbox: {shown} is or holds the tole data dir and cannot be granted"
            ));
        }
    }
    Ok(p)
}

/// The `bwrap` argument list (without the program name) that runs
/// `program args` in the view of `profile`. Pure: no I/O, no environment.
/// `prefix` is the install dir of `program` when it is not under a system
/// dir (bound read-only).
pub fn bwrap_args(
    profile: &Profile,
    workspace: Option<&Path>,
    prefix: Option<&Path>,
    program: &Path,
    args: &[String],
) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |parts: &[&str]| a.extend(parts.iter().map(OsString::from));
    // Fixed hardening, always.
    push(&["--unshare-all", "--unshare-user", "--disable-userns"]);
    push(&["--cap-drop", "ALL", "--new-session", "--die-with-parent"]);
    if profile.network == Network::Host {
        push(&["--share-net"]);
    }
    // Read-only system view.
    for d in ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64"] {
        push(&["--ro-bind-try", d, d]);
    }
    let mut etc = vec![
        "ld.so.cache",
        "ld.so.conf",
        "ld.so.conf.d",
        "passwd",
        "group",
        "nsswitch.conf",
        "localtime",
        "ssl",
        "ca-certificates",
        "alternatives",
    ];
    if profile.network == Network::Host {
        etc.extend(["resolv.conf", "hosts"]);
    }
    for f in etc {
        let p = format!("/etc/{f}");
        push(&["--ro-bind-try", &p, &p]);
    }
    push(&["--proc", "/proc", "--dev", "/dev"]);
    push(&["--tmpfs", "/tmp", "--tmpfs", SCRATCH]);
    drop(push);
    let bind = |a: &mut Vec<OsString>, flag: &str, p: &Path| {
        a.push(flag.into());
        a.push(p.into());
        a.push(p.into());
    };
    if let Some(p) = prefix {
        bind(&mut a, "--ro-bind-try", p);
    }
    let mut chdir: &Path = Path::new(SCRATCH);
    if let Some(ws) = workspace {
        match profile.workspace {
            Workspace::None => {}
            Workspace::Read => {
                bind(&mut a, "--ro-bind", ws);
                chdir = ws;
            }
            Workspace::Write => {
                bind(&mut a, "--bind", ws);
                // Paths that run later outside the sandbox stay read-only.
                for rel in [".git/hooks", ".git/config", ".tole"] {
                    bind(&mut a, "--ro-bind-try", &ws.join(rel));
                }
                chdir = ws;
            }
        }
    }
    for p in &profile.read {
        bind(&mut a, "--ro-bind", p);
    }
    for p in &profile.write {
        bind(&mut a, "--bind", p);
    }
    // The only `--setenv` values: the scratch dir (no secrets).
    for k in ["HOME", "TMPDIR"] {
        a.extend([OsString::from("--setenv"), k.into(), SCRATCH.into()]);
    }
    a.extend([OsString::from("--chdir"), chdir.into(), "--".into()]);
    a.push(program.into());
    a.extend(args.iter().map(OsString::from));
    a
}

/// What to do with one server launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Run as before. `warning` is set when the owner asked for a sandbox
    /// (`optional`) and there is none.
    Direct { warning: Option<String> },
    /// Run `program args` instead (the `bwrap` command line).
    Sandboxed {
        program: PathBuf,
        args: Vec<OsString>,
    },
    /// Do not start; the reason names the remedy.
    Refused(String),
}

/// What [`plan`] reads from the machine.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub path: Option<String>,
    pub home: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    /// The workspace of the session this server belongs to.
    pub workspace: Option<PathBuf>,
}

/// Decides one launch. Pure: the caller passes what it found.
pub fn plan(
    policy: Option<&Policy>,
    avail: &Availability,
    unattended: bool,
    env: &Env,
    server: (&str, &str, &[String]),
) -> Launch {
    let name = server.0;
    let Some(p) = policy.filter(|p| p.mode != Mode::Off) else {
        return Launch::Direct { warning: None };
    };
    if p.skip.iter().any(|s| s == name) {
        return Launch::Direct { warning: None };
    }
    let built = match avail {
        Availability::Usable { bwrap, .. } => build(p, env, server).map(|a| (bwrap.clone(), a)),
        Availability::Unavailable(why) => Err(why.clone()),
    };
    match built {
        Ok((program, args)) => Launch::Sandboxed { program, args },
        Err(why) => match p.mode {
            Mode::Optional if !unattended => Launch::Direct {
                warning: Some(format!(
                    "mcp[{name}]: no sandbox ({why}); starting unsandboxed (sandbox mode is optional)"
                )),
            },
            Mode::Optional => Launch::Refused(format!(
                "mcp[{name}]: no sandbox ({why}); an unattended run does not start an \
                 unsandboxed server (install bubblewrap or set sandbox mode = \"off\")"
            )),
            _ => Launch::Refused(format!(
                "mcp[{name}]: not started, sandbox required but unavailable ({why}); install \
                 bubblewrap or set sandbox mode = \"optional\" or \"off\" in the user config"
            )),
        },
    }
}

fn build(p: &Policy, env: &Env, server: (&str, &str, &[String])) -> Result<Vec<OsString>, String> {
    let (name, command, args) = server;
    let (home, data_dir) = (env.home.as_deref(), env.data_dir.as_deref());
    // Everything bound below is the resolved path `check_grant` validated.
    let ws = match (p.profile.workspace, env.workspace.as_deref()) {
        (Workspace::None, _) => None,
        (_, None) => return Err("no workspace to bind".into()),
        (_, Some(w)) => Some(check_grant(w, home, data_dir)?),
    };
    let mut profile = p.profile.clone();
    let grant = |v: &[PathBuf]| -> Result<Vec<PathBuf>, String> {
        v.iter().map(|e| check_grant(e, home, data_dir)).collect()
    };
    profile.read = grant(&p.profile.read)?;
    profile.write = grant(&p.profile.write)?;
    let program = which(command, env.path.as_deref())
        .ok_or_else(|| format!("cannot find the server command {command:?} on PATH ({name})"))?;
    let system = ["/usr", "/bin", "/sbin", "/lib"];
    let mut program = program;
    let mut prefix = None;
    if let (Some(dir), Some(file)) = (program.parent(), program.file_name()) {
        if !system.iter().any(|s| dir.starts_with(s)) {
            let dir = check_grant(dir, home, data_dir)?;
            program = dir.join(file);
            prefix = Some(dir);
        }
    }
    Ok(bwrap_args(
        &profile,
        ws.as_deref(),
        prefix.as_deref(),
        &program,
        args,
    ))
}

/// [`plan`] with this process's policy, backend, `PATH` and data dir.
/// `workspace` is the session's; without one the current directory is used.
pub fn launch(name: &str, workspace: Option<&Path>, command: &str, args: &[String]) -> Launch {
    let policy = policy();
    if policy.as_ref().is_none_or(|p| p.mode == Mode::Off) {
        return Launch::Direct { warning: None };
    }
    let env = Env {
        path: std::env::var("PATH").ok(),
        home: std::env::var_os("HOME").map(PathBuf::from),
        data_dir: crate::paths::tole_data_dir(),
        workspace: workspace
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok()),
    };
    plan(
        policy.as_ref(),
        &detect(),
        unattended(),
        &env,
        (name, command, args),
    )
}

/// The `tole doctor` lines: names and states only.
pub fn doctor_lines(policy: Option<&Policy>, avail: &Availability) -> Vec<String> {
    let backend = match avail {
        Availability::Usable { version, .. } => format!("bubblewrap {version} usable"),
        Availability::Unavailable(why) => format!("unavailable ({why})"),
    };
    let mut out = vec![format!("sandbox: backend {backend}")];
    out.push(match policy.filter(|p| p.mode != Mode::Off) {
        None => "sandbox: off (set [sandbox] mode in the user config to sandbox stdio MCP servers)"
            .into(),
        Some(p) => {
            let mode = if p.mode == Mode::Required {
                "required"
            } else {
                "optional"
            };
            let net = if p.profile.network == Network::Host {
                "host"
            } else {
                "none"
            };
            let skip = if p.skip.is_empty() {
                String::new()
            } else {
                format!(", skipping {}", p.skip.join(", "))
            };
            format!(
                "sandbox: mode {mode}, profile {}, network {net}{skip}",
                p.profile_name
            )
        }
    });
    if cfg!(not(target_os = "linux")) {
        out.push("sandbox: this platform has no sandbox; servers run unsandboxed".into());
    }
    out
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
