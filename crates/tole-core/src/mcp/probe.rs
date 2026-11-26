//! A one-shot probe of a server that is not approved yet (#600, `tole plugin
//! add`).
//!
//! The reactor keeps a connection (and its process) for the life of the
//! host, which is right for a session and wrong for a look at unvetted code.
//! [`probe_manifest`] starts a stdio server on its own, lists its tools and
//! stops it again on every exit path:
//!
//! - **Own process group** (Unix): the server leads a fresh group, and the
//!   whole group is killed when the probe ends, whether it returned, failed,
//!   timed out or panicked, so a launcher's grandchildren do not outlive it.
//! - **No secret**: the environment is scrubbed with an empty grant, so no
//!   secret reaches a server the owner has not approved, whatever the
//!   `[secrets]` release says.
//! - **Sandbox on demand**: with `require_sandbox` the server starts only
//!   inside the sandbox; a launch that would run direct (sandbox off,
//!   skipped, no backend) is refused before anything is spawned.
//! - **stderr is discarded**: a hostile server cannot write escape
//!   sequences to the owner's terminal.
//!
//! A remote (HTTP) server has no local process to stop and goes through the
//! shared reactor. On a platform without process groups only the server
//! process itself is killed.
#![allow(unsafe_code)] // SAFETY: killpg and pre_exec of a process group the probe owns; see the blocks.

use super::*;

/// How long the whole probe (spawn, handshake, listing) may take.
const PROBE_BUDGET: Duration = Duration::from_secs(30);

/// Kills the process group led by `pid` (Unix); a no-op elsewhere.
struct GroupKill(Option<u32>);

impl GroupKill {
    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.filter(|p| *p > 1) {
            // SAFETY: killpg on a group this probe created; a stale group
            // id fails with ESRCH and nothing else happens.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill();
    }
}

/// `cfg`'s version and tool list, from a process that is stopped (with its
/// whole group) before this returns. The server gets no secret. With
/// `require_sandbox` it is started only inside the sandbox.
pub fn probe_manifest(cfg: &McpServerConfig, require_sandbox: bool) -> Result<Listed, String> {
    probe_manifest_within(cfg, require_sandbox, PROBE_BUDGET)
}

/// [`probe_manifest`] with another time budget for the whole probe.
pub fn probe_manifest_within(
    cfg: &McpServerConfig,
    require_sandbox: bool,
    budget: Duration,
) -> Result<Listed, String> {
    let McpTransport::Stdio { command, args } = &cfg.transport else {
        return list_manifest(cfg);
    };
    let (name, workspace) = (cfg.name.clone(), cfg.workspace.clone());
    let (command, args) = (command.clone(), args.clone());
    // Its own thread and runtime: this works from any caller, async or not.
    std::thread::Builder::new()
        .name("tole-mcp-probe".into())
        .spawn(move || {
            run(
                &name,
                workspace.as_deref(),
                &command,
                &args,
                require_sandbox,
                budget,
            )
        })
        .map_err(|e| format!("could not start the probe: {e}"))?
        .join()
        .map_err(|_| "the probe panicked".to_string())?
}

fn run(
    name: &str,
    workspace: Option<&std::path::Path>,
    command: &str,
    args: &[String],
    require_sandbox: bool,
    budget: Duration,
) -> Result<Listed, String> {
    let mut std_cmd = match crate::sandbox::launch(name, workspace, command, args) {
        crate::sandbox::Launch::Sandboxed { program, args } => {
            let mut c = std::process::Command::new(program);
            c.args(args);
            c
        }
        crate::sandbox::Launch::Direct { .. } if require_sandbox => {
            return Err(format!(
                "mcp {name:?}: not started: this server must run in the sandbox and the \
                 sandbox does not apply to it (set `[sandbox] mode`, check `tole doctor`)"
            ));
        }
        crate::sandbox::Launch::Direct { warning } => {
            if let Some(w) = warning {
                eprintln!("tole: {w}");
            }
            let mut c = std::process::Command::new(command);
            c.args(args);
            c
        }
        crate::sandbox::Launch::Refused(why) => return Err(why),
    };
    // No secret: the grant of a server nobody approved is empty.
    crate::subprocess::scrub_env_with_keep(&mut std_cmd, &|_| false);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        std_cmd.process_group(0);
    }
    // The probe's own group no longer receives the terminal's SIGINT, and
    // tole installs no signal handler: if tole dies mid-probe (Ctrl-C), no
    // Drop runs. Ask the kernel to kill the server when its parent goes
    // (Linux; the child is spawned from this probe's thread, which lives
    // until the probe ends). The check closes the race of a parent that died
    // before the request took effect. A launcher's own children are covered
    // by the group kill on a normal end, and by `bwrap --die-with-parent`
    // inside the sandbox.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe calls (prctl, getppid) run between
        // fork and exec.
        let parent = std::process::id() as libc::pid_t;
        unsafe {
            std_cmd.pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the probe runtime: {e}"))?;
    // The child is registered with this runtime when it is spawned.
    let _enter = rt.enter();
    let mut tokio_cmd = tokio::process::Command::from(std_cmd);
    tokio_cmd.kill_on_drop(true);
    let (transport, _stderr) =
        rmcp::transport::child_process::TokioChildProcess::builder(tokio_cmd)
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("mcp {name:?}: failed to start server: {e}"))?;
    // From here on every exit path, a panic included, ends the group.
    let group = GroupKill(transport.id());
    let out = rt.block_on(async {
        tokio::time::timeout(budget, async {
            let service =
                rmcp::service::serve_client(rmcp::model::ClientConfig::default(), transport)
                    .await
                    .map_err(|e| format!("mcp {name:?}: handshake failed: {e}"))?;
            let mut conn = McpConnection {
                service,
                used: Instant::now(),
            };
            let listed = conn.list_tools().await;
            // Stop the group while the server process is still ours (not
            // yet reaped), so the group id cannot belong to anyone else.
            group.kill();
            drop(conn);
            listed
        })
        .await
        .map_err(|_| format!("mcp {name:?}: probe timed out"))?
    });
    drop(group);
    out
}
