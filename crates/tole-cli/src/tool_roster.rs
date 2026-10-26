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

use crate::session_host::{PlanEmitter, UpdatePlanTool};
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
        }
    }
}

/// Child-agent depth from `TOLE_AGENT_DEPTH` (0 = not a child).
pub fn agent_depth() -> u32 {
    std::env::var("TOLE_AGENT_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
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

fn add(reg: &mut ToolRegistry, tool: Box<dyn Tool>) -> Result<()> {
    let name = tool.name().to_string();
    reg.register(tool)
        .map_err(|e| anyhow!("registering {name}: {e}"))
}

/// Build the session's registry. Probe-gated tools (uteke, cora, gh,
/// gitea, web search, systemone) are absent when their backend is, never
/// phantoms; a missing binary is reported on stderr.
pub fn build(approver: impl Approver + 'static, roster: Roster) -> Result<ToolRegistry> {
    let interactive = approver.interactive();
    let mut reg = ToolRegistry::with_approver(approver);
    let depth = agent_depth();
    let plan = roster.plan_mode;
    let root = &roster.workspace;
    #[cfg_attr(not(feature = "shell-tools"), allow(unused_variables))]
    let shell_dir = &roster.shell_dir;
    let env = &crate::config_apply::real_env;
    // Child agents (#380): file tools record touches for sibling notices.
    let tracking = crate::sibling::tracking_from_env(env);
    let tracked = |tool: Box<dyn Tool>| crate::sibling::track(tool, tracking.as_ref(), root);

    // Role children (#383) get the handoff tool; everyone else does not.
    if let Some(tool) = crate::handoff::tool_from_env(env) {
        add(&mut reg, Box::new(tool))?;
    }
    #[cfg(feature = "shell-tools")]
    {
        use tole_core::jobs::JobStartTool;
        use tole_core::run_command::RunCommandTool;
        use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};
        if binary_available("uteke") {
            add(&mut reg, Box::new(UtekeRecallTool::new()))?;
            if !plan {
                add(&mut reg, Box::new(UtekeDocumentTool::new(None)))?;
            }
        } else {
            eprintln!("tole: uteke binary not found — uteke_recall/uteke_document disabled");
        }
        if !plan {
            // A child's command tools refuse to spawn tole itself (the
            // spawn-yourself bypass of the depth cap).
            let run_cmd = RunCommandTool::new(shell_dir.clone());
            let job_start = JobStartTool::new(root.clone());
            let (run_cmd, job_start) = if depth >= 1 {
                (
                    run_cmd.in_child_agent_mode(),
                    job_start.in_child_agent_mode(),
                )
            } else {
                (run_cmd, job_start)
            };
            add(&mut reg, Box::new(run_cmd))?;
            add(&mut reg, Box::new(job_start))?;
        }
        add(
            &mut reg,
            Box::new(tole_core::jobs::JobPollTool::new(root.clone())),
        )?;
    }
    add(&mut reg, Box::new(VerifyPackageTool::new()))?;
    // systemone_decide (#172): present iff SYSTEMONE_API_KEY is set.
    if let Some(t) = tole_core::systemone::SystemOneTool::from_env() {
        add(&mut reg, Box::new(t))?;
    }
    // Child agents (#171): parent sessions only, never in plan mode.
    #[cfg(feature = "shell-tools")]
    if let (Some(agents), 0, false) = (&roster.agents, depth, plan) {
        let bin = std::env::current_exe().context("resolving the tole binary for child agents")?;
        let start = tole_core::agents::AgentStartTool::new(bin, root.clone())
            .with_worktrees(agents.worktrees)
            .with_parent_allows(agents.parent_allows.clone());
        add(&mut reg, Box::new(start))?;
        add(
            &mut reg,
            Box::new(tole_core::agents::AgentPollTool::new(root.clone())),
        )?;
    }
    add(&mut reg, tracked(Box::new(ReadFileTool::new(root.clone()))))?;
    // Web tools (#215): fetch always; search only with a backend.
    add(&mut reg, Box::new(tole_core::web::WebFetchTool))?;
    if let Some(search) = tole_core::web::WebSearchTool::from_env() {
        add(&mut reg, Box::new(search))?;
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
            match crate::session_host::detect_github_repo(shell_dir) {
                Some(repo) => add(&mut reg, Box::new(tole_core::gh::GhTool::new(repo)))?,
                None => {
                    eprintln!("tole: no GitHub origin remote detected — gh tool not registered")
                }
            }
            crate::session_host::register_gitea(&mut reg, shell_dir);
            add(
                &mut reg,
                Box::new(tole_core::git::GitTool::new().in_dir(shell_dir.clone())),
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
    #[cfg(feature = "mcp")]
    for cfg in &roster.mcp_servers {
        let names = tole_core::mcp::register_server_tools(&mut reg, cfg);
        if cfg.name == "cora" && !names.is_empty() {
            cora_mcp_ok = true;
        }
        if names.is_empty() {
            eprintln!("tole: mcp[{}]: no tools registered", cfg.name);
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
            eprintln!("tole: cora binary not found — cora_search disabled");
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

    #[test]
    fn binary_available_requires_an_executable() {
        let ws = workspace("bin");
        let plain = ws.join("not-exec");
        std::fs::write(&plain, "x").unwrap();
        assert!(!binary_available(plain.to_str().unwrap()));
        assert!(!binary_available("tole-no-such-binary-xyz"));
    }
}
