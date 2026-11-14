//! The tool roster of a session: which tools every face registers, and
//! under which conditions. One place for the per-tool rules that used to
//! be copied into each face's registry builder and had drifted (plan mode
//! meant three things, `delete_file` had two rules, server sessions lacked
//! sibling tracking and `verify_package`).
//!
//! Faces differ only in the [`Roster`] they pass: the approver, the
//! workspace, plan mode, whether children may be started, the MCP servers
//! and the plan emitter. The Destructive rule stays layered: the roster
//! registers `delete_file` only behind an interactive approver, and the
//! registry and `AllowlistApprover` refuse it on their own as before.

use crate::tools::WriteFileTool;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tole_core::approval::Approver;
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
use tole_core::read_file::ReadFileTool;
use tole_core::tool::{Tool, ToolRegistry};
use tole_core::verify_package::VerifyPackageTool;

/// Child agents a session may start (#171).
#[derive(Debug, Clone, Default)]
pub struct AgentTools {
    /// Start each child in its own git worktree.
    pub worktrees: bool,
    /// The parent's `--allow` patterns, inherited by its children.
    pub parent_allows: Vec<String>,
    /// Delegation (#585): `Some` turns the agent tools into `delegate` /
    /// `delegate_poll` (with `agent_start` / `agent_poll` as aliases),
    /// registered wherever the delegation tree lets this session delegate.
    pub delegation: Option<crate::delegate::Setup>,
}

/// What a face asks of the roster.
pub struct Roster {
    /// File-tool jail root; job logs live under it too.
    pub workspace: PathBuf,
    /// Working directory of the shell tools (`run_command`, `git`, `gh`,
    /// `gitea`).
    pub shell_dir: PathBuf,
    /// Read-only session (#109): Write and Destructive tools are absent,
    /// not merely denied.
    pub plan_mode: bool,
    /// `Some` when the face may start child agents. Ignored in a child
    /// (`TOLE_AGENT_DEPTH >= 1`, the structural depth cap) and in plan
    /// mode.
    pub agents: Option<AgentTools>,
    /// MCP servers whose tools join the session (#74).
    #[cfg(feature = "mcp")]
    pub mcp_servers: Vec<tole_core::mcp::McpServerConfig>,
    /// Plan publishing (#196): only for a transport that renders plans.
    pub plan_emitter: Option<PlanEmitter>,
    /// Where finished background jobs nudge the session (#217).
    pub job_notices: Option<tole_core::interrupt::InterruptQueue>,
    /// The session's id when the face knows it before building the roster
    /// (server faces do); recorded as a skill draft's provenance (#592).
    pub session: Option<String>,
}

impl Roster {
    /// A roster with the same directory for files and shell tools and
    /// nothing optional.
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            shell_dir: workspace.clone(),
            workspace,
            plan_mode: false,
            agents: None,
            #[cfg(feature = "mcp")]
            mcp_servers: Vec::new(),
            plan_emitter: None,
            job_notices: None,
            session: None,
        }
    }
}

/// Child-agent depth from `TOLE_AGENT_DEPTH` (0 = not a child).
pub fn agent_depth() -> u32 {
    tole_core::child_env::depth(&crate::config_apply::real_env)
}

/// True when `name` resolves to an executable file (a path, or a lookup
/// in `PATH`). A non-executable file is not "available": a probe that
/// says yes for an unrunnable binary is a phantom tool by another name.
pub fn binary_available(name: &str) -> bool {
    fn executable(p: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            // Windows: PATH lookups append extensions (".exe").
            p.is_file() || p.with_extension("exe").is_file()
        }
    }
    if name.contains('/') {
        return executable(Path::new(name));
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| executable(&dir.join(name))))
        .unwrap_or(false)
}

/// Env var that turns on the probe notices below (#435).
pub const VERBOSE_ENV: &str = "TOLE_VERBOSE";

/// A probe miss (an optional binary or the GitHub origin is absent): an
/// expected state on most machines, so it is printed only with
/// `TOLE_VERBOSE=1`. `tole doctor` reports the sibling binaries anyway.
#[cfg_attr(not(feature = "shell-tools"), allow(dead_code))] // probes run only with shell-tools
fn probe_notice(msg: &str) {
    if verbose(&crate::config_apply::real_env) {
        eprintln!("tole: {msg}");
    }
}

#[cfg_attr(not(feature = "shell-tools"), allow(dead_code))] // probes run only with shell-tools
fn verbose(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env(VERBOSE_ENV).is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
}

