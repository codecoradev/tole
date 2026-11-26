//! The `[sandbox]` and `[sandbox.consumers]` tables and the policies they set.

use super::sandbox;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The `[sandbox]` table (#601). USER LAYER ONLY, like [`CronConfig`]: a
/// cloned repository must never loosen or switch off its own sandbox.
/// Off unless `mode` says otherwise; Linux (bubblewrap) only.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// `off` (default) | `optional` | `required`.
    pub mode: Option<String>,
    /// Built-in profile for stdio MCP servers: `server` (default;
    /// workspace read-only) or `strict` (no workspace).
    pub profile: Option<String>,
    /// `none` (default) or `host`.
    pub network: Option<String>,
    /// Extra read-only paths.
    pub read: Option<Vec<String>>,
    /// Extra writable paths.
    pub write: Option<Vec<String>>,
    /// Server names that run unsandboxed (the owner's first-party ones).
    pub skip: Option<Vec<String>>,
    /// Most sandboxed servers at once, 1 to 64 (default 16): a launch past
    /// it is refused.
    pub max_servers: Option<usize>,
    /// Seconds a sandboxed server may sit unused before it is stopped,
    /// 1 to 86400 (default 900); it restarts on its next call.
    pub idle_secs: Option<u64>,
    /// Lazy start (#750, default off): a locked, sandboxed stdio server
    /// whose approval stored a complete tool snapshot registers its tools
    /// from it and starts on its first call, after the same lock checks.
    pub lazy_start: Option<bool>,
    /// The sandbox of the commands a model makes the host run (#748).
    pub consumers: Option<SandboxConsumersConfig>,
    /// Named profiles beyond the built-in `server` and `strict` (#810).
    pub profiles: Option<BTreeMap<String, sandbox::SandboxProfileConfig>>,
    /// Per-server profile bindings, by server name (#810).
    pub servers: Option<BTreeMap<String, sandbox::SandboxServerConfig>>,
}

/// The `[sandbox.consumers]` table (#748): which model-driven commands run
/// in the `command` profile. USER LAYER ONLY, with the rest of `[sandbox]`.
/// Everything is off by default. `cron` covers cron script jobs and
/// `command:` deliveries (agent jobs use `run_command` and `job_start`).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConsumersConfig {
    /// `run_command`: `off` (default) | `optional` | `required`.
    pub run_command: Option<String>,
    /// `job_start`: `off` (default) | `optional` | `required`.
    pub job_start: Option<String>,
    /// The mission `--verify` command: `optional` (the default when the key is
    /// absent, #748) | `off` | `required`. A mission is unattended, so a WRITTEN
    /// `optional` counts as `required`; the default does too, except that it
    /// runs unsandboxed with a loud note on a host with no sandbox backend.
    pub verify: Option<String>,
    /// The `on_turnend` stop gates: `optional` (the default when the key is
    /// absent, #748) | `off` | `required`; same rules as `verify`.
    pub turnend: Option<String>,
    /// Cron script jobs and `command:` deliveries: `off` (default) |
    /// `optional` | `required`. A cron job is unattended, so `optional` counts
    /// as `required`: there is no unsandboxed fallback.
    pub cron: Option<String>,
    /// Network of the `command` profile: `none` (default) or `host`.
    pub network: Option<String>,
    /// Most CPU seconds per process, 1 to 86400 (default 3600).
    pub cpu_secs: Option<u64>,
    /// Most address space per process in MiB, 256 to 1048576 (default 16384).
    pub memory_mb: Option<u64>,
    /// Most processes and threads in the sandbox, 16 to 65536 (default 4096).
    pub processes: Option<u64>,
    /// Largest file a process may write in MiB, 1 to 1048576 (default 4096).
    pub file_size_mb: Option<u64>,
}

