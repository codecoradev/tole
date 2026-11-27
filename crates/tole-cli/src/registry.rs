//! The tool registries of the CLI faces and the owner's MCP servers.

#[cfg(feature = "shell-tools")]
use crate::flags::refuse_for;
use crate::HostConfig;
#[cfg(not(feature = "shell-tools"))]
use anyhow::bail;
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
use anyhow::Context;
use anyhow::Result;
use tole_cli::approver::{InteractiveApprover, StdioPrompt};
use tole_cli::session::assemble::{resolve_workspace_root, roster_for, FacePolicy, RosterInputs};
use tole_cli::tool_roster;
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
use tole_core::approval::AllowlistApprover;
use tole_core::tool::ToolRegistry;

#[cfg(test)]
mod tests;

pub(crate) fn build_approver(
    allow_patterns: &[String],
    yes: bool,
) -> InteractiveApprover<StdioPrompt> {
    InteractiveApprover::stdio()
        .with_allow_patterns(allow_patterns.to_vec())
        .with_auto_write(yes)
}

/// The MCP client servers of the OWNER for a server session (#946), read once
/// at startup: the `--mcp-server` flag or the user config (the owner typed
/// both). The merged list the CLI uses is NOT read here: it also holds the
/// `cora` auto-preset, which attaches on a PATH lookup and must never happen
/// on a server face. A server specification that comes from the PROJECT
/// config (a file of the served repository) is refused loudly, as hooks from
/// it are: a repository must not choose a process to start in the owner's
/// account.
#[cfg(feature = "shell-tools")]
pub(crate) fn owner_mcp_specs(
    face: &str,
    settings: &tole_cli::config_apply::Settings,
) -> Result<Vec<String>> {
    use tole_cli::config_apply::Source;
    let Some(e) = settings.mcp_server.as_ref() else {
        return Ok(Vec::new());
    };
    match e.source {
        Source::Flag | Source::UserConfig => Ok(e.value.clone()),
        _ => Err(refuse_for(
            &format!(
                "an MCP server specification from the project config is not supported by \
                 `tole {face}` (a repository file must not choose a process to start; set \
                 `mcp_server` in the user config or pass --mcp-server)"
            ),
            Some(Source::Config),
            true,
        )),
    }
}

/// Installs the owner's MCP servers for the server sessions of `face`.
#[cfg(feature = "shell-tools")]
pub(crate) fn install_owner_mcp(
    face: &str,
    settings: &tole_cli::config_apply::Settings,
) -> Result<()> {
    let specs = owner_mcp_specs(face, settings)?;
    tole_cli::session::owner_mcp::set(specs).map_err(anyhow::Error::msg)
}

/// The CLI faces' registry: the shared tool roster behind the
/// interactive approver.
pub(crate) fn build_registry(
    approver: impl tole_core::approval::Approver + 'static,
    host: &HostConfig,
    allow_patterns: &[String],
) -> Result<ToolRegistry> {
    // Hook wiring (run/chat/resume) is shell-tools-gated; without it these
    // flags would be silently ignored — refuse loudly instead.
    #[cfg(not(feature = "shell-tools"))]
    if host.on_pretool_non_empty() || host.on_posttool_non_empty() || !host.on_turnend.is_empty() {
        bail!(
            "--on-pretool/--on-posttool/--on-turnend require the shell-tools feature \
             (this build lacks it); rebuild with default features"
        );
    }
    let roster = roster_for(
        &FacePolicy::CLI,
        RosterInputs {
            workspace: resolve_workspace_root(host.workspace.as_ref())?,
            plan_mode: host.plan_mode,
            allow_patterns,
            agents_worktree: host.agents_worktree,
            delegation: Some(&host.delegation),
            #[cfg(feature = "mcp")]
            mcp_specs: &host.mcp_server,
            #[cfg(not(feature = "mcp"))]
            mcp_specs: &[],
        },
    )?;
    tool_roster::build(approver, roster)
}

/// Server-mode registry: the same hardened tools as `build_registry`,
/// minus two things a server cannot have — the interactive approver
/// (stdin is the MCP protocol) and Destructive tools (structurally
/// refused without one; skipped here with a note). Write tools need
/// explicit `--allow` pre-authorization.
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
pub(crate) fn build_server_registry(
    workspace: Option<&String>,
    allow_patterns: &[String],
    plan_mode: bool,
) -> Result<ToolRegistry> {
    let roster = roster_for(
        &FacePolicy::MCP_SERVER,
        RosterInputs {
            workspace: resolve_workspace_root(workspace)?,
            plan_mode,
            allow_patterns,
            agents_worktree: false,
            delegation: None,
            mcp_specs: &[],
        },
    )?;
    // No child agents and no MCP client tools behind an MCP server face;
    // delete_file (Destructive) is absent behind the allowlist approver.
    let reg = tool_roster::build(
        AllowlistApprover::allow_only(allow_patterns.to_vec()),
        roster,
    )?;
    eprintln!("tole mcp: {} tool(s) registered", reg.specs().len());
    Ok(reg)
}

/// Server-mode registry for the MCP-over-HTTP host (#137): the same
/// hardened tools as stdio MCP (D1); the session tools join separately
/// via RegistryServer::with_extra_tools.
#[cfg(all(feature = "mcp-http", feature = "shell-tools"))]
pub(crate) fn build_server_registry_for_mcp(
    workspace: Option<&String>,
    plan_mode: bool,
) -> Result<ToolRegistry> {
    // Empty allowlist: the session tools carry their own approver per
    // session; registry Write tools stay pre-auth-off (deny by default).
    // `--workspace` roots the server-level file tools exactly like the
    // stdio `tole mcp` face — without it they jailed to the process cwd.
    // Plan mode (#294): the roster's absence semantics, as on every face.
    build_server_registry(workspace, &[], plan_mode)
}

/// D1 (issue #94): serve the registry over MCP stdio. Blocks until the
/// client disconnects.
///
/// Gated as a whole (issue #175): only the gated `Command::Mcp` arm
/// calls it, but an ungated definition still references
/// `build_server_registry` and `tole_core::mcp_server::serve_stdio` —
/// both absent without the `mcp` feature — so the no-mcp profile
/// failed to compile even though every call site was gated.
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
pub(crate) fn mcp_server_command(
    workspace: Option<&String>,
    allow_patterns: &[String],
    plan_mode: bool,
) -> Result<()> {
    // Plan mode (issue #109) applies to server mode too (cora scan-3
    // #9): serve read-only tools only when the operator asked for it.
    let registry = build_server_registry(workspace, allow_patterns, plan_mode)?;
    tokio::runtime::Runtime::new()
        .context("creating tokio runtime")?
        .block_on(tole_core::mcp_server::serve_stdio(registry))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