/// The review gate for this session (#534): `Some` only when the policy is
/// not `off` AND MCP servers are attached (without the cora review tools
/// there is no way to get a review, so there is nothing to gate). It starts
/// inactive; [`build`] activates it once both review tools are registered.
/// A malformed `TOLE_REVIEW_GATE` fails the build loudly.
#[cfg(feature = "shell-tools")]
fn review_gate_for(
    roster: &Roster,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<std::sync::Arc<crate::review_gate::Gate>>> {
    use crate::review_gate::{Gate, Policy};
    let policy = Policy::from_env(env).map_err(anyhow::Error::msg)?;
    #[cfg(feature = "mcp")]
    let has_servers = !roster.mcp_servers.is_empty();
    #[cfg(not(feature = "mcp"))]
    let has_servers = false;
    Ok((policy != Policy::Off && has_servers).then(|| Gate::new(policy, roster.shell_dir.clone())))
}

/// Wrap a commit-capable tool in the gate when there is one.
#[cfg(feature = "shell-tools")]
fn gated(
    tool: Box<dyn Tool>,
    kind: crate::review_gate::Kind,
    gate: &Option<std::sync::Arc<crate::review_gate::Gate>>,
) -> Box<dyn Tool> {
    match gate {
        Some(g) => Box::new(crate::review_gate::Gated::new(tool, g.clone(), kind)),
        None => tool,
    }
}

fn add(reg: &mut ToolRegistry, tool: Box<dyn Tool>) -> Result<()> {
    let name = tool.name().to_string();
    reg.register(tool)
        .map_err(|e| anyhow!("registering {name}: {e}"))
}

/// Where a web tool's requests go (#625).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebLane {
    /// A `TOLE_WEB_SEARCH_URL` / `TOLE_WEB_FETCH_URL` backend.
    Generic,
    /// The nginjen backend: `(base URL, API key)`.
    Nginjen(String, String),
    /// nginjen-lite inside tole (`web-builtin`).
    Builtin,
    /// The direct text fetcher (fetch without `web-builtin`).
    Direct,
    /// No tool (search without a backend or `web-builtin`).
    Absent,
}

impl WebLane {
    /// The lane's name for `tole doctor`: never a URL or a key.
    pub fn label(&self, tool: &str) -> &'static str {
        match (self, tool) {
            (WebLane::Generic, _) => "generic backend (TOLE_WEB_*)",
            (WebLane::Nginjen(..), _) => "nginjen backend (NGINJEN_BASE_URL)",
            (WebLane::Builtin, "web_search") => "built-in (DuckDuckGo Lite, then Bing, then Brave)",
            (WebLane::Builtin, _) => "built-in (nginjen-lite, robots.txt honoured)",
            (WebLane::Direct, _) => "direct text fetch",
            (WebLane::Absent, _) => "off (no search backend)",
        }
    }
}

/// The `(web_search, web_fetch)` lanes: a `TOLE_WEB_*` backend wins, then
/// the nginjen backend (both `NGINJEN_BASE_URL` and `NGINJEN_API_KEY`
/// set), then the built-in tools; without `web-builtin`, the direct
/// fetcher and no search.
pub fn web_lanes(env: &dyn Fn(&str) -> Option<String>) -> Result<(WebLane, WebLane), String> {
    let set = |k: &str| env(k).is_some_and(|v| !v.trim().is_empty());
    let nginjen = tole_core::web::nginjen_backend(env)?;
    let pick = |generic: bool, fallback: WebLane| {
        if generic {
            WebLane::Generic
        } else if let Some((base, key)) = &nginjen {
            WebLane::Nginjen(base.clone(), key.clone())
        } else {
            fallback
        }
    };
    let builtin = cfg!(feature = "web-builtin");
    Ok((
        pick(
            set(tole_core::web::SEARCH_URL_ENV),
            if builtin {
                WebLane::Builtin
            } else {
                WebLane::Absent
            },
        ),
        pick(
            set(tole_core::web::FETCH_URL_ENV),
            if builtin {
                WebLane::Builtin
            } else {
                WebLane::Direct
            },
        ),
    ))
}

/// `tole doctor` lines for the web tools: lane names only, never a URL or
/// a key.
pub fn web_doctor_lines(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    match web_lanes(env) {
        Ok((search, fetch)) => vec![
            format!("web_search: {}", search.label("web_search")),
            format!("web_fetch: {}", fetch.label("web_fetch")),
        ],
        Err(e) => vec![format!("web: ERROR {e}")],
    }
}

