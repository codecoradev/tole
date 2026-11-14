//! `tole plugin list|approve|diff|revoke` (#599 part 2, design
//! `third-party-plugins-design.md` §4.1, §4.5): the owner's view of the
//! plugin lock, without starting a session.
//!
//! - Everything printed goes through the process `SecretSet` and is made
//!   terminal-safe (redaction first, then any cap).
//! - `approve` and `revoke` write only with consent: a terminal answer, or a
//!   `--hash` that matches (the live manifest hash to approve, the locked
//!   one to revoke). A script without `--hash` is refused.
//! - A lock that does not read safely (symlink, FIFO, oversized, malformed,
//!   newer) is never written; `list` reports it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::json;
use tole_core::mcp::{Listed, McpServerConfig};
use tole_core::plugin_lock::{self, admit, Admission, LockEntry, Lockfile, ToolChange, NOT_STORED};
use tole_core::secrets::SecretSet;

use crate::plugin_gate::{integrity, record_event, render_manifest, source_of, Gate};

/// `tole plugin <action>`.
#[derive(Debug, clap::Subcommand)]
pub enum PluginAction {
    /// Every MCP server (configured or locked) with its lock state
    /// (approved, changed, unapproved, revoked), version, manifest hash,
    /// approval date and lock. Connects to each configured server.
    List {
        #[arg(long)]
        json: bool,
    },
    /// What changed since approval, tool by tool: added, removed, changed
    /// description, changed schema.
    Diff { name: String },
    /// Show the server's full live tool list, then approve it: answer at a
    /// terminal, or pass --hash with the manifest hash shown.
    Approve {
        name: String,
        #[arg(long)]
        hash: Option<String>,
        /// Also pin it in the project lock (`.tole/plugins.lock`).
        #[arg(long)]
        project: bool,
    },
    /// Mark the server revoked: it never registers until approved again.
    /// At a terminal it asks; a script passes --hash with the locked hash.
    Revoke {
        name: String,
        #[arg(long)]
        hash: Option<String>,
        /// Revoke it in the project lock instead of the user lock.
        #[arg(long)]
        project: bool,
    },
}

/// What a command needs from the host (injected for tests).
pub struct Ctx<'a> {
    pub specs: Vec<McpServerConfig>,
    pub user: Option<PathBuf>,
    pub workspace: PathBuf,
    pub list: &'a dyn Fn(&McpServerConfig) -> Result<Listed, String>,
    pub interactive: bool,
    pub ask: &'a mut dyn FnMut(&str) -> bool,
    pub secrets: SecretSet,
}

impl Ctx<'_> {
    /// Redacted, then terminal-safe.
    fn clean(&self, s: &str) -> String {
        plugin_lock::terminal_safe(&self.secrets.redact(s))
    }

    fn spec(&self, name: &str) -> Result<&McpServerConfig, String> {
        self.specs.iter().find(|c| c.name == name).ok_or_else(|| {
            format!("no MCP server named `{name}` is configured (--mcp-server or `mcp_server`)")
        })
    }

    fn project_path(&self) -> PathBuf {
        Lockfile::project_path(&self.workspace)
    }
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(19)]
}

/// Runs `action`; the text to print, or the error (exit 1).
pub fn run(action: PluginAction, ctx: &mut Ctx<'_>) -> Result<String, String> {
    if matches!(action, PluginAction::Approve { .. }) {
        crate::owner_only::refuse_under_session("tole plugin approve")?;
    }
    match action {
        PluginAction::List { json } => Ok(list(ctx, json)),
        PluginAction::Diff { name } => diff(ctx, &name),
        PluginAction::Approve {
            name,
            hash,
            project,
        } => approve(ctx, &name, hash.as_deref(), project),
        PluginAction::Revoke {
            name,
            hash,
            project,
        } => revoke(ctx, &name, hash.as_deref(), project),
    }
}

/// Whether the binary on disk still has the hash recorded at approval.
fn binary_note(e: &LockEntry) -> Option<&'static str> {
    if e.integrity.is_empty() || !e.source.starts_with("stdio:") {
        return None;
    }
    (integrity(&e.source) != e.integrity)
        .then_some("binary changed since approval: refused at connect until re-approved")
}

