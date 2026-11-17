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

pub mod command;
pub mod flags;
pub mod identity;
pub mod report;

use std::collections::BTreeMap;
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
    /// Inside a writable workspace: paths re-bound read-only on top of it
    /// (planted code there would run later outside the sandbox). The caller
    /// passes RESOLVED paths, so a link is never a bind path; ignored when
    /// the workspace is not writable.
    pub protect: Vec<PathBuf>,
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
        protect: Vec::new(),
    })
}

/// The process's sandbox settings, from the user config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub mode: Mode,
    pub profile_name: String,
    pub profile: Profile,
    /// Per-server profiles (`[sandbox.servers.<name>]`, #810), by server
    /// name: a server listed here gets that profile instead of
    /// [`Policy::profile`]. Set by the owner's user config only.
    pub servers: BTreeMap<String, ServerProfile>,
    /// Server names left unsandboxed (the owner's first-party servers).
    pub skip: Vec<String>,
    /// Most sandboxed servers this process keeps running at once; a launch
    /// beyond it is refused (never evicting another session's server).
    pub max_servers: usize,
    /// Seconds a sandboxed server may sit unused before it is stopped (it
    /// restarts on its next call).
    pub idle_secs: u64,
}

/// A named profile a server is bound to (#810).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProfile {
    pub name: String,
    pub profile: Profile,
}

impl Policy {
    /// The profile name and profile `server` runs in: its own entry when
    /// the owner bound one, else the policy's default.
    pub fn profile_for(&self, server: &str) -> (&str, &Profile) {
        match self.servers.get(server) {
            Some(s) => (&s.name, &s.profile),
            None => (&self.profile_name, &self.profile),
        }
    }
}

/// Default and bounds of [`Policy::max_servers`].
pub const DEFAULT_MAX_SERVERS: usize = 16;
pub const MAX_SERVERS_RANGE: std::ops::RangeInclusive<usize> = 1..=64;
/// Default and bounds of [`Policy::idle_secs`].
pub const DEFAULT_IDLE_SECS: u64 = 900;
pub const IDLE_SECS_RANGE: std::ops::RangeInclusive<u64> = 1..=86_400;

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

/// The process's configured policy, `None` when the sandbox is not
/// configured. This is the OWNER'S config and nothing else: a run flag
/// (`--no-sandbox`, `--sandbox`, #750) never changes it. Everything that
/// decides trust or approval from the config reads it: the first-party
/// names (`skip`), the sandbox of a `tole plugin add` probe, the model
/// command consumers.
pub fn policy() -> Option<Policy> {
    POLICY.get().cloned().flatten()
}

static LAZY_START: OnceLock<bool> = OnceLock::new();

/// Sets `[sandbox] lazy_start` (#750) once per process: locked, sandboxed
/// stdio servers register from the locked snapshot and start on first use.
/// A second call with another value is refused, like [`set_policy`].
pub fn set_lazy_start(on: bool) -> Result<(), String> {
    if *LAZY_START.get_or_init(|| on) == on {
        Ok(())
    } else {
        Err("the sandbox lazy_start setting is already set for this process and differs".into())
    }
}

/// Whether lazy start is on (default off).
pub fn lazy_start() -> bool {
    LAZY_START.get().copied().unwrap_or(false)
}

/// The policy THIS RUN's stdio MCP servers start under: [`policy`] with the
/// run's flag override applied (#750). `--no-sandbox` reads as "not
/// configured" while the run is attended and is ignored once the process is
/// unattended; `--sandbox <p>` replaces the profile of every server. Only
/// the session's server launch path reads this (connection keys, the idle
/// reaper, the cap, the startup line, [`launch_session`]).
pub fn session_policy() -> Option<Policy> {
    flags::apply(policy())
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

/// Every `name` on `path` that can be the backend, in `PATH` order: an
/// ABSOLUTE directory entry and an executable regular file. A relative or
/// empty entry (`.`, `` = the working directory) is skipped: it names a
/// directory the model can write, so a planted `./bwrap` would decide what the
/// sandbox is (#748: with the implicit default a bad candidate that fails its
/// probe would downgrade verify to unsandboxed). A file without the execute
/// bit is skipped too, so it cannot shadow the real one behind it.
pub fn backend_candidates(name: &str, path: Option<&str>) -> Vec<PathBuf> {
    path.unwrap_or_default()
        .split(':')
        .filter(|d| Path::new(d).is_absolute())
        .map(|d| Path::new(d).join(name))
        .filter(|p| is_executable_file(p))
        .collect()
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

#[cfg(target_os = "linux")]
/// Whether a spawn error may pass on its own (no process slot, no memory, a
/// signal): such a failure says nothing about the host, so it is retried and
/// never cached as "no sandbox here".
fn transient_spawn_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{Interrupted, OutOfMemory, WouldBlock};
    matches!(e.kind(), WouldBlock | Interrupted | OutOfMemory)
}

/// Checks the backend: `bwrap --version`, then a smoke run of
/// `bwrap --ro-bind / / true` that catches kernels and containers that
/// refuse user namespaces.
#[cfg(target_os = "linux")]
pub fn probe(bwrap: &Path) -> Availability {
    probe_classified(bwrap).0
}

/// [`probe`], and whether a negative answer may be transient (a spawn error
/// of the kinds [`transient_spawn_error`] names, still there after retries).
#[cfg(target_os = "linux")]
fn probe_classified(bwrap: &Path) -> (Availability, bool) {
    let mut last = (Availability::Unavailable(String::new()), false);
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
        }
        last = probe_once(bwrap);
        if !last.1 {
            break;
        }
    }
    last
}

