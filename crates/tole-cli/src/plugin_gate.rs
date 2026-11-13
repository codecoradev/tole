//! The host side of the plugin lock (#599 part 1, design
//! `third-party-plugins-design.md` §4.1–4.2).
//!
//! **Default policy for a server with no lock entry:** it is never trusted
//! silently. In an interactive terminal session (`tole`, `tole chat`,
//! `tole run`, `tole resume` on a TTY, never inside a child agent) the
//! [`preflight`] shows its FULL tool list (every name, description and
//! input schema, control characters dropped, nothing summarized or cut)
//! before any session starts and asks; a yes writes the user lock and a
//! ledger entry. Everywhere else (missions, cron, `serve`, delegated
//! children, a declined prompt) an unlocked server is unavailable: its
//! tools are not registered and a note names the remedy. A server whose
//! live manifest or version changed since approval, or that is revoked, is
//! handled the same way (re-approval in an interactive session).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tole_core::ledger::LedgerStore as _;
use tole_core::plugin_lock::{self, admit, Admission, LockEntry, Lockfile, ManifestTool};

/// The layered lock of one session.
pub struct Gate {
    lock: BTreeMap<String, LockEntry>,
    /// Project-lock notes and lock read errors.
    pub notes: Vec<String>,
    user_path: Option<PathBuf>,
    /// The lock that could not be read when the session started (user or
    /// project): nothing is trusted, and no approval is written until it is
    /// fixed and the session restarted, even if the file reads fine later.
    broken: Option<PathBuf>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Gate {
    /// Loads the user lock (`user`, the tole data dir's by default) and the
    /// project lock of `workspace`. An unreadable lock trusts nothing.
    pub fn load(user: Option<PathBuf>, workspace: &Path) -> Self {
        let mut notes = Vec::new();
        let mut broken = None;
        let user_lock = match user.as_deref().map(Lockfile::load) {
            Some(Ok(l)) => l,
            Some(Err(e)) => {
                broken = user.clone();
                notes.push(format!(
                    "plugin lock: {e}; no MCP server is trusted until it is fixed"
                ));
                Lockfile::default()
            }
            None => Lockfile::default(),
        };
        let project_path = Lockfile::project_path(workspace);
        let project = match Lockfile::load(&project_path) {
            Ok(l) => l,
            Err(e) => {
                notes.push(format!(
                    "plugin lock: {e}; no MCP server is trusted until it is fixed"
                ));
                return Self {
                    lock: BTreeMap::new(),
                    notes,
                    user_path: user,
                    broken: Some(project_path),
                };
            }
        };
        let (lock, more) = plugin_lock::effective(&user_lock, &project);
        notes.extend(more);
        Self {
            lock,
            notes,
            user_path: user,
            broken,
        }
    }

    /// The connect-time guard: `Ok` only for an exact match. A change is
    /// recorded in the ledger; every refusal names the remedy.
    pub fn check(&self, server: &str, version: &str, tools: &[ManifestTool]) -> Result<(), String> {
        let a = admit(&self.lock, server, version, tools);
        if let Admission::Changed { .. } = &a {
            self.record(
                "manifest_change",
                server,
                &plugin_lock::manifest_hash(tools),
            );
        }
        match a {
            Admission::Trusted => Ok(()),
            other => Err(refusal(server, &other)),
        }
    }

    /// The refusal for `server` when the lock decides without the live
    /// manifest (#680): no entry (or an unreadable lock, which trusts
    /// nothing) or revoked. Such a server is not connected at all, so an
    /// unapproved or revoked stdio server never runs.
    pub fn refuse_unseen(&self, server: &str) -> Option<String> {
        match self.lock.get(server) {
            None => Some(refusal(server, &Admission::Unlocked)),
            Some(e) if e.revoked => Some(refusal(server, &Admission::Revoked)),
            Some(_) => None,
        }
    }

    /// Writes `server`'s approval (as it is now) to the user lock and the
    /// ledger.
    pub fn approve(
        &mut self,
        server: &str,
        source: &str,
        version: &str,
        tools: &[ManifestTool],
    ) -> Result<(), String> {
        let path = self
            .user_path
            .clone()
            .ok_or("plugin lock: no tole data dir to write the user lock to")?;
        if let Some(b) = &self.broken {
            return Err(format!(
                "plugin lock: {} was unreadable when this session started; fix or remove it \
                 and start again before approving",
                b.display()
            ));
        }
        let mut lock = Lockfile::load(&path)?;
        let release = tole_core::secret_release::release();
        let entry = LockEntry {
            source: source.to_string(),
            version: version.to_string(),
            integrity: integrity(source),
            manifest_hash: plugin_lock::manifest_hash(tools),
            secrets: release
                .held_grants()
                .into_iter()
                .filter(|n| release.allows(n, "mcp"))
                .collect(),
            sandbox: None,
            approved_at: now(),
            revoked: false,
            manifest: plugin_lock::snapshot(tools, &tole_core::secrets::process_secrets()),
        };
        lock.servers.insert(server.to_string(), entry.clone());
        lock.save(&path)?;
        self.record("approve", server, &entry.manifest_hash);
        self.lock.insert(server.to_string(), entry);
        Ok(())
    }

    fn record(&self, event: &str, server: &str, hash: &str) {
        if let Some(user) = self.user_path.as_deref() {
            record_event(user, event, server, hash);
        }
    }

    /// `tole doctor`: each configured server's lock state (no connect).
    pub fn doctor_lines(&self, servers: &[String]) -> Vec<String> {
        let mut out = vec!["plugins (MCP servers):".to_string()];
        for s in servers {
            out.push(match self.lock.get(s) {
                Some(e) if e.revoked => format!("  ! {s}: revoked (never connected)"),
                Some(e) => format!(
                    "  ✓ {s}: approved {} ({}, manifest {}); a changed manifest blocks it at connect",
                    tole_core::skill_approval::format_time(e.approved_at),
                    if e.version.is_empty() { "no version" } else { &e.version },
                    &e.manifest_hash[..e.manifest_hash.len().min(19)]
                ),
                None => format!(
                    "  ! {s}: not approved: unavailable in missions, cron and serve; start `tole` \
                     in a terminal to review its tools and approve it"
                ),
            });
        }
        out.extend(self.notes.iter().map(|n| format!("  ! {n}")));
        out
    }
}

/// Appends one ledger line next to the user lock `user` (names and
/// hashes only).
pub fn record_event(user: &Path, event: &str, server: &str, hash: &str) {
    let Some(dir) = user.parent() else {
        return;
    };
    let line = plugin_lock::ledger_line(event, server, hash, now());
    let mut store = crate::ledger_file::FileStore::new(dir.join(plugin_lock::LEDGER_FILE));
    if let Err(e) = store.append(line.as_bytes()) {
        eprintln!("tole: plugin ledger: {e}");
    }
}

/// Why `server`'s tools were not registered, and what to do.
pub fn refusal(server: &str, a: &Admission) -> String {
    match a {
        Admission::Trusted => String::new(),
        Admission::Unlocked => format!(
            "not approved in the plugin lock: its tools are not registered. Start `tole` in a \
             terminal to review its full tool list and approve it; missions, cron and serve never \
             use an unapproved server ({server})"
        ),
        Admission::Changed { locked, live } => format!(
            "its tools or version changed since you approved them (approved {locked}, now {live}): \
             not registered. Start `tole` in a terminal to review the new tool list and re-approve \
             it ({server})"
        ),
        Admission::Revoked => {
            format!("revoked in the plugin lock: its tools are not registered ({server})")
        }
    }
}

/// `sha256:<hex>` of a stdio server's resolved program (empty for HTTP or
/// when it cannot be read).
pub fn integrity(source: &str) -> String {
    let Some(cmd) = source
        .strip_prefix("stdio:")
        .and_then(|c| c.split_whitespace().next())
    else {
        return String::new();
    };
    let path = if cmd.contains('/') {
        Some(PathBuf::from(cmd))
    } else {
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(cmd))
                .find(|p| p.is_file())
        })
    };
    path.and_then(|p| plugin_lock::file_sha256(&p))
        .unwrap_or_default()
}

