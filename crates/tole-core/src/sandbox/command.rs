//! Sandbox for the commands a model makes the host run (#748; design
//! `docs/sandbox-design.md` §7 and §9.2, owner decisions of 2026-11-14).
//!
//! First slice: `run_command` and `job_start`, the two model-driven tools.
//! Second slice: the mission `--verify` command, then the `on_turnend` stop
//! gates. The owner opts in per consumer in the user config
//! (`[sandbox.consumers] run_command | job_start | verify = "off" |
//! "optional" | "required"`, default off for all of them). The stop gates,
//! cron scripts and deliveries keep running as before; the types here are
//! shaped so they can join later.
//!
//! The `command` profile: the workspace writable, with `.git/hooks`,
//! `.git/config` and `.tole` read-only on top (code planted there would run
//! later outside the sandbox), the tole data dir and `$HOME` absent, no
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
//! - policy: `optional` is `required` in an unattended run (a mission's
//!   `verify` is always one: nobody answers there); with no working sandbox
//!   (including macOS and Windows) `required` refuses and `optional` runs
//!   unsandboxed with a warning when a human is attached.
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
pub const PROTECTED: [&str; 3] = [".git/hooks", ".git/config", ".tole"];

/// The commands that can be sandboxed: the two model-driven tools and the
/// mission `--verify` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumer {
    RunCommand,
    JobStart,
    Verify,
}

impl Consumer {
    /// Every consumer, in the order the doctor lists them.
    pub const ALL: [Consumer; 3] = [Consumer::RunCommand, Consumer::JobStart, Consumer::Verify];

    /// The consumer's name, as in `[sandbox.consumers]` and `[secrets]`.
    pub fn name(self) -> &'static str {
        match self {
            Consumer::RunCommand => "run_command",
            Consumer::JobStart => "job_start",
            Consumer::Verify => "verify",
        }
    }

    /// The mission `--verify` command only runs inside `tole mission`, an
    /// autonomous run with nobody to answer a prompt: for it `optional` is
    /// always `required`, whatever the process flag says.
    pub fn always_unattended(self) -> bool {
        self == Consumer::Verify
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
    /// The mission `--verify` command.
    pub verify: Mode,
    /// Network of the `command` profile: none, or the host's.
    pub network: Network,
    pub limits: Limits,
}

