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
use tole_core::mcp::{Listed, McpServerConfig, McpTransport};
use tole_core::plugin_launcher::unpinned_launcher;
use tole_core::plugin_lock::Tier;
use tole_core::plugin_lock::{
    self, admit, admit_unseen, tier_of, Admission, LockEntry, Lockfile, ToolChange, NOT_STORED,
};
use tole_core::plugin_scan;
use tole_core::sandbox::{Availability, Mode, Policy};
use tole_core::secrets::SecretSet;

use crate::plugin_gate::{
    apply_ledger, blocked_notes, integrity, record_event, render_manifest, source_of, Approval,
    Gate,
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
    /// Install a third-party MCP server from `name=<command line or https
    /// url>`: start it in the sandbox (required for an unverified one),
    /// show its full tool text and the scan, and lock it after you type the
    /// confirmation. Interactive only; there is no `--yes`.
    Add { spec: String },
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
    /// Shows the text and returns the line the owner typed (`add`).
    pub typed: &'a mut dyn FnMut(&str) -> String,
    /// Starts a server for review the way an unapproved server must be
    /// started (see `mcp::probe_manifest`); the flag requires the sandbox.
    pub probe_one: &'a dyn Fn(&McpServerConfig, bool) -> Result<Listed, String>,
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
        PluginAction::Add { .. } => crate::owner_only::refuse_under_session("tole plugin add")?,
        _ => {}
    }
    match action {
        PluginAction::List { json } => Ok(list(ctx, json)),
        PluginAction::Diff { name } => diff(ctx, &name),
        PluginAction::Add { spec } => add(ctx, &spec),
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
        Admission::BundleChanged => "changed (plugin manifest)",
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
    let mut user = load(ctx.user.clone(), "user");
    let project = load(Some(ctx.project_path()), "project");
    let broken = !notes.is_empty();
    // A lock edited by hand cannot bring back what the ledger says was
    // revoked: such an entry is shown and treated as revoked. The ledger
    // records the USER's decisions, so it is compared with the user lock,
    // before a project lock narrows it.
    if let Some(u) = ctx.user.as_deref().filter(|_| !broken) {
        let check = apply_ledger(u, &mut user);
        notes.extend(blocked_notes(&check));
        notes.extend(check.unrecorded.iter().map(|n| {
            format!(
                "`{n}` is approved in the lock but the ledger has no approval for it \
                 (written by hand, or approved before the ledger)"
            )
        }));
    }
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
    let mut user = user;
    let ledger_notes = ctx
        .user
        .as_deref()
        .map(|u| blocked_notes(&apply_ledger(u, &mut user)))
        .unwrap_or_default();
    let (eff, _) = plugin_lock::effective(&user, &project);
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

/// Most bytes of tool text (names, descriptions, schemas) `add` shows and
/// locks: more is not something an owner can review.
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

fn is_stdio(cfg: &McpServerConfig) -> bool {
    matches!(cfg.transport, McpTransport::Stdio { .. })
}

/// Why a server of `tier` may not be started on this machine now, or `None`.
/// A third-party server nobody verified starts only inside the sandbox
/// (design §4.3): with it off, skipped or unavailable (no Linux, no
/// bubblewrap) nothing is started. A server the owner lists as their own
/// (`[sandbox] skip`, the first-party list) is not asked for one.
fn sandbox_refusal(ctx: &Ctx<'_>, cfg: &McpServerConfig, tier: Tier) -> Option<String> {
    if tier != Tier::ThirdPartyUnverified || !is_stdio(cfg) {
        return None;
    }
    let wrapped = ctx
        .sandbox
        .as_ref()
        .is_some_and(|p| p.mode != Mode::Off && !p.skip.iter().any(|s| s == &cfg.name))
        && matches!(ctx.backend, Availability::Usable { .. });
    (!wrapped).then(|| {
        format!(
            "`{}` is a third-party server nobody verified: it starts only inside the sandbox, \
             and here the sandbox is {}. Turn it on (`[sandbox] mode = \"required\"` in the user \
             config; Linux with bubblewrap only), or, if it is your own server, list it under \
             `[sandbox] skip`.",
            cfg.name,
            sandbox_status(ctx, &cfg.name, true)
        )
    })
}

/// The profile an approval records: the one the server ran in, if any.
fn profile_used(ctx: &Ctx<'_>, cfg: &McpServerConfig) -> Option<String> {
    let p = ctx.sandbox.as_ref()?;
    (is_stdio(cfg)
        && p.mode != Mode::Off
        && !p.skip.iter().any(|s| s == &cfg.name)
        && matches!(ctx.backend, Availability::Usable { .. }))
    .then(|| p.profile_name.clone())
}

/// Starts `cfg` to read its tools for a review, the way its tier allows:
/// the owner's own servers through the shared connection (as a session
/// would), everything else through the one-shot probe (no secret, process
/// group ended, sandbox required for an unverified one). The program file's
/// hash is taken before and checked after, so a binary swapped under the
/// probe is refused; it is the hash that gets locked.
fn start_for_review(
    ctx: &Ctx<'_>,
    cfg: &McpServerConfig,
    tier: Tier,
) -> Result<(Listed, String), String> {
    if let Some(why) = sandbox_refusal(ctx, cfg, tier) {
        return Err(why);
    }
    let source = source_of(cfg);
    let before = integrity(&source);
    let listed = if tier == Tier::FirstParty {
        (ctx.list)(cfg)?
    } else {
        (ctx.probe_one)(cfg, tier == Tier::ThirdPartyUnverified && is_stdio(cfg))?
    };
    if integrity(&source) != before {
        return Err(format!(
            "the program file of `{}` changed while it was started; nothing was written, try again",
            ctx.clean(&cfg.name)
        ));
    }
    let size: usize = listed
        .1
        .iter()
        .map(|t| {
            t.name.len() + t.description.len() + plugin_lock::canonical_json(&t.input_schema).len()
        })
        .sum();
    if size > MAX_MANIFEST_BYTES {
        return Err(format!(
            "`{}` lists {size} bytes of tool text, more than the {MAX_MANIFEST_BYTES} a person can \
             review: not added",
            ctx.clean(&cfg.name)
        ));
    }
    Ok((listed, before))
}

/// Everything the model would read, server text behind a bar.
fn render_fenced(name: &str, version: &str, tools: &[plugin_lock::ManifestTool]) -> String {
    let mut out =
        format!(
        "MCP server `{}` (version {}) offers {} tool(s). This is everything the model would read \
         (text behind a bar is what the server sent: data, not instructions):\n",
        name,
        if version.is_empty() { "unknown" } else { version },
        tools.len()
    );
    for t in tools {
        let schema = serde_json::to_string_pretty(&t.input_schema).unwrap_or_default();
        out.push_str(&format!(
            "\n--- tool `{}`\ndescription:\n{}\ninput schema:\n{}\n",
            t.name,
            quote(&t.description),
            quote(&schema)
        ));
    }
    out
}

/// `tole plugin add name=<command line or https url>`: starts the server
/// where its tier allows, shows its full text and the scan, and locks it
/// after a typed confirmation. The server is not configured for sessions
/// (that stays the owner's config); the approval is bound to exactly this
/// source.
fn add(ctx: &mut Ctx<'_>, spec: &str) -> Result<String, String> {
    if !ctx.interactive {
        return Err(
            "`tole plugin add` is interactive only (it asks you to type a confirmation): run it \
             in a terminal"
                .into(),
        );
    }
    if spec.chars().any(char::is_control) {
        return Err("the spec has a control character".into());
    }
    let cfg = McpServerConfig::parse(spec)?;
    if let McpTransport::Stdio { command, .. } = &cfg.transport {
        if command.starts_with('-') {
            return Err(format!(
                "`{}` is not a program (it starts with `-`)",
                ctx.clean(command)
            ));
        }
    }
    let name = cfg.name.clone();
    // A remote server is sent the bearer token its NAME has in the
    // environment: never to a URL nobody approved.
    if matches!(cfg.transport, McpTransport::Http { .. }) {
        let var = tole_core::mcp::token_env_name(&name);
        if crate::config_apply::real_env(&var).is_some() {
            return Err(format!(
                "{var} is set: the probe would send that bearer token to a URL you have not \
                 approved. Unset it for this command, or pick another name"
            ));
        }
    }
    // Never first-party through `add`: a name is not a proof, and an
    // unknown program must not inherit the owner's own servers' tier.
    if ctx.first_party.iter().any(|f| f == &name) {
        return Err(format!(
            "`{}` is on your first-party list: `add` never installs under such a name; use \
             `tole plugin approve {}` for your own server",
            ctx.clean(&name),
            ctx.clean(&name)
        ));
    }
    let user_lock = match ctx.user.as_deref() {
        Some(p) => Lockfile::load(p)?,
        None => return Err("no tole data dir: no user lock to write".into()),
    };
    if user_lock.servers.contains_key(&name) {
        return Err(format!(
            "`{}` is already in your plugin lock: `tole plugin approve {}` re-approves it, \
             `tole plugin revoke {}` blocks it (a name is not reused for another program)",
            ctx.clean(&name),
            ctx.clean(&name),
            ctx.clean(&name)
        ));
    }
    let tier = Tier::ThirdPartyUnverified;
    let ((version, tools), before) = start_for_review(ctx, &cfg, tier)?;
    let source = source_of(&cfg);
    // The other servers the model sees next to this one: configured ones
    // and locked ones, with the tool names their approvals recorded.
    let others = plugin_scan::Others {
        servers: ctx
            .specs
            .iter()
            .map(|c| c.name.clone())
            .chain(user_lock.servers.keys().cloned())
            .filter(|n| n != &name)
            .collect(),
        tools: user_lock
            .servers
            .values()
            .flat_map(|e| e.manifest.iter().map(|t| t.name.clone()))
            .collect(),
    };
    let findings = plugin_scan::scan(&tools, &others, &ctx.secrets);
    let hash = plugin_lock::manifest_hash(&tools);
    let expect = format!(
        "add {name} {}",
        hash.trim_start_matches("sha256:").get(..8).unwrap_or("")
    );
    let mut text = ctx.clean(&render_fenced(&name, &version, &tools));
    text.push_str(&format!(
        "\nsource: {}\nprogram hash: {}\nmanifest: {hash}\ntier: {} (sandbox: {})\n",
        ctx.clean(&source),
        if before.is_empty() { "(none)" } else { &before },
        tier.as_str(),
        ctx.clean(&sandbox_status(ctx, &name, is_stdio(&cfg)))
    ));
    if let McpTransport::Stdio { command, args } = &cfg.transport {
        if let Some(w) = unpinned_launcher(command, args) {
            text.push_str(&format!("! {}\n", ctx.clean(&w)));
        }
    }
    text.push_str(&ctx.clean(&plugin_scan::report(&findings)));
    text.push_str(&format!(
        "It will hold no secret and starts only as exactly the source above.\n\
         Type `{expect}` to approve it (anything else cancels): "
    ));
    if (ctx.typed)(&text).trim() != expect {
        return Err("not confirmed: nothing written".into());
    }
    let mut gate = Gate::load(ctx.user.clone(), &ctx.workspace);
    // Exactly what was shown is what is locked: the tools listed above, the
    // program hash taken before the start, no secret grant.
    gate.approve_with(
        &name,
        &source,
        &version,
        &tools,
        Approval {
            integrity: Some(before),
            tier: Some(tier),
            sandbox: profile_used(ctx, &cfg),
            secrets: Some(Vec::new()),
            bundle: None,
        },
    )?;
    Ok(ctx.clean(&format!(
        "locked `{name}` ({hash}) as {}. To use it, configure the same command line \
         (`mcp_server = [\"{spec}\"]` in your config, or `--mcp-server`): the approval is bound to \
         exactly `{source}`.\n",
        tier.as_str()
    )))
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
    let existing = Lockfile::load(
        ctx.user
            .as_deref()
            .ok_or("no tole data dir: no user lock")?,
    )?
    .servers
    .get(name)
    .cloned();
    let tier = tier_of(name, existing.as_ref(), &ctx.first_party);
    let ((version, live), before) = start_for_review(ctx, &cfg, tier)?;
    let hash = plugin_lock::manifest_hash(&live);
    let shown = ctx.clean(&render_manifest(name, &version, &live));
    let q = format!("{shown}\nmanifest {hash}\nApprove `{name}`? [y/N] ");
    // A refusal still shows what was (not) approved, so a script's owner
    // can review it and pass --hash.
    consent(ctx, &hash, expect, &q).map_err(|e| format!("{shown}\nmanifest {hash}\n{e}"))?;
    let mut gate = Gate::load(ctx.user.clone(), &ctx.workspace);
    gate.approve_with(
        name,
        &source_of(&cfg),
        &version,
        &live,
        Approval {
            integrity: Some(before),
            tier: Some(tier),
            sandbox: profile_used(ctx, &cfg),
            secrets: None,
            bundle: None,
        },
    )?;
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