pub use tole_core::plugin_lock::terminal_safe;

/// The full model-visible text of a server's tools, for approval.
pub fn render_manifest(server: &str, version: &str, tools: &[ManifestTool]) -> String {
    let mut out =
        format!(
        "MCP server `{}` (version {}) offers {} tool(s). This is everything the model will read:\n",
        terminal_safe(server),
        terminal_safe(if version.is_empty() { "unknown" } else { version }),
        tools.len()
    );
    for t in tools {
        let schema = serde_json::to_string_pretty(&t.input_schema).unwrap_or_default();
        out.push_str(&format!(
            "\n--- tool `{}`\ndescription:\n{}\ninput schema:\n{}\n",
            terminal_safe(&t.name),
            terminal_safe(&t.description),
            terminal_safe(&schema)
        ));
    }
    out
}

/// `stdio:<command line>` or the url.
#[cfg(feature = "mcp")]
pub fn source_of(cfg: &tole_core::mcp::McpServerConfig) -> String {
    match &cfg.transport {
        tole_core::mcp::McpTransport::Stdio { command, args } => {
            format!(
                "stdio:{}",
                std::iter::once(command)
                    .chain(args)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        tole_core::mcp::McpTransport::Http { url } => url.clone(),
    }
}

/// Before an interactive session starts: every server that is unlocked or
/// changed is shown in full and the owner is asked (`ask` gets the text
/// and returns the answer). Servers the owner declines stay unavailable.
#[cfg(feature = "mcp")]
pub fn preflight(
    specs: &[tole_core::mcp::McpServerConfig],
    gate: &mut Gate,
    ask: &mut dyn FnMut(&str) -> bool,
) {
    for cfg in specs {
        let Ok((version, tools)) = tole_core::mcp::list_manifest(cfg) else {
            continue; // the registration reports the failure
        };
        let a = admit(&gate.lock, &cfg.name, &version, &tools);
        let why = match a {
            Admission::Unlocked => "is not approved yet",
            Admission::Changed { .. } => "CHANGED since you approved it",
            Admission::Trusted | Admission::Revoked => continue,
        };
        let text = format!(
            "{}\nMCP server `{}` {why}. Approve these tools? [y/N] ",
            render_manifest(&cfg.name, &version, &tools),
            cfg.name
        );
        if ask(&text) {
            if let Err(e) = gate.approve(&cfg.name, &source_of(cfg), &version, &tools) {
                eprintln!("tole: {e}");
            }
        }
    }
}

#[cfg(test)]
#[path = "plugin_gate_tests.rs"]
mod tests;