#[cfg(target_os = "linux")]
fn probe_once(bwrap: &Path) -> (Availability, bool) {
    let no = |why: String| (Availability::Unavailable(why), false);
    let spawn_failed = |e: std::io::Error| {
        (
            Availability::Unavailable(format!("cannot run bwrap: {e}")),
            transient_spawn_error(&e),
        )
    };
    let out = match std::process::Command::new(bwrap).arg("--version").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => return no(format!("bwrap --version failed ({})", o.status)),
        Err(e) => return spawn_failed(e),
    };
    let Some((major, minor, version)) = parse_version(&out) else {
        return no("cannot read the bwrap version".into());
    };
    if (major, minor) < MIN_BWRAP {
        return no(format!(
            "bubblewrap {version} is too old (0.8 or later has --disable-userns)"
        ));
    }
    let smoke = std::process::Command::new(bwrap)
        .args(["--ro-bind", "/", "/", "true"])
        .output();
    match smoke {
        Ok(o) if o.status.success() => (
            Availability::Usable {
                bwrap: bwrap.to_path_buf(),
                version,
            },
            false,
        ),
        Ok(o) => no(format!(
            "bwrap cannot create a sandbox here (user namespaces refused?): {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => spawn_failed(e),
    }
}

/// The backend of this machine, probed once per process. A DEFINITE answer is
/// cached (a usable bwrap, no bwrap on `PATH`, a refused namespace); a failure
/// that may be transient (no process slot, no memory) is retried by
/// [`probe`] and, if it persists, answered but NOT cached, so one bad moment
/// is never a permanent "no sandbox" for a long-lived `tole serve`.
pub fn detect() -> Availability {
    static CACHE: std::sync::Mutex<Option<Availability>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = cache.as_ref() {
        return a.clone();
    }
    let (a, definite) = detect_uncached();
    if definite {
        *cache = Some(a.clone());
    }
    a
}

/// The answer, and whether it is definite (may be cached).
#[cfg(target_os = "linux")]
fn detect_uncached() -> (Availability, bool) {
    let found = backend_candidates("bwrap", std::env::var("PATH").ok().as_deref());
    if found.is_empty() {
        return (
            Availability::Unavailable(
                "bubblewrap (bwrap) is not on an absolute PATH entry; install the bubblewrap package"
                    .into(),
            ),
            true,
        );
    }
    // The first USABLE candidate wins; a bad earlier one does not decide. When
    // none works the first one's reason is the answer.
    let mut first = None;
    let mut definite = true;
    for p in found {
        let (a, transient) = probe_classified(&p);
        if matches!(a, Availability::Usable { .. }) {
            return (a, true);
        }
        definite &= !transient;
        first.get_or_insert(a);
    }
    (
        first.unwrap_or(Availability::Unavailable(String::new())),
        definite,
    )
}

#[cfg(not(target_os = "linux"))]
fn detect_uncached() -> (Availability, bool) {
    (
        Availability::Unavailable("no sandbox on this platform (Linux only)".into()),
        true,
    )
}

/// A profile entry or workspace the sandbox must never grant, judged on
/// the fully resolved path (symlinks, `..`, trailing slashes and relative
/// paths resolved; one that cannot be resolved is refused):
/// - `/`, or anything in `/proc`, `/sys` or `/dev`;
/// - `$HOME`, the account's passwd home or any ancestor of either (`/home`),
///   since that would mount it.
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
    // The git tool takes its global config from the passwd home, not `$HOME`
    // (which a plan takes from the environment): `~/.gitconfig` must never be
    // sandbox-writable, whatever `$HOME` says.
    if let Some(h) = crate::paths::account_home() {
        if crate::paths::canon_lenient(&h).starts_with(&p) {
            return Err(format!(
                "sandbox: {shown} is or holds the account's home directory and cannot be granted"
            ));
        }
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
/// dir (bound read-only). The sandbox dies with tole (see
/// [`bwrap_args_for`] for the one case that must not).
pub fn bwrap_args(
    profile: &Profile,
    workspace: Option<&Path>,
    prefix: Option<&Path>,
    program: &Path,
    args: &[String],
) -> Vec<OsString> {
    bwrap_args_for(profile, workspace, prefix, program, args, true)
}

/// [`bwrap_args`] with a choice the caller owns: `die_with_parent` is
/// `false` only for a detached job (`job_start`), whose process is meant to
/// outlive the call. Every other flag is fixed.
pub fn bwrap_args_for(
    profile: &Profile,
    workspace: Option<&Path>,
    prefix: Option<&Path>,
    program: &Path,
    args: &[String],
    die_with_parent: bool,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |parts: &[&str]| a.extend(parts.iter().map(OsString::from));
    // Fixed hardening, always.
    push(&["--unshare-all", "--unshare-user", "--disable-userns"]);
    push(&["--cap-drop", "ALL", "--new-session"]);
    if die_with_parent {
        push(&["--die-with-parent"]);
    }
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
                // Paths that run later outside the sandbox stay read-only,
                // after the writable bind so they win.
                for p in &profile.protect {
                    bind(&mut a, "--ro-bind", p);
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

impl Env {
    /// What this process sees now: `PATH`, `HOME`, the tole data dir and
    /// `workspace` (the current directory when `None`).
    pub fn current(workspace: Option<&Path>) -> Self {
        Env {
            path: std::env::var("PATH").ok(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            data_dir: crate::paths::tole_data_dir(),
            workspace: workspace
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok()),
        }
    }
}

/// Why a launch under `policy` could not be wrapped although the backend is
/// usable (a granted path that no longer resolves, no workspace, a refused
/// program directory): `None` when it can, or when no sandbox is configured
/// for `server`. In mode `optional` [`plan`] starts such a server unwrapped;
/// a caller that holds an approval recorded WITH a sandbox uses this to
/// refuse the launch instead (fail closed).
pub fn build_failure(
    policy: &Policy,
    avail: &Availability,
    env: &Env,
    server: (&str, &str, &[String]),
) -> Option<String> {
    if policy.mode == Mode::Off || policy.skip.iter().any(|s| s == server.0) {
        return None;
    }
    match avail {
        Availability::Usable { .. } => build(policy, env, server).err(),
        Availability::Unavailable(_) => None,
    }
}

/// Why `server` would start UNSANDBOXED under `policy` although a sandbox is
/// configured for it: `Some(reason)` exactly when [`plan`] returns
/// `Launch::Direct` with a warning (mode `optional`, a human attached, and
/// no backend or a wrapper that cannot be built). `None` for a sandboxed,
/// skipped, refused (`required`, unattended) or unconfigured server, so the
/// fallback is never reported where [`plan`] does not take it.
pub fn fallback_reason(
    policy: &Policy,
    avail: &Availability,
    unattended: bool,
    env: &Env,
    server: (&str, &str, &[String]),
) -> Option<String> {
    if policy.mode != Mode::Optional || unattended || policy.skip.iter().any(|s| s == server.0) {
        return None;
    }
    match avail {
        Availability::Usable { .. } => build(policy, env, server).err(),
        Availability::Unavailable(why) => Some(why.clone()),
    }
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
    let base = p.profile_for(name).1;
    let ws = match (base.workspace, env.workspace.as_deref()) {
        (Workspace::None, _) => None,
        (_, None) => return Err("no workspace to bind".into()),
        (_, Some(w)) => Some(check_grant(w, home, data_dir)?),
    };
    let mut profile = base.clone();
    let grant = |v: &[PathBuf]| -> Result<Vec<PathBuf>, String> {
        v.iter().map(|e| check_grant(e, home, data_dir)).collect()
    };
    profile.read = grant(&base.read)?;
    profile.write = grant(&base.write)?;
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

/// [`plan`] with this process's configured policy, backend, `PATH` and data
/// dir (a probe, `tole plugin add`: never changed by a run flag).
/// `workspace` is the session's; without one the current directory is used.
pub fn launch(name: &str, workspace: Option<&Path>, command: &str, args: &[String]) -> Launch {
    launch_under(policy(), name, workspace, command, args)
}

/// [`launch`] under this run's [`session_policy`]: what a session's stdio
/// MCP server starts under (the flags of #750 apply).
pub fn launch_session(
    name: &str,
    workspace: Option<&Path>,
    command: &str,
    args: &[String],
) -> Launch {
    launch_under(session_policy(), name, workspace, command, args)
}

fn launch_under(
    policy: Option<Policy>,
    name: &str,
    workspace: Option<&Path>,
    command: &str,
    args: &[String],
) -> Launch {
    if policy.as_ref().is_none_or(|p| p.mode == Mode::Off) {
        return Launch::Direct { warning: None };
    }
    let env = Env::current(workspace);
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
        out.push(
            "sandbox: this platform has no sandbox; servers and commands run unsandboxed".into(),
        );
    }
    out.extend(flags::note());
    out
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