fn list(ctx: &Ctx<'_>, as_json: bool) -> String {
    let mut notes = Vec::new();
    let mut load = |p: Option<PathBuf>, which: &str| match p.as_deref().map(Lockfile::load) {
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            notes.push(format!("{which} lock unreadable, nothing trusted: {e}"));
            Lockfile::default()
        }
        None => Lockfile::default(),
    };
    let user = load(ctx.user.clone(), "user");
    let project = load(Some(ctx.project_path()), "project");
    let broken = !notes.is_empty();
    let (eff, more) = plugin_lock::effective(&user, &project);
    notes.extend(more);
    let names: BTreeSet<String> = ctx
        .specs
        .iter()
        .map(|c| c.name.clone())
        .chain(user.servers.keys().cloned())
        .chain(project.servers.keys().cloned())
        .collect();
    let mut rows = Vec::new();
    for name in &names {
        let entry = eff.get(name).or_else(|| user.servers.get(name));
        let state = match ctx.specs.iter().find(|c| &c.name == name) {
            None => match entry {
                Some(e) if e.revoked => "revoked (not configured)".to_string(),
                Some(_) => "approved (not configured)".to_string(),
                None => "unapproved (not configured)".to_string(),
            },
            Some(cfg) => match (ctx.list)(cfg) {
                Err(e) => format!("unreachable: {e}"),
                Ok(_) if broken => "unapproved (a lock is unreadable)".to_string(),
                Ok((v, tools)) => {
                    let source = crate::plugin_gate::source_of(cfg);
                    let integrity = crate::plugin_gate::integrity(&source);
                    let live = tole_core::plugin_lock::Live {
                        source: &source,
                        integrity: &integrity,
                        version: &v,
                        tools: &tools,
                    };
                    match admit(&eff, name, &live) {
                        Admission::Trusted => "approved".into(),
                        Admission::Changed { .. } => "changed".into(),
                        Admission::Moved => "changed (another command or URL)".into(),
                        Admission::Rebuilt => "changed (another program file)".into(),
                        Admission::Unlocked => "unapproved".into(),
                        Admission::Revoked => "revoked".into(),
                    }
                }
            },
        };
        let lock = match (
            user.servers.contains_key(name),
            project.servers.contains_key(name),
        ) {
            (true, true) => "user+project",
            (true, false) => "user",
            (false, true) => "project",
            (false, false) => "-",
        };
        rows.push(json!({
            "name": ctx.clean(name),
            "state": ctx.clean(&state),
            "version": entry.map(|e| ctx.clean(&e.version)),
            "manifest_hash": entry.map(|e| e.manifest_hash.clone()),
            "approved_at": entry.map(|e| tole_core::skill_approval::format_time(e.approved_at)),
            "lock": lock,
            "binary": entry.and_then(binary_note),
        }));
    }
    if as_json {
        return json!({"servers": rows, "notes": notes.iter().map(|n| ctx.clean(n)).collect::<Vec<_>>()})
            .to_string();
    }
    let mut out = String::new();
    for r in &rows {
        let s = |k: &str| r[k].as_str().unwrap_or("-").to_string();
        out.push_str(&format!(
            "{}  {}  version {}  {}  approved {}  lock {}",
            s("name"),
            s("state"),
            s("version"),
            short(&s("manifest_hash")),
            s("approved_at"),
            s("lock")
        ));
        if let Some(b) = r["binary"].as_str() {
            out.push_str(&format!("  ! {b}"));
        }
        out.push('\n');
    }
    for n in notes {
        out.push_str(&format!("! {}\n", ctx.clean(&n)));
    }
    if rows.is_empty() {
        out.push_str("no MCP server is configured or locked\n");
    }
    out
}

fn diff(ctx: &Ctx<'_>, name: &str) -> Result<String, String> {
    let cfg = ctx.spec(name)?;
    let user = match ctx.user.as_deref() {
        Some(p) => Lockfile::load(p)?,
        None => Lockfile::default(),
    };
    let (version, live) = (ctx.list)(cfg)?;
    let Some(e) = user.servers.get(name) else {
        return Ok(format!(
            "`{}` is not in your plugin lock; `tole plugin approve {}` shows its tools\n",
            ctx.clean(name),
            ctx.clean(name)
        ));
    };
    let live_hash = plugin_lock::manifest_hash(&live);
    let mut out = format!(
        "`{}`: approved version {} {}{}\n         live version {} {}\n",
        ctx.clean(name),
        ctx.clean(&e.version),
        e.manifest_hash,
        if e.revoked { " (revoked)" } else { "" },
        ctx.clean(&version),
        live_hash
    );
    if let Some(b) = binary_note(e) {
        out.push_str(&format!("! {b}\n"));
    }
    if e.manifest.is_empty() {
        out.push_str(
            "this approval predates tool snapshots (lock version 1): only its hash is known. \
             The live tool list follows; approving again stores a snapshot.\n",
        );
        out.push_str(&ctx.clean(&render_manifest(name, &version, &live)));
        return Ok(out);
    }
    let changes = plugin_lock::tool_changes(&e.manifest, &live, &ctx.secrets);
    if changes.is_empty() {
        out.push_str("no tool changed\n");
    }
    for c in changes {
        let text = |s: &str| {
            if s.is_empty() {
                NOT_STORED.to_string()
            } else {
                s.to_string()
            }
        };
        out.push_str(&match c {
            ToolChange::Added(t) => format!(
                "+ tool `{}` added\n  description:\n{}\n  schema:\n{}\n",
                t.name,
                text(&t.description),
                text(&t.schema)
            ),
            ToolChange::Removed(t) => format!("- tool `{}` removed\n", t.name),
            ToolChange::Description { old, new } => format!(
                "~ tool `{}`: description changed\n  was:\n{}\n  now:\n{}\n",
                new.name,
                text(&old.description),
                text(&new.description)
            ),
            ToolChange::Schema { old, new } => format!(
                "~ tool `{}`: schema changed\n  was:\n{}\n  now:\n{}\n",
                new.name,
                text(&old.schema),
                text(&new.schema)
            ),
        });
    }
    Ok(ctx.clean(&out))
}