impl SandboxConsumersConfig {
    /// The core settings this table sets; a bad value is an error naming its key.
    pub fn settings(&self) -> Result<tole_core::sandbox::command::Consumers, String> {
        use tole_core::sandbox::command::{Consumers, Limits};
        use tole_core::sandbox::{Mode, Network};
        // An absent `verify` or `turnend` is `optional` BY DEFAULT (#748), a
        // written one is the owner's word and never implicit; the other
        // consumers default to off.
        let dflt = Consumers::host_default();
        let mode = |key: &str, v: &Option<String>| match v.as_deref() {
            None if key == "verify" => Ok(dflt.verify),
            None if key == "turnend" => Ok(dflt.turnend),
            None => Ok(Mode::Off),
            Some(s) => Mode::parse(s).ok_or_else(|| {
                format!("sandbox.consumers.{key} must be off, optional or required, got {s:?}")
            }),
        };
        let d = Limits::default();
        let limits = Limits {
            cpu_secs: self.cpu_secs.unwrap_or(d.cpu_secs),
            memory_mb: self.memory_mb.unwrap_or(d.memory_mb),
            processes: self.processes.unwrap_or(d.processes),
            file_size_mb: self.file_size_mb.unwrap_or(d.file_size_mb),
        };
        limits.check()?;
        Ok(Consumers {
            run_command: mode("run_command", &self.run_command)?,
            job_start: mode("job_start", &self.job_start)?,
            verify: mode("verify", &self.verify)?,
            turnend: mode("turnend", &self.turnend)?,
            cron: mode("cron", &self.cron)?,
            verify_implicit: self.verify.is_none(),
            turnend_implicit: self.turnend.is_none(),
            network: match self.network.as_deref() {
                None | Some("none") => Network::None,
                Some("host") => Network::Host,
                Some(s) => {
                    return Err(format!(
                        "sandbox.consumers.network must be none or host, got {s:?}"
                    ))
                }
            },
            limits,
        })
    }
}

impl SandboxConfig {
    /// The settings of the model-command consumers (the defaults when the
    /// table is absent: `verify` and `turnend` optional, the rest off).
    pub fn consumers(&self) -> Result<tole_core::sandbox::command::Consumers, String> {
        self.consumers.as_ref().map_or_else(
            || Ok(tole_core::sandbox::command::Consumers::host_default()),
            SandboxConsumersConfig::settings,
        )
    }

    /// The core policy this table sets; a bad value is an error naming its key.
    pub fn policy(&self) -> Result<tole_core::sandbox::Policy, String> {
        use tole_core::sandbox::{Mode, Network};
        let mode = match self.mode.as_deref() {
            None => Mode::Off,
            Some(s) => Mode::parse(s).ok_or_else(|| {
                format!("sandbox.mode must be off, optional or required, got {s:?}")
            })?,
        };
        let name = self.profile.as_deref().unwrap_or("server");
        let mut profile = self
            .resolve_profile(name)
            .map_err(|e| format!("sandbox.profile: {e}"))?;
        match self.network.as_deref() {
            None => {}
            Some("none") => profile.network = Network::None,
            Some("host") => profile.network = Network::Host,
            Some(s) => return Err(format!("sandbox.network must be none or host, got {s:?}")),
        }
        let paths = |v: &Option<Vec<String>>| v.iter().flatten().map(Into::into).collect();
        if self.read.is_some() {
            profile.read = paths(&self.read);
        }
        if self.write.is_some() {
            profile.write = paths(&self.write);
        }
        self.check_tables()?;
        let servers = self.server_profiles()?;
        use tole_core::sandbox::{
            DEFAULT_IDLE_SECS, DEFAULT_MAX_SERVERS, IDLE_SECS_RANGE, MAX_SERVERS_RANGE,
        };
        let max_servers = self.max_servers.unwrap_or(DEFAULT_MAX_SERVERS);
        if !MAX_SERVERS_RANGE.contains(&max_servers) {
            return Err(format!(
                "sandbox.max_servers must be {} to {}, got {max_servers}",
                MAX_SERVERS_RANGE.start(),
                MAX_SERVERS_RANGE.end()
            ));
        }
        let idle_secs = self.idle_secs.unwrap_or(DEFAULT_IDLE_SECS);
        if !IDLE_SECS_RANGE.contains(&idle_secs) {
            return Err(format!(
                "sandbox.idle_secs must be {} to {}, got {idle_secs}",
                IDLE_SECS_RANGE.start(),
                IDLE_SECS_RANGE.end()
            ));
        }
        Ok(tole_core::sandbox::Policy {
            mode,
            profile_name: name.to_string(),
            profile,
            servers,
            skip: self.skip.clone().unwrap_or_default(),
            max_servers,
            idle_secs,
        })
    }
}