/// Build the session's registry. Probe-gated tools (uteke, cora, gh,
/// gitea, web search, systemone) are absent when their backend is, never
/// phantoms; a miss is reported on stderr only with `TOLE_VERBOSE=1`.
pub fn build(approver: impl Approver + 'static, roster: Roster) -> Result<ToolRegistry> {
    let interactive = approver.interactive();
    let mut reg = ToolRegistry::with_approver(approver);
    let depth = agent_depth();
    let plan = roster.plan_mode;
    let root = &roster.workspace;
    #[cfg_attr(not(feature = "shell-tools"), allow(unused_variables))]
    let shell_dir = &roster.shell_dir;
    let env = &crate::config_apply::real_env;
    // Known secrets, redacted at every egress sink of these tools (#595).
    let secrets = crate::provider_chain::process_secrets(env);
    // Child agents (#380): file tools record touches for sibling notices.
    let tracking = crate::sibling::tracking_from_env(env);
    let tracked = |tool: Box<dyn Tool>| crate::sibling::track(tool, tracking.as_ref(), root);

    #[cfg(feature = "shell-tools")]
    let review_gate = review_gate_for(&roster, env)?;
    // Role children (#383) get the handoff tool; everyone else does not.
    if let Some(tool) = crate::handoff::tool_from_env(env) {
        add(&mut reg, Box::new(tool))?;
    }
    // Delegated children (#585) hand in their one completion report.
    if let Some(path) = tole_core::child_env::delegation_report(env) {
        add(
            &mut reg,
            Box::new(tole_core::delegation_report::DelegateReportTool::new(path)),
        )?;
    }
    #[cfg(feature = "shell-tools")]
    {
        use tole_core::jobs::JobStartTool;
        use tole_core::run_command::RunCommandTool;
        use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};
        if binary_available("uteke") {
            add(&mut reg, Box::new(UtekeRecallTool::new()))?;
            if !plan {
                add(
                    &mut reg,
                    Box::new(UtekeDocumentTool::new(None).with_secrets(&secrets)),
                )?;
            }
        } else {
            probe_notice("uteke binary not found — uteke_recall/uteke_document disabled");
        }
        if !plan {
            // A child's command tools refuse to spawn tole itself (the
            // spawn-yourself bypass of the depth cap).
            let run_cmd = RunCommandTool::new(shell_dir.clone());
            let mut job_start = JobStartTool::new(root.clone());
            if let Some(q) = roster.job_notices.clone() {
                job_start = job_start.with_completion_notice(q);
            }
            let (run_cmd, job_start) = if depth >= 1 {
                (
                    run_cmd.in_child_agent_mode(),
                    job_start.in_child_agent_mode(),
                )
            } else {
                (run_cmd, job_start)
            };
            use crate::review_gate::Kind;
            add(
                &mut reg,
                gated(Box::new(run_cmd), Kind::Command, &review_gate),
            )?;
            add(
                &mut reg,
                gated(Box::new(job_start), Kind::Command, &review_gate),
            )?;
        }
        add(
            &mut reg,
            Box::new(tole_core::jobs::JobPollTool::new(root.clone())),
        )?;
    }
    add(&mut reg, Box::new(VerifyPackageTool::new()))?;
    // systemone_decide (#172): present iff SYSTEMONE_API_KEY is set.
    if let Some(t) = tole_core::systemone::SystemOneTool::from_env() {
        add(&mut reg, Box::new(t.with_secrets(&secrets)))?;
    }
    // Child agents (#171): parent sessions only, never in plan mode. Not
    // behind this crate's shell-tools gate: every CLI profile had them.
    let delegator = match (&roster.agents, plan) {
        (
            Some(AgentTools {
                delegation: Some(setup),
                worktrees,
                parent_allows,
            }),
            false,
        ) => {
            let bin =
                std::env::current_exe().context("resolving the tole binary for delegation")?;
            crate::delegate::Delegator::for_session(
                bin,
                root.clone(),
                *worktrees,
                parent_allows.clone(),
                setup.clone(),
                &crate::config_apply::real_env,
            )
        }
        _ => None,
    };
    if let Some(host) = delegator {
        let host = std::sync::Arc::new(host);
        for (start, poll) in [("delegate", "delegate_poll"), ("agent_start", "agent_poll")] {
            add(
                &mut reg,
                Box::new(crate::delegate::DelegateTool {
                    name: start,
                    host: host.clone(),
                }),
            )?;
            add(
                &mut reg,
                Box::new(crate::delegate::DelegatePollTool {
                    name: poll,
                    host: host.clone(),
                    inner: tole_core::agents::AgentPollTool::new(host.root().to_path_buf()),
                }),
            )?;
        }
        // The discussion tools (#588) exist exactly where `delegate` does:
        // the same host, so the same depth limit, and they need a provider.
        if let (Some(data), Some(pc)) = (
            host.setup().data_dir.clone(),
            crate::config_apply::provider_config(),
        ) {
            let secrets = crate::provider_chain::process_secrets(&crate::config_apply::real_env);
            let make = crate::discussion_host::factory(pc, secrets.clone());
            let d = crate::discuss_tool::DiscussHost::new(
                host.clone(),
                data.join(tole_core::discussion_log::DISCUSSIONS_DIR),
                secrets,
                make,
            );
            add(
                &mut reg,
                Box::new(crate::discuss_tool::DiscussTool(d.clone())),
            )?;
            add(
                &mut reg,
                Box::new(crate::discuss_tool::DiscussStatusTool(d.clone())),
            )?;
            add(&mut reg, Box::new(crate::discuss_tool::DiscussStopTool(d)))?;
        }
    } else if let (
        Some(
            agents @ AgentTools {
                delegation: None, ..
            },
        ),
        0,
        false,
    ) = (&roster.agents, depth, plan)
    {
        let bin = std::env::current_exe().context("resolving the tole binary for child agents")?;
        let mut start = tole_core::agents::AgentStartTool::new(bin, root.clone())
            .with_worktrees(agents.worktrees)
            .with_parent_allows(agents.parent_allows.clone());
        // The child session gets this session's grant, no more (#598).
        start.opts.keep_secrets = tole_core::secret_release::release().held_grants();
        add(&mut reg, Box::new(start))?;
        add(
            &mut reg,
            Box::new(tole_core::agents::AgentPollTool::new(root.clone())),
        )?;
    }
    // Agent-created cron jobs (#581): a session face's tool, never a cron
    // run's (no agents), a child's or a plan-mode session's. The job is
    // held to this session's own allow list. The scheduler is a
    // shell-tools module, like `tole cron`.
    #[cfg(feature = "shell-tools")]
    if let (Some(agents), 0, false) = (&roster.agents, depth, plan) {
        add(
            &mut reg,
            Box::new(crate::cron::tool::CronCreateTool::new(
                None,
                root.clone(),
                agents.parent_allows.clone(),
            )),
        )?;
    }
    // Skill growth (#592): a session face's tool, never a child's or a
    // plan-mode session's. It writes a draft only; the taint tracker
    // records what this session fetched from outside, at the execute funnel.
    if let (Some(_), 0, false) = (&roster.agents, depth, plan) {
        let taint = tole_core::skill_draft::TaintTracker::default();
        reg.set_taint_tracker(taint.clone());
        #[cfg(feature = "shell-tools")]
        let index: Option<std::sync::Arc<dyn tole_core::skill_index::SkillIndex>> =
            crate::skill_recall::SkillRecall::from_env(None)
                .ok()
                .and_then(|r| r.index);
        #[cfg(not(feature = "shell-tools"))]
        let index = None;
        add(
            &mut reg,
            Box::new(crate::skill_propose::SkillProposeTool::new(
                None,
                taint,
                roster.session.clone(),
                crate::config_apply::provider_config().map(|c| c.model),
                secrets.clone(),
                index,
            )),
        )?;
    }
    add(&mut reg, tracked(Box::new(ReadFileTool::new(root.clone()))))?;
    // The verbatim read of a sensitive file (#598 part b): Risk::Write, so
    // each call needs approval. Registered in plan mode too: it writes
    // nothing.
    add(&mut reg, tracked(Box::new(ReadFileTool::raw(root.clone()))))?;
    // Web tools (#215, #625): one lane per tool, see [`web_lanes`].
    let (search_lane, fetch_lane) =
        web_lanes(&|k: &str| std::env::var(k).ok()).map_err(anyhow::Error::msg)?;
    let allow_private = tole_core::web::allow_private_from_env();
    #[cfg(feature = "web-builtin")]
    let builtin = std::sync::Arc::new(crate::web_builtin::TolHttp::new(&secrets, allow_private));
    match fetch_lane {
        WebLane::Generic => {
            if let Some(fetch) =
                tole_core::web::WebBackendFetchTool::from_env().map_err(anyhow::Error::msg)?
            {
                add(&mut reg, Box::new(fetch.with_secrets(&secrets)))?;
            }
        }
        WebLane::Nginjen(base, key) => add(
            &mut reg,
            Box::new(
                tole_core::web::NginjenFetchTool::new(&base, &key, allow_private)
                    .with_secrets(&secrets),
            ),
        )?,
        #[cfg(feature = "web-builtin")]
        WebLane::Builtin => add(
            &mut reg,
            Box::new(crate::web_builtin::BuiltinFetchTool::new(
                std::sync::Arc::clone(&builtin),
            )),
        )?,
        _ => add(
            &mut reg,
            Box::new(tole_core::web::WebFetchTool::default().with_secrets(&secrets)),
        )?,
    }
    match search_lane {
        // A malformed search config (#436) fails startup loudly.
        WebLane::Generic => {
            if let Some(search) =
                tole_core::web::WebSearchTool::from_env().map_err(anyhow::Error::msg)?
            {
                add(&mut reg, Box::new(search.with_secrets(&secrets)))?;
            }
        }
        WebLane::Nginjen(base, key) => add(
            &mut reg,
            Box::new(tole_core::web::WebSearchTool::nginjen(&base, &key).with_secrets(&secrets)),
        )?,
        #[cfg(feature = "web-builtin")]
        WebLane::Builtin => add(
            &mut reg,
            Box::new(crate::web_builtin::BuiltinSearchTool::new(
                builtin, &secrets,
            )),
        )?,
        _ => {}
    }
    if !plan {
        add(
            &mut reg,
            tracked(Box::new(WriteFileTool::new(root.clone()))),
        )?;
        add(&mut reg, tracked(Box::new(EditFileTool::new(root.clone()))))?;
        #[cfg(feature = "shell-tools")]
        {
            // gh is probe-gated (#254): no detected GitHub origin, no tool.
            match detect_github_repo(shell_dir) {
                Some(repo) => add(&mut reg, Box::new(tole_core::gh::GhTool::new(repo)))?,
                None => probe_notice("no GitHub origin remote detected — gh tool not registered"),
            }
            register_gitea(&mut reg, shell_dir);
            add(
                &mut reg,
                gated(
                    Box::new(tole_core::git::GitTool::new().in_dir(shell_dir.clone())),
                    crate::review_gate::Kind::Git,
                    &review_gate,
                ),
            )?;
        }
        // Destructive: only behind an interactive approver — every call
        // prompts; allowlists and --yes never apply, and a server has no
        // human to ask.
        if interactive {
            add(
                &mut reg,
                tracked(Box::new(DeleteFileTool::new(root.clone()))),
            )?;
        }
    }
    if let Some(emitter) = roster.plan_emitter {
        add(&mut reg, Box::new(UpdatePlanTool::new(emitter)))?;
    }
    // MCP servers (#74): registered last so a slow server never blocks
    // native tool availability.
    #[cfg(feature = "mcp")]
    let mut cora_mcp_ok = false;
    // Servers that advertised `resources` (#213): one ReadOnly `mcp_read`
    // tool, registered only when at least one server offers any.
    #[cfg(feature = "mcp")]
    let mut resource_servers = Vec::new();
    // The rug-pull guard (#599): a server registers only when its live
    // manifest matches its lock entry.
    #[cfg(feature = "mcp")]
    let plugin_gate = crate::plugin_gate::Gate::load(
        tole_core::plugin_lock::Lockfile::user_path(),
        &roster.workspace,
    );
    #[cfg(feature = "mcp")]
    if !roster.mcp_servers.is_empty() {
        for n in &plugin_gate.notes {
            eprintln!("tole: {n}");
        }
    }
    #[cfg(feature = "mcp")]
    for cfg in &roster.mcp_servers {
        // #680, #679: an unapproved or revoked server, or an approved name
        // now pointing at another command, URL or binary, is not started.
        // The lock also binds them after connect (`check`), and the
        // server's process gets only the secrets its approval granted.
        let source = crate::plugin_gate::source_of(cfg);
        if let Some(note) = plugin_gate.refuse_unseen(&cfg.name, &source) {
            eprintln!("tole: mcp[{}]: {note}", cfg.name);
            eprintln!("tole: mcp[{}]: no tools registered", cfg.name);
            continue;
        }
        let admit = |s: &str, v: &str, t: &[tole_core::plugin_lock::ManifestTool]| {
            plugin_gate.check(s, &source, v, t)
        };
        let mut cfg = cfg.clone();
        cfg.secret_grant = plugin_gate.secret_grant(&cfg.name);
        let cfg = &cfg;
        let names = tole_core::mcp::register_server_tools(&mut reg, cfg, &secrets, &admit);
        if cfg.name == "cora" && !names.is_empty() {
            cora_mcp_ok = true;
        }
        if names.is_empty() {
            eprintln!("tole: mcp[{}]: no tools registered", cfg.name);
        }
        // #680: only a server this session admitted is asked for resources.
        if reg.mcp_admitted(&cfg.name) && tole_core::mcp::probe_server_extras(cfg).resources {
            resource_servers.push(cfg.name.clone());
        }
    }
    #[cfg(feature = "mcp")]
    if !resource_servers.is_empty() {
        add(
            &mut reg,
            Box::new(tole_core::mcp::McpReadTool::new(resource_servers)),
        )?;
    }
    // Review gate (#534): on only when the agent can actually get a review.
    #[cfg(all(feature = "shell-tools", feature = "mcp"))]
    if let Some(gate) = &review_gate {
        if reg.get("mcp_cora_cora_review_prepare").is_some()
            && reg.get("mcp_cora_cora_review_finalize").is_some()
        {
            gate.activate();
        }
    }
    #[cfg(not(feature = "mcp"))]
    let cora_mcp_ok = false;
    // Native cora_search (E4): the fallback when the cora MCP toolset did
    // not register — decided by the registration outcome, not config.
    #[cfg(feature = "shell-tools")]
    if !cora_mcp_ok {
        if binary_available("cora") {
            add(
                &mut reg,
                Box::new(tole_core::cora_search::CoraSearchTool::new()),
            )?;
        } else {
            probe_notice("cora binary not found — cora_search disabled");
        }
    }
    #[cfg(not(feature = "shell-tools"))]
    let _ = cora_mcp_ok;
    // MCP tools are always Write: plan mode keeps only ReadOnly tools.
    if plan {
        reg.retain_read_only();
    }
    Ok(reg)
}