impl Default for Consumers {
    fn default() -> Self {
        Consumers {
            run_command: Mode::Off,
            job_start: Mode::Off,
            verify: Mode::Off,
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
            Consumer::Verify => self.verify,
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

/// The protected paths of a RESOLVED workspace that exist and resolve inside
/// it ([`PROTECTED`], as [`Profile::protect`]). A link is followed, so binding the target keeps it read-only whatever
/// the link says; one that leads out of the workspace is not in the sandbox's
/// view at all. A path that does not exist at launch is not protected (see
/// the design note, residual).
pub fn protected_paths(ws: &Path) -> Vec<PathBuf> {
    PROTECTED
        .iter()
        .filter_map(|rel| std::fs::canonicalize(ws.join(rel)).ok())
        .filter(|p| p.starts_with(ws))
        .collect()
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
    profile.protect = protected_paths(&ws);
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
        super::unattended() || consumer.always_unattended(),
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
    command_prepared(consumer, workspace, program, args, false).map(|l| l.cmd)
}

/// A command ready to spawn. `report` is set when it is wrapped in the sandbox.
pub struct Launched {
    pub cmd: Command,
    pub report: Option<LauncherReport>,
}

/// What `bwrap` says about the command it ran (`--json-status-fd`).
///
/// The launcher's own failures (a user namespace refused after the self-test,
/// a mount that cannot be made, `bwrap` gone, a limit it cannot live under)
/// exit non-zero like a failing command does, and the caller must not blame
/// the command for them. `bwrap` reports `{"exit-code": N}` only when the
/// command itself ran and ended; a setup or exec failure writes no such line
/// (checked: a missing bind source and a missing program both print
/// `bwrap: ...`, exit 1 and report no exit code), whatever the exit status
/// says. (`child-pid` is written before the mounts are made, so it proves
/// nothing.) The command inherits the descriptor and could write to it, but it
/// can only make the report agree with what it really did.
pub struct LauncherReport {
    #[cfg(target_os = "linux")]
    read: std::fs::File,
    /// The write end, closed when the report is read.
    #[cfg(target_os = "linux")]
    write: std::fs::File,
}

impl LauncherReport {
    /// The exit code `bwrap` reported for the command, once the process has
    /// ended; `None` when it reported none (the launcher failed).
    #[cfg(target_os = "linux")]
    pub fn command_exit_code(self) -> Option<i32> {
        use std::io::Read;
        drop(self.write);
        let mut read = self.read;
        let mut text = String::new();
        // Non-blocking: the process is gone, what it wrote is in the pipe; a
        // leaked descriptor held by a survivor must not make this wait.
        let mut buf = [0u8; 4096];
        while let Ok(n) = read.read(&mut buf) {
            if n == 0 || text.len() > 64 * 1024 {
                break;
            }
            text.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        exit_code_of(&text)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn command_exit_code(self) -> Option<i32> {
        None
    }
}

/// The `exit-code` of a `bwrap` JSON status text (objects, one or more lines).
#[cfg(target_os = "linux")]
fn exit_code_of(text: &str) -> Option<i32> {
    let mut out = None;
    for v in serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>() {
        let Ok(v) = v else { break };
        if let Some(c) = v.get("exit-code").and_then(serde_json::Value::as_i64) {
            out = i32::try_from(c).ok();
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn status_pipe() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid out-array of two ints.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both are fresh descriptors this function owns.
    let (r, w) = unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    };
    // Only the read end is non-blocking (separate open file descriptions).
    // SAFETY: plain fcntl on an owned descriptor.
    unsafe {
        let fl = libc::fcntl(r.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(r.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    Ok((r, w))
}

/// [`command_for`] for a command that runs in the process's working
/// directory (the mission `--verify` command inherits it, so it is the
/// workspace the sandbox binds). The directory is only looked up when the
/// sandbox is on. A wrapped command comes with a [`LauncherReport`].
pub fn command_in_cwd(
    consumer: Consumer,
    program: &str,
    args: &[String],
) -> Result<Launched, String> {
    let cwd = if consumers().mode(consumer) == Mode::Off {
        PathBuf::new()
    } else {
        std::env::current_dir().map_err(|e| {
            format!(
                "{}: not run, the working directory is unknown ({e})",
                consumer.name()
            )
        })?
    };
    command_prepared(consumer, &cwd, program, args, true)
}

/// Whether `consumer` could run a command here now, without running one: the
/// refusal [`command_in_cwd`] would give, or `Ok`. For a start-up check, so a
/// run that cannot work stops before it spends anything.
pub fn preflight(consumer: Consumer) -> Result<(), String> {
    if consumers().mode(consumer) == Mode::Off {
        return Ok(());
    }
    let cwd = std::env::current_dir().map_err(|e| {
        format!(
            "{}: not run, the working directory is unknown ({e})",
            consumer.name()
        )
    })?;
    match launch_command(consumer, &cwd, "sh", &[]) {
        CommandLaunch::Refused(why) => Err(why),
        _ => Ok(()),
    }
}

/// [`command_for`] plus the launcher report (when `report` and wrapped).
fn command_prepared(
    consumer: Consumer,
    workspace: &Path,
    program: &str,
    args: &[String],
    report: bool,
) -> Result<Launched, String> {
    match launch_command(consumer, workspace, program, args) {
        CommandLaunch::Direct { warning } => {
            if let Some(w) = warning {
                warn_once(consumer, &w);
            }
            let mut c = Command::new(program);
            c.args(args);
            Ok(Launched {
                cmd: c,
                report: None,
            })
        }
        CommandLaunch::Sandboxed {
            program,
            args,
            limits,
        } => {
            let mut c = Command::new(program);
            #[cfg(target_os = "linux")]
            if report {
                use std::os::fd::AsRawFd;
                let (read, write) = status_pipe().map_err(|e| {
                    format!(
                        "{}: not run, no status pipe for the sandbox ({e})",
                        consumer.name()
                    )
                })?;
                let fd = write.as_raw_fd();
                c.arg("--json-status-fd").arg(STATUS_FD.to_string());
                c.args(args);
                harden(&mut c, limits, Some(fd));
                return Ok(Launched {
                    cmd: c,
                    report: Some(LauncherReport { read, write }),
                });
            }
            let _ = report;
            c.args(args);
            harden(&mut c, limits, None);
            Ok(Launched {
                cmd: c,
                report: None,
            })
        }
        CommandLaunch::Refused(why) => Err(why),
    }
}

/// A fallback to no sandbox is warned about once per process and tool, not
/// on every call (a chatty line per command would drown a terminal).
fn warn_once(consumer: Consumer, text: &str) {
    static WARNED: [AtomicBool; 3] = [
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ];
    if !WARNED[consumer.index()].swap(true, Ordering::SeqCst) {
        eprintln!("tole: {text}");
    }
}

/// Limits and file-descriptor hygiene for the `bwrap` launcher, applied
/// between fork and exec. rlimits are inherited by everything inside, and a
/// process cannot raise a hard limit it lowered. Only async-signal-safe
/// calls run there. Linux only.
#[cfg(target_os = "linux")]
fn harden(cmd: &mut Command, limits: Limits, status_fd: Option<std::os::fd::RawFd>) {
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
            // The status pipe of the launcher goes to fd 3 (the only
            // descriptor above 2 that survives the exec).
            if let Some(fd) = status_fd {
                if fd == STATUS_FD {
                    libc::fcntl(fd, libc::F_SETFD, 0);
                } else if libc::dup2(fd, STATUS_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn harden(_cmd: &mut Command, _limits: Limits, _status_fd: Option<i32>) {}

/// The descriptor on which `bwrap` writes its JSON status (`--json-status-fd`).
#[cfg(target_os = "linux")]
const STATUS_FD: i32 = 3;

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
    doctor_lines_for(
        c,
        avail,
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}

/// Toolchain directories under `home` that the sandbox cannot see (it has no
/// `$HOME`; there are no `read` extras yet), so a command that needs them
/// fails inside it, for example with exit 127.
pub fn unseen_toolchain_dirs(home: &Path) -> Vec<PathBuf> {
    [".cargo/bin", ".rustup"]
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

/// [`doctor_lines`] with the home directory given.
pub fn doctor_lines_for(c: &Consumers, avail: &Availability, home: Option<&Path>) -> Vec<String> {
    let word = |m: Mode| match m {
        Mode::Off => "off",
        Mode::Optional => "optional",
        Mode::Required => "required",
    };
    let mut out = Vec::new();
    if Consumer::ALL.iter().all(|k| c.mode(*k) == Mode::Off) {
        out.push(
            "sandbox consumers: run_command off, job_start off, verify off (model commands and \
             the mission --verify command run unsandboxed; set [sandbox.consumers] run_command \
             / job_start / verify in the user config to sandbox them)"
                .into(),
        );
    } else {
        let net = match c.network {
            Network::None => "none",
            Network::Host => "host (commands can reach the network)",
        };
        let l = c.limits;
        out.push(format!(
            "sandbox consumers: run_command {}, job_start {}, verify {}, network {net}, limits \
             cpu {}s memory {}MB processes {} file {}MB",
            word(c.run_command),
            word(c.job_start),
            word(c.verify),
            l.cpu_secs,
            l.memory_mb,
            l.processes,
            l.file_size_mb
        ));
        if let Availability::Unavailable(why) = avail {
            for k in Consumer::ALL {
                let (level, tail) = match (c.mode(k), k.always_unattended()) {
                    (Mode::Off, _) => continue,
                    (Mode::Required, _) => ("ERROR", "refused (required)"),
                    (Mode::Optional, true) => (
                        "ERROR",
                        "refused: a mission is an unattended run, where optional counts as required",
                    ),
                    (Mode::Optional, false) => (
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
    if c.verify != Mode::Off {
        let dirs = home.map(unseen_toolchain_dirs).unwrap_or_default();
        if !dirs.is_empty() {
            let shown: Vec<_> = dirs.iter().map(|d| d.display().to_string()).collect();
            out.push(format!(
                "sandbox consumers: WARNING verify is sandboxed and {} exist but are not visible                  inside the sandbox ($HOME is absent): a verify that needs them (cargo, rustc)                  fails every time, for example with exit 127, and uses up the three strikes",
                shown.join(", ")
            ));
        }
        out.push(
            "sandbox consumers: a sandboxed verify can also fail deterministically for lack of              network (network none unless the owner wrote host) or under the address-space limit              (memory_mb); the model is told to fix it like any failure"
                .into(),
        );
    }
    out.push(
        "sandbox consumers: turnend, cron scripts and deliveries are not sandboxed yet".into(),
    );
    out
}

#[cfg(all(test, unix))]
#[path = "command_tests.rs"]
mod tests;