/// Consent for writing: `--hash` must equal `hash`; without it, a terminal
/// answer; a script is refused.
fn consent(
    ctx: &mut Ctx<'_>,
    hash: &str,
    expect: Option<&str>,
    question: &str,
) -> Result<(), String> {
    match expect.map(str::trim) {
        Some(want) if want == hash => Ok(()),
        Some(want) => Err(format!(
            "the hash is now {hash}, not {want}: review it again"
        )),
        None if !ctx.interactive => Err(format!(
            "no terminal to confirm on: pass --hash {hash} after reviewing the text above"
        )),
        None if (ctx.ask)(question) => Ok(()),
        None => Err("not confirmed: nothing written".into()),
    }
}

fn approve(
    ctx: &mut Ctx<'_>,
    name: &str,
    expect: Option<&str>,
    project: bool,
) -> Result<String, String> {
    let cfg = ctx.spec(name)?.clone();
    let (version, live) = (ctx.list)(&cfg)?;
    let hash = plugin_lock::manifest_hash(&live);
    let shown = ctx.clean(&render_manifest(name, &version, &live));
    let q = format!("{shown}\nmanifest {hash}\nApprove `{name}`? [y/N] ");
    // A refusal still shows what was (not) approved, so a script's owner
    // can review it and pass --hash.
    consent(ctx, &hash, expect, &q).map_err(|e| format!("{shown}\nmanifest {hash}\n{e}"))?;
    let mut gate = Gate::load(ctx.user.clone(), &ctx.workspace);
    gate.approve(name, &source_of(&cfg), &version, &live)?;
    let mut out = format!("{shown}\napproved `{name}` ({hash}) in the user lock\n");
    if project {
        let path = ctx.project_path();
        let mut lock = Lockfile::load(&path)?;
        let user = Lockfile::load(ctx.user.as_deref().ok_or("no user lock")?)?;
        // The user's own entry: a project pin can never grant more.
        let entry = user
            .servers
            .get(name)
            .cloned()
            .ok_or("approval not found")?;
        lock.servers.insert(name.to_string(), entry);
        lock.save(&path)?;
        out.push_str(&format!("pinned in the project lock {}\n", path.display()));
    }
    Ok(ctx.clean(&out))
}

fn revoke(
    ctx: &mut Ctx<'_>,
    name: &str,
    expect: Option<&str>,
    project: bool,
) -> Result<String, String> {
    let path = if project {
        ctx.project_path()
    } else {
        ctx.user.clone().ok_or("no tole data dir: no user lock")?
    };
    let mut lock = Lockfile::load(&path)?;
    let entry = lock
        .servers
        .get(name)
        .cloned()
        .ok_or_else(|| format!("`{}` is not in {}", ctx.clean(name), path.display()))?;
    let q = format!(
        "Revoke `{}` ({})? It will not register until approved again. [y/N] ",
        ctx.clean(name),
        entry.manifest_hash
    );
    consent(ctx, &entry.manifest_hash, expect, &q)?;
    lock.servers.get_mut(name).expect("present").revoked = true;
    lock.save(&path)?;
    if let Some(user) = ctx.user.as_deref() {
        record_event(user, "revoke", name, &entry.manifest_hash);
    }
    let mut out = format!("revoked `{name}` in {}\n", path.display());
    if !entry.secrets.is_empty() {
        out.push_str(&format!(
            "it was granted {}: rotate them if you no longer trust it\n",
            entry.secrets.join(", ")
        ));
    }
    Ok(ctx.clean(&out))
}

/// The `Value` form of a row, for tests.
#[cfg(test)]
pub(crate) fn rows(out: &str) -> Vec<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(out).unwrap()["servers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "plugin_cmd_tests.rs"]
mod tests;