/// Register `todo_read` / `todo_write` (#198) with state hydrated from the
/// transcript, so a resumed session restores its last list (#276).
/// `todo_write` is absent in plan mode, like every other Write tool.
pub fn register_todo(
    reg: &mut ToolRegistry,
    entries: &[tole_core::entry::Entry],
    plan_mode: bool,
) -> Result<()> {
    let state = tole_core::todo::TodoState::new();
    state.hydrate(entries);
    add(
        reg,
        Box::new(tole_core::todo::TodoReadTool::new(std::sync::Arc::clone(
            &state,
        ))),
    )?;
    if !plan_mode {
        add(reg, Box::new(tole_core::todo::TodoWriteTool::new(state)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::approval::{AllowlistApprover, ToolRequest, Verdict};
    use tole_core::tool::Risk;

    /// Stands in for a human-backed approver (ACP editor, stdio prompt).
    struct Human;
    impl Approver for Human {
        fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
            Verdict::Deny
        }
        fn interactive(&self) -> bool {
            true
        }
    }

    fn workspace(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-roster-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(reg: &ToolRegistry) -> Vec<String> {
        reg.specs()
            .iter()
            .filter_map(|s| s["function"]["name"].as_str().or(s["name"].as_str()))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn destructive_only_behind_an_interactive_approver() {
        let ws = workspace("destructive");
        let human = build(Human, Roster::new(ws.clone())).unwrap();
        assert!(human.get("delete_file").is_some());
        let server = build(AllowlistApprover::allow_only(vec![]), Roster::new(ws)).unwrap();
        assert!(server.get("delete_file").is_none());
        assert!(server.get("write_file").is_some());
    }

    #[test]
    fn plan_mode_leaves_only_read_only_tools() {
        let ws = workspace("plan");
        let mut roster = Roster::new(ws);
        roster.plan_mode = true;
        roster.agents = Some(AgentTools::default());
        let reg = build(Human, roster).unwrap();
        let tools = names(&reg);
        assert!(!tools.is_empty());
        for name in &tools {
            let risk = reg.get(name).unwrap().risk();
            assert_eq!(risk, Risk::ReadOnly, "{name} is {risk:?} in plan mode");
        }
        assert!(reg.get("read_file").is_some());
        assert!(reg.get("agent_start").is_none());
    }

    #[test]
    fn agent_tools_only_when_the_face_allows_children() {
        let ws = workspace("agents");
        let none = build(Human, Roster::new(ws.clone())).unwrap();
        assert!(none.get("agent_start").is_none());
        let mut roster = Roster::new(ws);
        roster.agents = Some(AgentTools::default());
        let parent = build(Human, roster).unwrap();
        // A test process is never a child (no TOLE_AGENT_DEPTH).
        assert!(parent.get("agent_start").is_some());
        assert!(parent.get("agent_poll").is_some());
    }

    /// #588: the discussion tools exist exactly where `delegate` does (and
    /// with a provider to run on), never with the classic agent tools or none.
    #[test]
    fn discussion_tools_come_only_with_delegate_and_a_provider() {
        let ws = workspace("discuss-roster");
        let all = ["discuss", "discuss_status", "discuss_stop"];
        for roster in [Roster::new(ws.clone()), {
            let mut r = Roster::new(ws.clone());
            r.agents = Some(AgentTools::default());
            r
        }] {
            let reg = build(Human, roster).unwrap();
            assert!(all.iter().all(|t| reg.get(t).is_none()));
        }
        let mut roster = Roster::new(ws.clone());
        roster.agents = Some(AgentTools {
            delegation: Some(crate::delegate::Setup {
                data_dir: Some(ws.join("data")),
                ..Default::default()
            }),
            ..AgentTools::default()
        });
        let reg = build(Human, roster).unwrap();
        assert!(reg.get("delegate").is_some());
        let provider = crate::config_apply::provider_config().is_some();
        assert!(all.iter().all(|t| reg.get(t).is_some() == provider));
        // Plan mode registers no agent tools at all, so no discussion.
        let mut plan = Roster::new(ws);
        plan.agents = Some(AgentTools {
            delegation: Some(crate::delegate::Setup::default()),
            ..AgentTools::default()
        });
        plan.plan_mode = true;
        let reg = build(Human, plan).unwrap();
        assert!(all.iter().all(|t| reg.get(t).is_none()));
    }

    /// #581: `cron_create` is a session face's tool. A cron run (no
    /// agents) and a plan-mode session never have it.
    #[cfg(feature = "shell-tools")]
    #[test]
    fn cron_create_only_where_a_face_allows_agents() {
        let ws = workspace("cron-create");
        assert!(build(Human, Roster::new(ws.clone()))
            .unwrap()
            .get("cron_create")
            .is_none());
        let mut roster = Roster::new(ws.clone());
        roster.agents = Some(AgentTools::default());
        assert!(build(Human, roster).unwrap().get("cron_create").is_some());
        let mut plan = Roster::new(ws);
        plan.agents = Some(AgentTools::default());
        plan.plan_mode = true;
        assert!(build(Human, plan).unwrap().get("cron_create").is_none());
    }

    #[test]
    fn binary_available_requires_an_executable() {
        let ws = workspace("bin");
        let plain = ws.join("not-exec");
        std::fs::write(&plain, "x").unwrap();
        assert!(!binary_available(plain.to_str().unwrap()));
        assert!(!binary_available("tole-no-such-binary-xyz"));
    }

    #[test]
    fn probe_notices_are_off_unless_verbose() {
        let env = |v: Option<&'static str>| {
            move |k: &str| (k == VERBOSE_ENV).then_some(v).flatten().map(String::from)
        };
        assert!(!verbose(&env(None)));
        assert!(!verbose(&env(Some("0"))));
        assert!(!verbose(&env(Some(""))));
        assert!(verbose(&env(Some("1"))));
        assert!(verbose(&env(Some(" true "))));
    }
}

// ---------------------------------------------------------------------------
// Tools registered by the host roster (moved out of `session_host`, #525):
// the plan tool and the gh/gitea probes belong with tool registration, not
// with session hosting.
// ---------------------------------------------------------------------------

/// Callback publishing a plan payload to the session's client (issue
/// #196 phase 4). ACP wires a closure sending the standard `plan`
/// session/update (Termul's PlanPanel renders it, full-replace); the
/// serve/MCP faces pass None — no UI to render plans there.
pub type PlanEmitter = std::sync::Arc<dyn Fn(&serde_json::Value) + Send + Sync>;

/// The model-facing `update_plan` tool (issue #196 phase 4): publishes
/// the execution plan to the host's plan panel. Structurally
/// ReadOnly — it mutates nothing durable and has no side effects
/// beyond the notification. Validation is strict-but-lenient: required
/// `content`, enum-checked `priority`/`status` with spec defaults, so
/// the model self-corrects from the error text on malformed input.
pub struct UpdatePlanTool {
    emitter: PlanEmitter,
}

impl UpdatePlanTool {
    pub fn new(emitter: PlanEmitter) -> Self {
        Self { emitter }
    }
}

impl tole_core::tool::Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn risk(&self) -> tole_core::tool::Risk {
        tole_core::tool::Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Publish or replace the execution plan shown in the user's plan panel.".into()
    }

    fn describe(&self, _input: &serde_json::Value) -> String {
        "publish the execution plan to the user's plan panel".into()
    }
    fn spec(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "entries": {
                    "type": "array",
                    "description": "The FULL plan, replacing any previous one",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "priority": {"enum": ["high", "medium", "low"]},
                            "status": {"enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["entries"]
        }))
    }
    fn execute(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
        let entries = normalize_plan(&input)?;
        (self.emitter)(&entries);
        Ok(serde_json::json!({
            "ok": true,
            "entries": entries.as_array().map(|a| a.len()).unwrap_or(0)
        }))
    }
}

/// Validate + normalize plan entries into the ACP `plan` update shape.
fn normalize_plan(input: &serde_json::Value) -> Result<serde_json::Value, String> {
    let arr = input["entries"]
        .as_array()
        .ok_or("update_plan: 'entries' (array) is required")?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for e in arr {
        let content = e["content"]
            .as_str()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or("update_plan: each entry needs a non-empty 'content' string")?;
        let priority = e["priority"].as_str().unwrap_or("medium");
        if !matches!(priority, "high" | "medium" | "low") {
            return Err(format!(
                "update_plan: invalid priority '{priority}' (high | medium | low)"
            ));
        }
        let status = e["status"].as_str().unwrap_or("pending");
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return Err(format!(
                "update_plan: invalid status '{status}' (pending | in_progress | completed)"
            ));
        }
        out.push(serde_json::json!({
            "content": content, "priority": priority, "status": status
        }));
    }
    Ok(serde_json::Value::Array(out))
}

