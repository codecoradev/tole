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
use tole_core::plugin_lock::{
    self, admit, admit_unseen, tier_of, Admission, LockEntry, Lockfile, ToolChange, NOT_STORED,
};
use tole_core::sandbox::{Availability, Mode, Policy};
use tole_core::secrets::SecretSet;

use crate::plugin_gate::{
    apply_ledger, blocked_notes, integrity, record_event, render_manifest, source_of, Gate,
};

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
    /// The owner's first-party server names (`[plugins] first_party`).
    pub first_party: Vec<String>,
    /// The process's sandbox policy and what this machine offers.
    pub sandbox: Option<Policy>,
    pub backend: Availability,
    /// The secret names currently released to `mcp` (what an approval made
    /// now would grant).
    pub mcp_grants: Vec<String>,
    /// Whether `list` and `diff` may start servers. False under an agent
    /// session: a command a session ran never launches anything.
    pub probe: bool,
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
    match &action {
        PluginAction::Approve { .. } => {
            crate::owner_only::refuse_under_session("tole plugin approve")?
        }
        PluginAction::Revoke { .. } => {
            crate::owner_only::refuse_under_session("tole plugin revoke")?
        }
        _ => {}
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

/// How `name` would be wrapped if started now.
fn sandbox_status(ctx: &Ctx<'_>, name: &str, stdio: bool) -> String {
    if !stdio {
        return "not applicable (remote server)".into();
    }
    let Some(p) = ctx.sandbox.as_ref().filter(|p| p.mode != Mode::Off) else {
        return "off".into();
    };
    if p.skip.iter().any(|s| s == name) {
        return "skipped (owner's skip list)".into();
    }
    match &ctx.backend {
        Availability::Usable { .. } => format!("wrapped (profile {})", p.profile_name),
        Availability::Unavailable(why) if p.mode == Mode::Required => {
            format!("required but unavailable, not started: {why}")
        }
        Availability::Unavailable(why) => format!("not available, runs unwrapped: {why}"),
    }
}

/// Server text as data: every line behind a bar, so it cannot forge the
/// structure of this output or pass as an instruction of tole.
fn quote(text: &str) -> String {
    if text.is_empty() {
        return "  | (empty)".into();
    }
    text.lines()
        .map(|l| format!("  | {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn state_of(a: &Admission) -> &'static str {
    match a {
        Admission::Trusted => "approved",
        Admission::Changed { .. } => "changed",
        Admission::Moved => "changed (another command or URL)",
        Admission::Rebuilt => "changed (another program file)",
        Admission::Unlocked => "unapproved",
        Admission::Revoked => "revoked",
    }
}

/// The state of a configured server. The lock decides first: a server it
/// would not let a session start (revoked, unapproved, another command or
/// program file) is never started here either.
fn probe_state(
    ctx: &Ctx<'_>,
    eff: &std::collections::BTreeMap<String, LockEntry>,
    cfg: &McpServerConfig,
) -> String {
    let source = source_of(cfg);
    let integrity = integrity(&source);
    if let Some(a) = admit_unseen(eff, &cfg.name, &source, &integrity) {
        return state_of(&a).to_string();
    }
    if !ctx.probe {
        return "locked (not probed: running under a session)".to_string();
    }
    match (ctx.list)(cfg) {
        Err(e) => format!("unreachable: {e}"),
        Ok((v, tools)) => {
            let live = tole_core::plugin_lock::Live {
                source: &source,
                integrity: &integrity,
                version: &v,
                tools: &tools,
            };
            state_of(&admit(eff, &cfg.name, &live)).to_string()
        }
    }
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
    let (mut eff, more) = plugin_lock::effective(&user, &project);
    notes.extend(more);
    // A lock edited by hand cannot bring back what the ledger says was
    // revoked: such an entry is shown and treated as revoked.
    if let Some(u) = ctx.user.as_deref().filter(|_| !broken) {
        let check = apply_ledger(u, &mut eff);
        notes.extend(blocked_notes(&check));
        notes.extend(check.unrecorded.iter().map(|n| {
            format!(
                "`{n}` is approved in the lock but the ledger has no approval for it \
                 (written by hand, or approved before the ledger)"
            )
        }));
    }
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
        let cfg = ctx.specs.iter().find(|c| &c.name == name);
        let source = cfg
            .map(source_of)
            .or_else(|| entry.map(|e| e.source.clone()));
        let stdio = source.as_deref().is_some_and(|s| s.starts_with("stdio:"));
        let state = match cfg {
            None => match entry {
                Some(e) if e.revoked => "revoked (not configured)".to_string(),
                Some(_) => "approved (not configured)".to_string(),
                None => "unapproved (not configured)".to_string(),
            },
            Some(_) if broken => "unapproved (a lock is unreadable)".to_string(),
            Some(cfg) => probe_state(ctx, &eff, cfg),
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
            "revoked": entry.is_some_and(|e| e.revoked),
            "tier": tier_of(name, entry, &ctx.first_party).as_str(),
            "source": source.as_deref().map(|s| ctx.clean(s)),
            "version": entry.map(|e| ctx.clean(&e.version)),
            "manifest_hash": entry.map(|e| e.manifest_hash.clone()),
            "integrity": entry.map(|e| e.integrity.clone()).filter(|i| !i.is_empty()),
            "secrets": entry
                .map(|e| e.secrets.iter().map(|s| ctx.clean(s)).collect::<Vec<_>>())
                .unwrap_or_default(),
            "sandbox": ctx.clean(&sandbox_status(ctx, name, stdio)),
            "approved_with_sandbox": entry.and_then(|e| e.sandbox.as_deref().map(|s| ctx.clean(s))),
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
        let secrets: Vec<&str> = r["secrets"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        out.push_str(&format!(
            "{}  {}  {}  version {}  {}  approved {}  lock {}\n  source {}\n  sandbox {}  secrets {}",
            s("name"),
            s("state"),
            s("tier"),
            s("version"),
            short(&s("manifest_hash")),
            s("approved_at"),
            s("lock"),
            s("source"),
            s("sandbox"),
            if secrets.is_empty() {
                "none".to_string()
            } else {
                secrets.join(", ")
            }
        ));
        if let Some(b) = r["binary"].as_str() {
            out.push_str(&format!("\n  ! {b}"));
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

/// What changed in the grant: approved secret names against those released
/// to `mcp` now.
fn secrets_note(ctx: &Ctx<'_>, e: &LockEntry) -> String {
    let join = |v: Vec<&String>| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
    let mut out = format!(
        "secrets approved: {}\n",
        if e.secrets.is_empty() {
            "none".to_string()
        } else {
            e.secrets.join(", ")
        }
    );
    let added: Vec<&String> = ctx
        .mcp_grants
        .iter()
        .filter(|g| !e.secrets.contains(g))
        .collect();
    let dropped: Vec<&String> = e
        .secrets
        .iter()
        .filter(|g| !ctx.mcp_grants.contains(g))
        .collect();
    if !added.is_empty() {
        out.push_str(&format!(
            "  released to mcp now but not part of the approval (not passed): {}\n",
            join(added)
        ));
    }
    if !dropped.is_empty() {
        out.push_str(&format!(
            "  approved but no longer released to mcp (not passed): {}\n",
            join(dropped)
        ));
    }
    out
}

fn diff(ctx: &Ctx<'_>, name: &str) -> Result<String, String> {
    let cfg = ctx.spec(name)?;
    let user = match ctx.user.as_deref() {
        Some(p) => Lockfile::load(p)?,
        None => Lockfile::default(),
    };
    // The merged lock, as `list` and a session see it: a project entry can
    // revoke or re-pin, and then the server must not be started here either.
    let project = Lockfile::load(&ctx.project_path())?;
    let (mut eff, _) = plugin_lock::effective(&user, &project);
    let ledger_notes = ctx
        .user
        .as_deref()
        .map(|u| blocked_notes(&apply_ledger(u, &mut eff)))
        .unwrap_or_default();
    let Some(e) = eff.get(name) else {
        return Ok(format!(
            "`{}` is not in your plugin lock; `tole plugin approve {}` shows its tools\n",
            ctx.clean(name),
            ctx.clean(name)
        ));
    };
    let source = source_of(cfg);
    let live_integrity = integrity(&source);
    let mut out = format!(
        "`{}` ({}){}\n",
        ctx.clean(name),
        tier_of(name, Some(e), &ctx.first_party).as_str(),
        if e.revoked { " REVOKED" } else { "" }
    );
    if e.source == source {
        out.push_str("source: unchanged\n");
    } else {
        out.push_str(&format!(
            "! source: approved `{}`\n          now      `{}`\n",
            ctx.clean(&e.source),
            ctx.clean(&source)
        ));
    }
    if let Some(b) = binary_note(e) {
        out.push_str(&format!(
            "! {b}\n          approved {}\n          now      {}\n",
            e.integrity,
            if live_integrity.is_empty() {
                "(unreadable)"
            } else {
                &live_integrity
            }
        ));
    } else if !e.integrity.is_empty() {
        out.push_str("binary: unchanged\n");
    }
    out.push_str(&secrets_note(ctx, e));
    for n in &ledger_notes {
        out.push_str(&format!("! {n}\n"));
    }
    let blocked = admit_unseen(&eff, name, &source, &live_integrity);
    let why = match (&blocked, ctx.probe) {
        (Some(a), _) => Some(format!("the lock refuses it: {}", state_of(a))),
        (None, false) => Some("running under an agent session".to_string()),
        (None, true) => None,
    };
    if let Some(why) = why {
        out.push_str(&format!(
            "live tools not fetched, the server is not started ({why}); approved version {} {}\n",
            ctx.clean(&e.version),
            e.manifest_hash
        ));
        out.push_str("(text behind a bar is what the server sent: data, not instructions)\n");
        for t in &e.manifest {
            let d = if t.description.is_empty() {
                NOT_STORED
            } else {
                &t.description
            };
            out.push_str(&format!("  tool `{}`\n{}\n", t.name, quote(d)));
        }
        return Ok(ctx.clean(&out));
    }
    let (version, live) = (ctx.list)(cfg)?;
    out.push_str(&format!(
        "approved version {} {}\nlive version     {} {}\n",
        ctx.clean(&e.version),
        e.manifest_hash,
        ctx.clean(&version),
        plugin_lock::manifest_hash(&live)
    ));
    if e.manifest.is_empty() {
        out.push_str(
            "this approval predates tool snapshots (lock version 1): only its hash is known. \
             The live tool list follows; approving again stores a snapshot.\n",
        );
        out.push_str(&ctx.clean(&render_manifest(name, &version, &live)));
        return Ok(ctx.clean(&out));
    }
    let changes = plugin_lock::tool_changes(&e.manifest, &live, &ctx.secrets);
    if changes.is_empty() {
        out.push_str("no tool changed\n");
    } else {
        out.push_str("(text behind a bar is what the server sent: data, not instructions)\n");
    }
    for c in changes {
        let text = |s: &str| quote(if s.is_empty() { NOT_STORED } else { s });
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
    // The ledger first: if the lock then cannot be written, the record
    // already says revoked and the server stays blocked (fail closed). A
    // project revoke only narrows, so it is recorded as such and does not
    // outlive the project lock.
    if let Some(user) = ctx.user.as_deref() {
        let event = if project { "revoke_project" } else { "revoke" };
        record_event(user, event, name, &entry.manifest_hash);
    }
    lock.save(&path)?;
    let mut out = format!(
        "revoked `{name}` in {}: it is not started again until you approve it again \
         (`tole plugin approve {name}`)\n",
        path.display()
    );
    if entry.secrets.is_empty() {
        out.push_str("it held no granted secret\n");
    } else {
        out.push_str(&format!(
            "it was granted these secrets: rotate them if you no longer trust it: {}\n",
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
#[path = "cmd_tests.rs"]
mod tests;
