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
use tole_core::plugin_lock::{self, admit, Admission, Live, LockEntry, Lockfile, ManifestTool};

/// What an approval records beyond the tool list (see
/// [`Gate::approve_with`]).
#[derive(Debug, Default)]
pub struct Approval {
    pub integrity: Option<String>,
    pub tier: Option<plugin_lock::Tier>,
    /// Name of the sandbox profile the server ran in (`None`: unsandboxed).
    pub sandbox: Option<String>,
    /// Hash of that profile's effective grants (#750); see
    /// [`tole_core::sandbox::identity`].
    pub sandbox_hash: Option<String>,
    pub secrets: Option<Vec<String>>,
    /// The bundle manifest the server is approved from (#811). With it, the
    /// ledger records the manifest hash and each secret grant.
    pub bundle: Option<plugin_lock::BundleRef>,
}

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
    /// The sandbox policy the live profile of a server is read from: the
    /// process's, taken when the gate is loaded (see [`Gate::with_policy`]).
    policy: Option<tole_core::sandbox::Policy>,
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
                    policy: tole_core::sandbox::policy(),
                };
            }
        };
        // The ledger holds the user's decisions: it is checked against the
        // user lock, before a project lock narrows it.
        let mut user_lock = user_lock;
        if let Some(u) = user.as_deref() {
            notes.extend(blocked_notes(&apply_ledger(u, &mut user_lock)));
        }
        let (lock, more) = plugin_lock::effective(&user_lock, &project);
        notes.extend(more);
        Self {
            lock,
            notes,
            user_path: user,
            broken,
            policy: tole_core::sandbox::policy(),
        }
    }

    /// The same gate reading live sandbox profiles from `policy` instead of
    /// the process's (embedders and tests; the process's is set once).
    pub fn with_policy(mut self, policy: Option<tole_core::sandbox::Policy>) -> Self {
        self.policy = policy;
        self
    }

    /// The identity of the sandbox profile `server` would run in now: `None`
    /// for a remote server, with the sandbox off, or for a server the owner
    /// skips. Read at every call, never cached, so an edit of the user
    /// config is seen at the next start (#750).
    pub fn live_profile(
        &self,
        server: &str,
        source: &str,
    ) -> Option<tole_core::sandbox::identity::ProfileId> {
        source.starts_with("stdio:").then_some(())?;
        self.policy.as_ref()?.profile_id(server)
    }

    /// The connect-time guard: `Ok` only for an exact match of the
    /// configured `source` (and the stdio binary it resolves to, #679), the
    /// live manifest and the version. A change is recorded in the ledger;
    /// every refusal names the remedy.
    pub fn check(
        &self,
        server: &str,
        source: &str,
        version: &str,
        tools: &[ManifestTool],
    ) -> Result<(), String> {
        let integrity = integrity(source);
        let live = Live {
            source,
            integrity: &integrity,
            version,
            tools,
        };
        let mut a = admit(&self.lock, server, &live);
        if a == Admission::Trusted {
            a = self
                .bundle_check(server)
                .or_else(|| self.profile_check(server, source))
                .unwrap_or(a);
        }
        let event = match &a {
            Admission::Changed { .. } => Some("manifest_change"),
            Admission::Moved => Some("source_change"),
            Admission::Rebuilt => Some("binary_change"),
            Admission::BundleChanged => Some("bundle_change"),
            Admission::ProfileChanged => Some("profile_change"),
            _ => None,
        };
        if let Some(event) = event {
            self.record(event, server, &plugin_lock::manifest_hash(tools));
        }
        match a {
            Admission::Trusted => Ok(()),
            other => Err(refusal(server, &other)),
        }
    }

    /// The refusal for `server`, configured as `source`, when the lock
    /// decides without connecting (#680, #679): no entry (or an unreadable
    /// lock, which trusts nothing), revoked, another command line or URL than
    /// the approved one, or another program file than the approved binary.
    /// Such a server is not started at all, so a program the owner never
    /// approved under this name never runs. Source and binary changes are
    /// recorded in the ledger.
    pub fn refuse_unseen(&self, server: &str, source: &str) -> Option<String> {
        let a = plugin_lock::admit_unseen(&self.lock, server, source, &integrity(source))
            .or_else(|| self.bundle_check(server))
            .or_else(|| self.profile_check(server, source))?;
        let event = match a {
            Admission::Moved => "source_change",
            Admission::Rebuilt => "binary_change",
            Admission::BundleChanged => "bundle_change",
            Admission::ProfileChanged => "profile_change",
            _ => "",
        };
        if let Some(e) = self.lock.get(server).filter(|_| !event.is_empty()) {
            self.record(event, server, &e.manifest_hash);
        }
        Some(refusal(server, &a))
    }

    /// [`Admission::BundleChanged`] when `server`'s bundle manifest file is
    /// not the approved one now (#811).
    fn bundle_check(&self, server: &str) -> Option<Admission> {
        let live = super::bundle::live_hash(self.lock.get(server)?)?;
        plugin_lock::bundle_admission(&self.lock, server, &live)
    }

    /// [`Admission::ProfileChanged`] when the sandbox profile `server` would
    /// run in now is not the approved one (#750). Read from the process's
    /// policy at every call, never cached, so an edit of the user config is
    /// seen at the next start.
    fn profile_check(&self, server: &str, source: &str) -> Option<Admission> {
        plugin_lock::profile_admission(
            &self.lock,
            server,
            self.live_profile(server, source).as_ref(),
        )
    }

    /// The secret NAMES `server`'s approval granted it (#679), for its
    /// launcher; `None` without an entry (such a server is not started).
    pub fn secret_grant(&self, server: &str) -> Option<Vec<String>> {
        self.lock.get(server).map(|e| e.secrets.clone())
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
        self.approve_with(server, source, version, tools, Approval::default())
    }

    /// [`Gate::approve`] with what the caller measured or decided itself:
    /// the program hash taken before the server was started, the tier, the
    /// sandbox profile it ran in and the secret grant (`None`: the `mcp`
    /// grants held now).
    pub fn approve_with(
        &mut self,
        server: &str,
        source: &str,
        version: &str,
        tools: &[ManifestTool],
        with: Approval,
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
            integrity: with.integrity.unwrap_or_else(|| integrity(source)),
            manifest_hash: plugin_lock::manifest_hash(tools),
            secrets: with.secrets.unwrap_or_else(|| {
                release
                    .held_grants()
                    .into_iter()
                    .filter(|n| release.allows(n, "mcp"))
                    .collect()
            }),
            sandbox: with.sandbox,
            sandbox_hash: with.sandbox_hash,
            approved_at: now(),
            revoked: false,
            manifest: plugin_lock::snapshot(tools, &tole_core::secrets::process_secrets()),
            tier: with.tier,
            bundle: with.bundle,
        };
        lock.servers.insert(server.to_string(), entry.clone());
        lock.save(&path)?;
        match &entry.bundle {
            // A bundle approval carries the manifest hash and one ledger line
            // per granted secret: the ledger can then tell a grant the owner
            // gave from one added to the lock by hand.
            Some(b) => {
                append(
                    &path,
                    plugin_lock::bundle_ledger_line(
                        "approve",
                        server,
                        &entry.manifest_hash,
                        &b.hash,
                        now(),
                    ),
                );
                for secret in &entry.secrets {
                    record_secret(&path, "secret_grant", server, secret);
                }
            }
            // A plain approval says its grants are recorded, then writes one
            // line per granted secret (#825), so a secret added to the lock
            // by hand afterwards is caught like a bundle's.
            None => {
                append(
                    &path,
                    plugin_lock::plain_ledger_line(server, &entry.manifest_hash, now()),
                );
                for secret in &entry.secrets {
                    record_secret(&path, "secret_grant", server, secret);
                }
            }
        }
        // The profile follows every approval, bundle or plain (#750): the
        // ledger then holds what the lock's `sandbox` fields must say.
        append(
            &path,
            plugin_lock::profile_ledger_line(
                server,
                entry.sandbox.as_deref(),
                entry.sandbox_hash.as_deref(),
                now(),
            ),
        );
        self.lock.insert(server.to_string(), entry);
        Ok(())
    }

    fn record(&self, event: &str, server: &str, hash: &str) {
        if let Some(user) = self.user_path.as_deref() {
            record_event(user, event, server, hash);
        }
    }

    /// The effective lock entries (user lock narrowed by the project lock,
    /// minus what the ledger blocks).
    pub fn entries(&self) -> &BTreeMap<String, LockEntry> {
        &self.lock
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

/// One note per server the ledger blocked, and one when the ledger could
/// not be read at all.
pub fn blocked_notes(check: &plugin_lock::LedgerCheck) -> Vec<String> {
    let mut notes: Vec<String> = check
        .blocked
        .iter()
        .map(|(s, why)| format!("plugin lock: `{s}` is not trusted: {why}"))
        .collect();
    notes.extend(check.secrets_unrecorded.iter().map(|s| {
        format!(
            "plugin lock: `{s}` holds secret grants approved before the ledger recorded them, \
             so they cannot be checked; approve it again (or withdraw one with `revoke \
             --secret`) to record them"
        )
    }));
    notes.extend(check.profile_unrecorded.iter().map(|s| {
        format!(
            "plugin lock: `{s}` was approved before approvals recorded their sandbox profile, so \
             a change of its profile is not detected; approve it again (`tole plugin approve \
             {s}`) to record it"
        )
    }));
    if let Some(why) = &check.unreadable {
        notes.push(format!(
            "plugin ledger unreadable ({why}): revocations recorded there are not checked \
             against the lock, and `tole plugin add` / `tole plugin approve` refuse, until it \
             is fixed (make `{}` a regular readable file again, or move it aside if you accept \
             losing that history)",
            plugin_lock::LEDGER_FILE
        ));
    }
    notes
}

/// `Err` when the ledger next to the user lock `user` exists but cannot be
/// read: a new approval must not be written on top of a history that cannot
/// be checked (a missing ledger is fine, there is nothing to compare).
pub fn ensure_ledger_readable(user: &Path) -> Result<(), String> {
    let Some(path) = user.parent().map(|d| d.join(plugin_lock::LEDGER_FILE)) else {
        return Ok(());
    };
    plugin_lock::read_ledger(&path).map(drop).map_err(|why| {
        format!(
            "the plugin ledger cannot be read ({why}), so earlier revocations cannot be checked \
             and nothing new is approved on top of it. Fix it first: make `{}` a regular \
             readable file again (not a symlink, under {} MiB), or move it aside if you accept \
             losing that history, then run the command again; `tole doctor` shows its state",
            path.display(),
            plugin_lock::MAX_LEDGER_BYTES / (1024 * 1024)
        )
    })
}

/// Marks `lock`'s entries the owner's ledger does not allow (see
/// [`plugin_lock::check_against_ledger`]) as revoked, so a lock edited by
/// hand cannot bring back a revoked server or re-pin an approved one.
pub fn apply_ledger(user: &Path, lock: &mut Lockfile) -> plugin_lock::LedgerCheck {
    let Some(path) = user.parent().map(|d| d.join(plugin_lock::LEDGER_FILE)) else {
        return plugin_lock::LedgerCheck::default();
    };
    let text = match plugin_lock::read_ledger(&path) {
        Ok(Some(t)) => t,
        Ok(None) => return plugin_lock::LedgerCheck::default(),
        Err(why) => {
            return plugin_lock::LedgerCheck {
                unreadable: Some(why),
                ..Default::default()
            }
        }
    };
    let check = plugin_lock::check_against_ledger(&lock.servers, &text);
    for (name, _) in &check.blocked {
        if let Some(e) = lock.servers.get_mut(name) {
            e.revoked = true;
        }
    }
    check
}

/// The owner's own servers: the built-in first-party names plus the ones
/// exempted from the sandbox (`[sandbox] skip`, an explicit "my own server").
pub fn first_party_names() -> Vec<String> {
    plugin_lock::DEFAULT_FIRST_PARTY
        .iter()
        .map(|s| s.to_string())
        .chain(
            tole_core::sandbox::policy()
                .map(|p| p.skip)
                .unwrap_or_default(),
        )
        .collect()
}

/// Appends one ledger line next to the user lock `user` (names and
/// hashes only).
pub fn record_event(user: &Path, event: &str, server: &str, hash: &str) {
    append(user, plugin_lock::ledger_line(event, server, hash, now()));
}

/// Appends the grant or revoke of one secret (its NAME) of `server`.
pub fn record_secret(user: &Path, event: &str, server: &str, secret: &str) {
    append(
        user,
        plugin_lock::secret_ledger_line(event, server, secret, now()),
    );
}

fn append(user: &Path, line: String) {
    let Some(dir) = user.parent() else {
        return;
    };
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
        Admission::Moved => format!(
            "its command line or URL is not the one you approved: not registered. If you moved \
             it on purpose, start `tole` in a terminal to review and re-approve it, or run \
             `tole plugin approve {server}` ({server})"
        ),
        Admission::Rebuilt => format!(
            "its program file is not the binary you approved (an upgrade or a swap): not \
             registered. Start `tole` in a terminal to review and re-approve it, or run \
             `tole plugin approve {server}` ({server})"
        ),
        Admission::ProfileChanged => format!(
            "the sandbox profile it would run in (its name or what it grants) is not the one \
             you approved, or no sandbox applies to it now: not registered. Review it with \
             `tole sandbox show {server}` and `tole plugin diff {server}`, then start `tole` \
             in a terminal or run `tole plugin approve {server}` to approve the new profile \
             ({server})"
        ),
        Admission::BundleChanged => format!(
            "its plugin manifest (tole-plugin.toml) is not the one you approved, or cannot be \
             read: not registered, and its secret grants do not apply. Review it and run \
             `tole plugin approve {server}` ({server})"
        ),
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
        let source = source_of(cfg);
        let integrity = integrity(&source);
        let live = Live {
            source: &source,
            integrity: &integrity,
            version: &version,
            tools: &tools,
        };
        let profile = gate.live_profile(&cfg.name, &source);
        let mut a = admit(&gate.lock, &cfg.name, &live);
        if a == Admission::Trusted {
            a = plugin_lock::profile_admission(&gate.lock, &cfg.name, profile.as_ref())
                .unwrap_or(a);
        }
        let why = match a {
            Admission::Unlocked => "is not approved yet",
            Admission::Changed { .. } => "CHANGED since you approved it",
            Admission::Moved => "now runs ANOTHER command or URL than the one you approved",
            Admission::Rebuilt => "now runs ANOTHER program file than the one you approved",
            Admission::BundleChanged => "has a CHANGED plugin manifest",
            Admission::ProfileChanged => {
                "would run in ANOTHER sandbox profile than the one you approved"
            }
            Admission::Trusted | Admission::Revoked => continue,
        };
        let text = format!(
            "{}\nMCP server `{}` {why}. Approve these tools? [y/N] ",
            render_manifest(&cfg.name, &version, &tools),
            cfg.name
        );
        if ask(&text) {
            let with = Approval {
                sandbox: profile.as_ref().map(|p| p.name.clone()),
                sandbox_hash: profile.map(|p| p.hash),
                ..Default::default()
            };
            if let Err(e) = gate.approve_with(&cfg.name, &source_of(cfg), &version, &tools, with) {
                eprintln!("tole: {e}");
            }
        }
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