pub fn detect_github_repo(cwd: &std::path::Path) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd);
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    github_repo_from_remote_url(&String::from_utf8_lossy(&out.stdout))
}

/// Register the `gitea` tool when BOTH probe legs hold: the checkout's
/// origin remote points at a Gitea instance (not GitHub — that's `gh`)
/// and a token env exists (`TOLE_GITEA_TOKEN` or `GITEA_TOKEN`).
/// Absent legs degrade to one warning line, never a phantom tool — the
/// same probe contract as uteke/cora.
pub fn register_gitea(reg: &mut tole_core::tool::ToolRegistry, cwd: &std::path::Path) {
    use tole_core::gitea::{gitea_from_remote, GiteaRemote, GiteaTool};
    let mut cmd = std::process::Command::new("git");
    cmd.args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd);
    let Ok(out) =
        tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let remote = gitea_from_remote(&String::from_utf8_lossy(&out.stdout));
    let (base, repo) = match remote {
        GiteaRemote::Ok { base, repo } => (base, repo),
        GiteaRemote::InsecureHttp { host } => {
            eprintln!(
                "tole: origin is a Gitea remote over plain http ({host}) — refusing to send the \
                 token unencrypted; use an https remote (loopback http is allowed)"
            );
            return;
        }
        GiteaRemote::NotGitea => return,
    };
    let token = std::env::var("TOLE_GITEA_TOKEN")
        .or_else(|_| std::env::var("GITEA_TOKEN"))
        .ok()
        .filter(|t| !t.trim().is_empty());
    let Some(token) = token else {
        eprintln!(
            "tole: origin is a Gitea remote ({repo}) but no TOLE_GITEA_TOKEN/GITEA_TOKEN is set — gitea tool disabled"
        );
        return;
    };
    if let Err(e) = reg.register(Box::new(GiteaTool::new(base, token, repo))) {
        eprintln!("tole: registering gitea: {e}");
    }
}

/// Best-effort `owner/name` from a git remote URL, for gh tool targeting.
///
/// The host must be EXACTLY `github.com` (case-insensitive): the URL is split
/// into authority and path first, so `github.com` appearing inside a longer
/// hostname (`github.com.evil.io`, `notgithub.com`), in the userinfo or in the
/// path of another forge never redirects the gh tool to an attacker-named repo
/// (#352). Handles `scheme://[user[:pass]@]host[:port]/owner/name` and the
/// scp-like `[user@]host:owner/name`.
pub fn github_repo_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        // Strip userinfo (everything up to the LAST '@'), then an optional
        // numeric port.
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = match host_port.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => host_port,
        };
        (host, path)
    } else {
        // scp-like: `[user@]host:path` (no scheme, no `//`).
        let (left, path) = url.split_once(':')?;
        let host = left.rsplit_once('@').map_or(left, |(_, h)| h);
        (host, path)
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let rest = path.trim_start_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    let valid = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if valid(owner) && valid(name) {
        Some(format!("{owner}/{name}"))
    } else {
        None
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn normalize_plan_validates_enums_and_defaults() {
        let plan = normalize_plan(&serde_json::json!({
            "entries": [
                {"content": "step one", "status": "in_progress"},
                {"content": "step two", "priority": "high"},
            ]
        }))
        .unwrap();
        assert_eq!(
            plan,
            serde_json::json!([
                {"content": "step one", "priority": "medium", "status": "in_progress"},
                {"content": "step two", "priority": "high", "status": "pending"},
            ])
        );
    }

    #[test]
    fn normalize_plan_rejects_malformed_input_loudly() {
        // Missing array.
        assert!(normalize_plan(&serde_json::json!({})).is_err());
        // Empty content.
        assert!(normalize_plan(&serde_json::json!({"entries": [{"content": " "}]})).is_err());
        // Out-of-enum values are refused, not silently coerced — the
        // model sees the error text and self-corrects.
        assert!(normalize_plan(
            &serde_json::json!({"entries": [{"content": "x", "priority": "urgent"}]})
        )
        .is_err());
        assert!(normalize_plan(
            &serde_json::json!({"entries": [{"content": "x", "status": "done"}]})
        )
        .is_err());
    }
}
