//! CLI host for tole-core: arg parsing and session wiring.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use tole_cli::approver::{InteractiveApprover, StdioPrompt};
use tole_cli::tools::WriteFileTool;
use tole_core::approval::AllowlistApprover;
#[cfg(feature = "shell-tools")]
use tole_core::cora_search::CoraSearchTool;
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
#[cfg(feature = "shell-tools")]
use tole_core::gh::GhTool;
#[cfg(feature = "shell-tools")]
use tole_core::git::GitTool;
#[cfg(feature = "shell-tools")]
use tole_core::jobs::{JobPollTool, JobStartTool};
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::read_file::ReadFileTool;
#[cfg(feature = "shell-tools")]
use tole_core::run_command::RunCommandTool;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{resume_turn, run_turn, TurnOutcome, LOOP_TRIP_AFTER};
#[cfg(feature = "shell-tools")]
use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};

/// Where sessions live unless the user overrides it.
const DEFAULT_SESSIONS_DIR: &str = ".tole/sessions";

/// Session id → path (`<dir>/<id>.jsonl`).
fn session_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// Session id validity: `[a-z0-9-]` — also prevents `../` traversal in
/// the sessions dir.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `tole` — a durable agent loop with approval gates.
#[derive(Parser)]
#[command(name = "tole", version, about, long_about = None)]
struct Cli {
    /// Sessions directory (default: .tole/sessions under the cwd).
    #[arg(short, long, global = true)]
    sessions_dir: Option<String>,

    /// Root directory for the file tools (read_file/write_file/edit_file/
    /// delete_file). Defaults to the current directory. run_command/git
    /// stay bound to the process cwd regardless.
    #[arg(long, global = true)]
    workspace: Option<String>,

    /// Register an external MCP server over stdio: name=command [args...].
    /// Repeatable. Every MCP tool joins the registry as Risk::Write and
    /// goes through the normal approval gate. Requires the `mcp` feature.
    #[cfg(feature = "mcp")]
    #[arg(long, global = true)]
    mcp_server: Vec<String>,

    /// Skip the auto-detected MCP presets (currently: the local `cora mcp`
    /// server attached when the `cora` binary is on PATH). Explicit
    /// --mcp-server flags are unaffected. Requires the `mcp` feature.
    #[cfg(feature = "mcp")]
    #[arg(long, global = true)]
    no_auto_mcp: bool,

    /// Harness memory loop backend (run/chat): on the first turn of a
    /// fresh session, memories relevant to the prompt are recalled from
    /// the owner's store and injected into the message; when the session
    /// settles, a compact summary is stored back. Currently `uteke`
    /// (namespace: repo-<dir>, override: TOLE_MEMORY_NAMESPACE). Falls
    /// back to the TOLE_MEMORY env.
    #[cfg(feature = "shell-tools")]
    #[arg(long, global = true)]
    memory: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a new session and run one user turn.
    Run {
        /// The user prompt for this turn.
        prompt: String,

        /// System prompt for this session (highest priority; else
        /// TOLE_SYSTEM_PROMPT env; else none). Pinned in the session
        /// header — resume re-applies exactly this.
        #[arg(long)]
        system: Option<String>,

        /// Auto-allow Write tools matching this glob pattern without
        /// asking (e.g. --allow 'write_*'). Patterns match tool names
        /// only: an equivalent-effect tool such as run_command stays
        /// separately gated, and Destructive tools are never
        /// auto-allowed. Repeatable.
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Auto-allow every Write call without prompting (heads-up
        /// mode. Destructive tools still prompt).
        #[arg(long)]
        yes: bool,
    },
    /// Resume an interrupted session. With an optional
    /// PROMPT, appends it as a new user message and runs one full turn
    /// (issue #55): headless flows can continue a mission without a
    /// separate `run` session. Without PROMPT, behaves as before:
    /// approvals-only recovery of a mid-flight turn.
    Resume {
        /// Session id to resume.
        id: String,

        /// Optional new user message for the resumed session.
        prompt: Option<String>,

        /// Same semantics as `run --allow`.
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Same semantics as `run --yes`.
        #[arg(long)]
        yes: bool,
    },
    /// List sessions in the sessions dir, newest first.
    Sessions,

    /// Show durable state of a session.
    Status {
        /// Session id to inspect.
        id: String,
    },
    /// Serve tole's tools over MCP stdio (issue #94): ReadOnly tools are
    /// always callable; Write tools require --allow patterns (server mode
    /// has no stdin human — stdin IS the protocol); Destructive tools are
    /// structurally absent.
    #[cfg(all(feature = "mcp", feature = "shell-tools"))]
    Mcp {
        /// Same semantics as `run --allow` (Write pre-authorization).
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Root directory for the file tools (same as run).
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Interactive multi-turn chat on one durable session.
    Chat {
        /// System prompt for a fresh session (ignored when resuming —
        /// the header-pinned prompt wins). Highest priority; else
        /// TOLE_SYSTEM_PROMPT env; else none.
        #[arg(long)]
        system: Option<String>,
        /// Resume an existing session by id instead of starting new.
        #[arg(long)]
        resume: Option<String>,

        /// Resume the most recently modified session in the sessions dir.
        #[arg(long, conflicts_with = "resume")]
        last: bool,

        /// Same semantics as `run --allow`.
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Same semantics as `run --yes`.
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli) {
        eprintln!("tole: {e:#}");
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    let sessions_dir = PathBuf::from(
        cli.sessions_dir
            .clone()
            .unwrap_or_else(|| DEFAULT_SESSIONS_DIR.to_string()),
    );
    #[cfg(feature = "mcp")]
    let mcp_specs = merge_mcp_specs(&cli.mcp_server, cli.no_auto_mcp, auto_mcp_specs());
    let host = HostConfig {
        workspace: cli.workspace.clone(),
        #[cfg(feature = "mcp")]
        mcp_server: mcp_specs,
        #[cfg(feature = "shell-tools")]
        memory: resolve_memory(cli.memory.as_ref())?,
        #[cfg(not(feature = "shell-tools"))]
        memory: None,
    };
    match cli.command {
        Command::Run {
            prompt,
            system,
            allow_patterns,
            yes,
        } => run_command(
            &sessions_dir,
            &prompt,
            system.as_deref(),
            &allow_patterns,
            yes,
            &host,
        ),
        Command::Resume {
            id,
            prompt,
            allow_patterns,
            yes,
        } => resume_command(
            &sessions_dir,
            &id,
            prompt.as_deref(),
            &allow_patterns,
            yes,
            &host,
        ),
        #[cfg(all(feature = "mcp", feature = "shell-tools"))]
        Command::Mcp {
            allow_patterns,
            workspace,
        } => mcp_server_command(workspace.as_ref(), &allow_patterns),
        Command::Sessions => sessions_command(&sessions_dir),
        Command::Status { id } => status_command(&sessions_dir, &id),
        Command::Chat {
            system,
            resume,
            last,
            allow_patterns,
            yes,
        } => chat_command(
            &sessions_dir,
            system.as_deref(),
            resume,
            last,
            &allow_patterns,
            yes,
            &host,
        ),
    }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/// Host knobs shared by the command fns: the file-tools jail root, the
/// MCP server list, the memory backend. Bundled so per-command
/// signatures stop growing with every feature.
struct HostConfig {
    workspace: Option<String>,
    #[cfg(feature = "mcp")]
    mcp_server: Vec<String>,
    #[cfg(feature = "shell-tools")]
    memory: Option<tole_core::memory::MemoryConfig>,
}

#[cfg(feature = "shell-tools")]
impl HostConfig {
    /// Pre-turn recall injection (memory loop): the returned prompt is
    /// what the provider sees and what the durable log records. Any
    /// failure degrades to the bare prompt — memory is an enhancement,
    /// never a dependency.
    fn inject_memory(&self, prompt: &str) -> String {
        let Some(mem) = self.memory.as_ref() else {
            return prompt.to_string();
        };
        match tole_core::memory::recall_block(mem, prompt) {
            Ok(block) if !block.is_empty() => {
                eprintln!(
                    "tole: memory: recalled context injected ({})",
                    mem.namespace
                );
                format!("{prompt}{block}")
            }
            Ok(_) => prompt.to_string(),
            Err(e) => {
                eprintln!("tole: memory recall failed (continuing without): {e}");
                prompt.to_string()
            }
        }
    }

    /// Post-session summary (memory loop): best-effort, stderr on
    /// failure, the session itself is never affected.
    fn remember(&self, session_id: &str, first_prompt: &str, last_answer: &str) {
        let Some(mem) = self.memory.as_ref() else {
            return;
        };
        match tole_core::memory::remember_session(mem, session_id, first_prompt, last_answer) {
            Ok(_) => eprintln!("tole: memory: session summary stored in {}", mem.namespace),
            Err(e) => eprintln!("tole: memory remember failed (session unaffected): {e}"),
        }
    }
}

/// Memory backend resolution: the `--memory` flag wins over the
/// `TOLE_MEMORY` env; `uteke` is the only backend. The namespace follows
/// the ecosystem `repo-<dir>` convention unless `TOLE_MEMORY_NAMESPACE`
/// overrides it. A missing uteke binary degrades to a warning + no-op
/// (the same probe contract as the uteke tools).
#[cfg(feature = "shell-tools")]
fn resolve_memory(flag: Option<&String>) -> Result<Option<tole_core::memory::MemoryConfig>> {
    let chosen = flag
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("TOLE_MEMORY")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        });
    let Some(backend) = chosen else {
        return Ok(None);
    };
    if backend != "uteke" {
        anyhow::bail!("unsupported memory backend {backend:?} (only 'uteke' is available)");
    }
    if !binary_available("uteke") {
        eprintln!(
            "tole: memory backend 'uteke' requested but the binary is missing — memory loop disabled"
        );
        return Ok(None);
    }
    let mut cfg = tole_core::memory::MemoryConfig::for_cwd("uteke", &std::env::current_dir()?);
    if let Ok(ns) = std::env::var("TOLE_MEMORY_NAMESPACE") {
        let ns = ns.trim();
        if !ns.is_empty() {
            cfg.namespace = tole_core::memory::sanitize_namespace(ns);
        }
    }
    Ok(Some(cfg))
}

fn build_approver(allow_patterns: &[String], yes: bool) -> InteractiveApprover<StdioPrompt> {
    InteractiveApprover::stdio()
        .with_allow_patterns(allow_patterns.to_vec())
        .with_auto_write(yes)
}

/// Auto-detected MCP presets: server specs attached with zero
/// configuration. Currently just the local `cora mcp` server when the
/// `cora` binary is on PATH — the full code-intel surface (callers,
/// impact, dead-code, review) without a manual flag.
#[cfg(feature = "mcp")]
fn auto_mcp_specs() -> Vec<String> {
    if binary_available("cora") {
        vec!["cora=cora mcp".to_string()]
    } else {
        Vec::new()
    }
}

/// Merge explicit `--mcp-server` specs over the auto-detected presets.
/// An explicit spec with the same server name wins — the user said what
/// they meant. `auto` is injected so the merge is unit-testable without
/// depending on which binaries this machine happens to have.
#[cfg(feature = "mcp")]
fn merge_mcp_specs(explicit: &[String], no_auto_mcp: bool, auto: Vec<String>) -> Vec<String> {
    let mut specs = explicit.to_vec();
    if !no_auto_mcp {
        let explicit_names: Vec<&str> = explicit
            .iter()
            .map(|s| s.split('=').next().unwrap_or_default())
            .collect();
        for spec in auto {
            let name = spec.split('=').next().unwrap_or_default();
            if !explicit_names.contains(&name) {
                specs.push(spec);
            }
        }
    }
    specs
}

/// B4 startup probing: a binary exists on PATH (or is an executable
/// absolute path). Used to skip CLI-backed tools whose engine is not
/// installed, instead of registering phantom tools that fail on every
/// call.
fn binary_available(name: &str) -> bool {
    fn executable(p: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // is_file() alone accepts non-executable files; a probe that
            // says "available" for an unrunnable binary is a phantom tool
            // by another name (CodeCora scan 2026-09-18).
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            // Windows: PATH lookups append extensions (".exe"); probe the
            // obvious one rather than declaring a runnable binary absent.
            p.is_file() || p.with_extension("exe").is_file()
        }
    }
    if name.contains('/') {
        return executable(Path::new(name));
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            if executable(&dir.join(name)) {
                return true;
            }
        }
    }
    false
}

/// Resolve the file-tools jail root: `--workspace <dir>` when given,
/// otherwise the process cwd (issue #57). The directory must exist — a
/// typo silently widening the jail to cwd would be worse than failing.
/// In tools.rs (not main.rs) so a bin-only crate can unit-test it.
pub fn resolve_workspace_root(explicit: Option<&String>) -> Result<PathBuf> {
    match explicit {
        Some(ws) => {
            let p = PathBuf::from(ws);
            let canon = p
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("workspace directory {}: {e}", p.display()))?;
            if !canon.is_dir() {
                anyhow::bail!("workspace is not a directory: {}", canon.display());
            }
            Ok(canon)
        }
        None => std::env::current_dir().context("resolving cwd"),
    }
}

/// Best-effort `owner/name` from a git remote URL, for gh tool
/// targeting (CodeCora dogfood finding 2026-09-18: a hardcoded repo
/// made `gh` target the wrong project outside this checkout).
fn github_repo_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let idx = url.to_ascii_lowercase().find("github.com")?;
    let rest = &url[idx + "github.com".len()..];
    let rest = rest.trim_start_matches(['/', ':']);
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

/// Ask the checkout itself which GitHub repo it belongs to ( Falls back
/// to None → callers keep the tole default).
#[cfg(feature = "shell-tools")]
fn detect_github_repo(cwd: &Path) -> Option<String> {
    use std::process::Command;
    let mut cmd = Command::new("git");
    cmd.args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd);
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
        .ok()?;
    if !out.status.success() {
        return None;
    }
    github_repo_from_remote_url(&String::from_utf8_lossy(&out.stdout))
}

fn build_registry(
    approver: InteractiveApprover<StdioPrompt>,
    workspace: Option<&String>,
    #[cfg(feature = "mcp")] mcp_servers: &[tole_core::mcp::McpServerConfig],
) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::with_approver(approver);
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let file_root = resolve_workspace_root(workspace)?;
    // Uteke first-class (B4): recall (read) + document (write), behind
    // startup probing — a missing uteke binary degrades to a warning,
    // not phantom tools.
    #[cfg(feature = "shell-tools")]
    if binary_available("uteke") {
        reg.register(Box::new(UtekeRecallTool::new()))
            .map_err(|e| anyhow::anyhow!("registering uteke_recall: {e}"))?;
        reg.register(Box::new(UtekeDocumentTool::new(None)))
            .map_err(|e| anyhow::anyhow!("registering uteke_document: {e}"))?;
    } else {
        eprintln!("tole: uteke binary not found — uteke_recall/uteke_document disabled");
    }
    // Generic dynamic command (B4): argv-split, cwd-jailed, Risk::Write.
    #[cfg(feature = "shell-tools")]
    reg.register(Box::new(RunCommandTool::new(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering run_command: {e}"))?;
    // Long-running jobs (#59): detached spawn + poll, logs inside the
    // file-tools workspace so read_file can reach the full log.
    #[cfg(feature = "shell-tools")]
    reg.register(Box::new(JobStartTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering job_start: {e}"))?;
    #[cfg(feature = "shell-tools")]
    reg.register(Box::new(JobPollTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering job_poll: {e}"))?;
    reg.register(Box::new(ReadFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering read_file: {e}"))?;
    // Write tools: gated per call. The jail root is the workspace.
    reg.register(Box::new(WriteFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering write_file: {e}"))?;
    reg.register(Box::new(EditFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering edit_file: {e}"))?;
    #[cfg(feature = "shell-tools")]
    {
        // Target the checkout's own GitHub repo when detectable — a
        // hardcoded one made `gh` act on the wrong project (CodeCora
        // dogfood finding 2026-09-18).
        let gh_repo = detect_github_repo(&cwd).unwrap_or_else(|| "codecoradev/tole".into());
        reg.register(Box::new(GhTool::new(gh_repo)))
            .map_err(|e| anyhow::anyhow!("registering gh: {e}"))?;
    }
    // Light git: status/diff/add/commit (push stays human).
    #[cfg(feature = "shell-tools")]
    reg.register(Box::new(GitTool::new().in_dir(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering git: {e}"))?;
    // Destructive tools: interactive-approver-only registration; every
    // call prompts — allowlists and --yes never apply (PRD risk table).
    reg.register(Box::new(DeleteFileTool::new(file_root)))
        .map_err(|e| anyhow::anyhow!("registering delete_file: {e}"))?;
    // MCP servers (#74): registered last so a slow server never blocks
    // native tool availability; per-call approval applies as usual.
    #[cfg(feature = "mcp")]
    let mut cora_mcp_tools = 0usize;
    #[cfg(feature = "mcp")]
    for cfg in mcp_servers {
        let names = tole_core::mcp::register_server_tools(&mut reg, cfg);
        #[cfg(feature = "mcp")]
        if cfg.name == "cora" {
            cora_mcp_tools = names.len();
        }
        if names.is_empty() {
            eprintln!("tole: mcp[{}]: no tools registered", cfg.name);
        }
    }
    // Native cora_search (E4): single-tool fallback, registered only when
    // the cora MCP surface did NOT materialize — decision is based on the
    // REGISTRATION OUTCOME, not config presence: a cora binary whose MCP
    // server fails (handshake, old version) must degrade to the native
    // tool instead of silently losing all code-intel (CodeCora finding).
    // The full MCP toolset supersedes the fallback when it registered.
    #[cfg(feature = "shell-tools")]
    {
        #[cfg(feature = "mcp")]
        let cora_mcp_ok = cora_mcp_tools > 0;
        #[cfg(not(feature = "mcp"))]
        let cora_mcp_ok = false;
        if !cora_mcp_ok {
            if binary_available("cora") {
                reg.register(Box::new(CoraSearchTool::new()))
                    .map_err(|e| anyhow::anyhow!("registering cora_search: {e}"))?;
            } else {
                eprintln!("tole: cora binary not found — cora_search disabled");
            }
        }
    }
    Ok(reg)
}

/// Server-mode registry: the same hardened tools as `build_registry`,
/// minus two things a server cannot have — the interactive approver
/// (stdin is the MCP protocol) and Destructive tools (structurally
/// refused without one; skipped here with a note). Write tools need
/// explicit `--allow` pre-authorization.
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
fn build_server_registry(
    workspace: Option<&String>,
    allow_patterns: &[String],
) -> Result<ToolRegistry> {
    let mut reg =
        ToolRegistry::with_approver(AllowlistApprover::allow_only(allow_patterns.to_vec()));
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let file_root = resolve_workspace_root(workspace)?;
    let count = |reg: &ToolRegistry| reg.specs().len();

    #[cfg(feature = "shell-tools")]
    {
        if binary_available("cora") {
            reg.register(Box::new(CoraSearchTool::new()))
                .map_err(|e| anyhow::anyhow!("registering cora_search: {e}"))?;
        }
        if binary_available("uteke") {
            reg.register(Box::new(UtekeRecallTool::new()))
                .map_err(|e| anyhow::anyhow!("registering uteke_recall: {e}"))?;
            reg.register(Box::new(UtekeDocumentTool::new(None)))
                .map_err(|e| anyhow::anyhow!("registering uteke_document: {e}"))?;
        }
        reg.register(Box::new(RunCommandTool::new(cwd.clone())))
            .map_err(|e| anyhow::anyhow!("registering run_command: {e}"))?;
        reg.register(Box::new(JobStartTool::new(file_root.clone())))
            .map_err(|e| anyhow::anyhow!("registering job_start: {e}"))?;
        reg.register(Box::new(JobPollTool::new(file_root.clone())))
            .map_err(|e| anyhow::anyhow!("registering job_poll: {e}"))?;
    }
    reg.register(Box::new(ReadFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering read_file: {e}"))?;
    reg.register(Box::new(WriteFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering write_file: {e}"))?;
    reg.register(Box::new(EditFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering edit_file: {e}"))?;
    #[cfg(feature = "shell-tools")]
    {
        let gh_repo = detect_github_repo(&cwd).unwrap_or_else(|| "codecoradev/tole".into());
        reg.register(Box::new(GhTool::new(gh_repo)))
            .map_err(|e| anyhow::anyhow!("registering gh: {e}"))?;
        reg.register(Box::new(GitTool::new().in_dir(cwd.clone())))
            .map_err(|e| anyhow::anyhow!("registering git: {e}"))?;
    }
    // delete_file (Destructive) is deliberately NOT registered: behind a
    // non-interactive approver the registry refuses it structurally.
    eprintln!("tole mcp: {} tool(s) registered", count(&reg));
    Ok(reg)
}

/// D1 (issue #94): serve the registry over MCP stdio. Blocks until the
/// client disconnects.
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
fn mcp_server_command(workspace: Option<&String>, allow_patterns: &[String]) -> Result<()> {
    let registry = build_server_registry(workspace, allow_patterns)?;
    tokio::runtime::Runtime::new()
        .context("creating tokio runtime")?
        .block_on(tole_core::mcp_server::serve_stdio(registry))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn run_command(
    sessions_dir: &Path,
    prompt: &str,
    system: Option<&str>,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
) -> Result<()> {
    let cfg = OpenAiConfig::from_env().context(
        "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY \
         (or the OPENAI_* equivalents)",
    )?;
    // Build everything that can fail BEFORE the session file exists, so a
    // failed startup does not leave a stray empty session polluting
    // `sessions` / `--last` (CodeCora scan 2026-09-18).
    #[cfg(feature = "mcp")]
    let mcp_cfgs: Vec<tole_core::mcp::McpServerConfig> = host
        .mcp_server
        .iter()
        .map(|s| tole_core::mcp::McpServerConfig::parse(s))
        .collect::<Result<Vec<_>, String>>()
        .map_err(anyhow::Error::msg)?;
    #[cfg(feature = "mcp")]
    let registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        &mcp_cfgs,
    )?;
    #[cfg(not(feature = "mcp"))]
    let registry = build_registry(build_approver(allow_patterns, yes), host.workspace.as_ref())?;

    let session_id = new_session_id();
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    let system_prompt = system
        .map(str::to_string)
        .or_else(resolve_system_prompt)
        .or_else(|| Some(default_system_prompt().to_string()));
    let mut storage =
        JsonlStorage::create_with(sessions_dir, &session_id, None, system_prompt.as_deref())
            .with_context(|| format!("creating session {session_id}"))?;
    println!("session: {session_id}");

    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    if let Some(sys) = system_prompt.as_deref() {
        provider = provider.with_system_prompt(sys);
    }
    // Memory loop, pre-turn: recalled context rides inside the first
    // user message — the durable log stores exactly what was sent. The
    // summary remembers the PRE-injection prompt: storing the injected
    // block would echo recalled memory back into the store (amplification).
    #[cfg(feature = "shell-tools")]
    let (raw_prompt, prompt) = (prompt.to_string(), host.inject_memory(prompt));
    #[cfg(not(feature = "shell-tools"))]
    let prompt = prompt.to_string();
    let outcome = run_turn(&mut storage, &mut provider, &registry, &prompt)?;
    // Memory loop, post-session: a settled Final turn leaves a compact
    // summary behind for the next session's recall.
    #[cfg(feature = "shell-tools")]
    if let TurnOutcome::Final { text } = &outcome {
        host.remember(&session_id, &raw_prompt, text);
    }
    report_outcome(&session_id, outcome);
    Ok(())
}

fn resume_command(
    sessions_dir: &Path,
    id: &str,
    prompt: Option<&str>,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
) -> Result<()> {
    if !valid_session_id(id) {
        anyhow::bail!("invalid session id {id:?} (allowed: [a-z0-9-], max 64)");
    }
    let path = session_path(sessions_dir, id);
    if !path.exists() {
        anyhow::bail!("session {id} not found at {}", path.display());
    }
    let cfg = OpenAiConfig::from_env().context(
        "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY \
         (or the OPENAI_* equivalents)",
    )?;
    let mut storage = JsonlStorage::open(&path).context("replaying session log")?;

    #[cfg(feature = "mcp")]
    let mcp_cfgs: Vec<tole_core::mcp::McpServerConfig> = host
        .mcp_server
        .iter()
        .map(|s| tole_core::mcp::McpServerConfig::parse(s))
        .collect::<Result<Vec<_>, String>>()
        .map_err(anyhow::Error::msg)?;
    #[cfg(feature = "mcp")]
    let registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        &mcp_cfgs,
    )?;
    #[cfg(not(feature = "mcp"))]
    let registry = build_registry(build_approver(allow_patterns, yes), host.workspace.as_ref())?;
    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    // B2: the system prompt is pinned in the session header — resume
    // re-applies exactly what the session was created with (never the
    // ambient env, which may have changed since).
    if let Some(sys) = storage.system_prompt() {
        provider = provider.with_system_prompt(sys);
    }
    let outcome = match prompt {
        Some(text) if !text.trim().is_empty() => {
            // New instructions on a settled session (issue #55): the
            // machine accepts a user message at a turn boundary, so
            // reuse run_turn on the resumed storage instead of the
            // approvals-only resume protocol.
            let outcome = run_turn(&mut storage, &mut provider, &registry, text)?;
            // Memory loop (CodeCora scan 2026-09-18): a settled resumed
            // turn leaves a summary like `run` does — the continuation is
            // its own durable event. No recall injection here: the resumed
            // session already carries its context.
            #[cfg(feature = "shell-tools")]
            if let TurnOutcome::Final { text: answer } = &outcome {
                host.remember(id, text, answer);
            }
            outcome
        }
        _ => resume_turn(&mut storage, &mut provider, &registry)?,
    };
    report_outcome(id, outcome);
    Ok(())
}

fn status_command(sessions_dir: &Path, id: &str) -> Result<()> {
    if !valid_session_id(id) {
        anyhow::bail!("invalid session id {id:?} (allowed: [a-z0-9-], max 64)");
    }
    let path = session_path(sessions_dir, id);
    if !path.exists() {
        anyhow::bail!("session {id} not found at {}", path.display());
    }
    let storage = JsonlStorage::open(&path).context("replaying session log")?;
    println!("session: {id}");
    println!("entries: {}", storage.entries().len());
    println!("pc:      {:?}", storage.state().pc);
    println!("seq:     {}", storage.state().seq);
    // B3: orientation fields — turns (user messages), usage totals, and
    // whether a system prompt is pinned.
    let turns: usize = storage
        .entries()
        .iter()
        .filter(|e| e.payload.get("role") == Some(&serde_json::json!("user")))
        .count();
    println!("turns:   {turns} (user messages)");
    println!(
        "system:  {}",
        match storage.system_prompt() {
            Some(_) => "pinned (see header)".to_string(),
            None => "-".to_string(),
        }
    );
    let usage = storage.usages();
    let prompt_tokens: u64 = usage
        .iter()
        .filter_map(|u| u.usage.get("prompt_tokens").and_then(|v| v.as_u64()))
        .sum();
    let completion_tokens: u64 = usage
        .iter()
        .filter_map(|u| u.usage.get("completion_tokens").and_then(|v| v.as_u64()))
        .sum();
    let cost: f64 = usage.iter().filter_map(|u| u.cost_usd).sum::<f64>();
    // `-0.0` renders as "-0.0000" (sum of no records is 0.0, but be
    // explicit: a negative-zero cost display would look like a bug).
    let cost = if cost == 0.0 { 0.0 } else { cost };
    println!("usage:   {prompt_tokens} in / {completion_tokens} out tokens, ${cost:.4} USD");
    Ok(())
}

/// B3: list every session in the dir, newest first, one line each.
fn sessions_command(sessions_dir: &Path) -> Result<()> {
    if !sessions_dir.exists() {
        println!(
            "no sessions in {} (dir does not exist) — start one with: tole chat",
            sessions_dir.display()
        );
        return Ok(());
    }
    // (epoch_secs, id, pc, seq, turns) — epoch secs first so a plain
    // sort_by_key ascending gives newest-first via Reverse.
    let mut rows: Vec<(u64, String, String, u64, usize)> = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // strip_suffix, not trim_end_matches: a (weird but possible)
        // "x.jsonl.jsonl" would otherwise yield the unusable id "x.jsonl"
        // (CodeCora scan 2026-09-18).
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !valid_session_id(stem) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = stem.to_string();
        // Read-only peek: replay the file to surface pc + turn count.
        let s = match JsonlStorage::open(entry.path()) {
            Ok(s) => s,
            Err(_) => continue, // unreadable/corrupt: skip, don't fail the listing
        };
        let turns: usize = s
            .entries()
            .iter()
            .filter(|e| e.payload.get("role") == Some(&serde_json::json!("user")))
            .count();
        rows.push((
            mtime,
            id,
            format!("{:?}", s.state().pc),
            s.state().seq,
            turns,
        ));
    }
    if rows.is_empty() {
        println!("no sessions in {}", sessions_dir.display());
        return Ok(());
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0)); // newest first
    println!(
        "{:<26} {:<12} {:>5} {:>6}  mtime",
        "session", "pc", "seq", "turns"
    );
    for row in &rows {
        let mtime = fmt_mtime(std::time::UNIX_EPOCH + std::time::Duration::from_secs(row.0));
        println!(
            "{:<26} {:<12} {:>5} {:>6}  {mtime}",
            row.1, row.2, row.3, row.4
        );
    }
    Ok(())
}

/// `YYYY-mm-dd HH:MM` in UTC — stable, no chrono dep.
fn fmt_mtime(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // civil-from-days (Howard Hinnant's algorithm) — no chrono.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}")
}

// ---------------------------------------------------------------------------
// Chat (B1)
// ---------------------------------------------------------------------------

/// Most recently modified session id in `dir` (None when empty).
fn latest_session_id(dir: &Path) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !valid_session_id(stem) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, name.trim_end_matches(".jsonl").to_string()));
        }
    }
    best.map(|(_, id)| id)
}

/// The B1 REPL: one durable session, many turns. Every user line becomes a
/// turn; mid-flight states (denial, provider failure) are resolved by
/// `resume_turn` on the next line, keeping the conversation alive without
/// losing durable context. Ctrl-C / EOF exit cleanly — every commit is
/// already durable, `tole chat --resume <id>` picks the thread back up.
fn chat_command(
    sessions_dir: &Path,
    system: Option<&str>,
    resume: Option<String>,
    last: bool,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
) -> Result<()> {
    use std::io::{BufRead, Write};

    let cfg = OpenAiConfig::from_env().context(
        "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY \
         (or the OPENAI_* equivalents)",
    )?;

    // Resolve the session: explicit id, --last, or fresh.
    let (session_id, fresh) = if let Some(id) = resume {
        if !valid_session_id(&id) {
            anyhow::bail!("invalid session id {id:?} (allowed: [a-z0-9-], max 64)");
        }
        let path = session_path(sessions_dir, &id);
        if !path.exists() {
            anyhow::bail!("session {id} not found at {}", path.display());
        }
        (id, false)
    } else if last {
        let Some(id) = latest_session_id(sessions_dir) else {
            anyhow::bail!("no sessions found in {}", sessions_dir.display());
        };
        (id, false)
    } else {
        (new_session_id(), true)
    };

    // Build everything that can fail BEFORE the fresh session file is
    // created, so a failed startup does not leave a stray empty session
    // polluting `sessions` / `--last` (CodeCora scan 2026-09-18).
    #[cfg(feature = "mcp")]
    let mcp_cfgs: Vec<tole_core::mcp::McpServerConfig> = host
        .mcp_server
        .iter()
        .map(|s| tole_core::mcp::McpServerConfig::parse(s))
        .collect::<Result<Vec<_>, String>>()
        .map_err(anyhow::Error::msg)?;
    #[cfg(feature = "mcp")]
    let registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        &mcp_cfgs,
    )?;
    #[cfg(not(feature = "mcp"))]
    let registry = build_registry(build_approver(allow_patterns, yes), host.workspace.as_ref())?;

    if fresh {
        std::fs::create_dir_all(sessions_dir)
            .with_context(|| format!("creating {}", sessions_dir.display()))?;
    }
    let path = session_path(sessions_dir, &session_id);
    let mut storage = if fresh {
        let system_prompt = system
            .map(str::to_string)
            .or_else(resolve_system_prompt)
            .or_else(|| Some(default_system_prompt().to_string()));
        JsonlStorage::create_with(sessions_dir, &session_id, None, system_prompt.as_deref())
            .with_context(|| format!("creating session {session_id}"))?
    } else {
        JsonlStorage::open(&path).context("replaying session log")?
    };
    println!(
        "tole chat — session {session_id} (Ctrl-D exits, resume: tole chat --resume {session_id})"
    );

    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    // B2: fresh sessions pin the resolved prompt; resumed sessions re-apply
    // the header-pinned one (see create_with above / JsonlStorage::open).
    if let Some(sys) = storage.system_prompt() {
        provider = provider.with_system_prompt(sys);
    }

    // Memory loop state: recall rides into the first message of a FRESH
    // session only (a resumed session already carries its context); the
    // session summary is stored when the REPL exits cleanly.
    #[cfg(feature = "shell-tools")]
    let (mut memory_injected, mut first_prompt, mut last_answer) = (!fresh, None, None);
    // Set when the typed message could not run because the session was
    // stuck mid-flight and the bounded resolve retries ran out — the
    // message is NOT in the durable log, so the operator must resend it.
    let mut dropped_message = false;

    let stdin = std::io::stdin();
    loop {
        print!("you> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                break; // EOF: clean exit, session stays durable
            }
            Ok(_) => {}
            Err(e) => anyhow::bail!("reading stdin: {e}"),
        }
        let text = line.trim();
        // Bound auto-resolve attempts per message so a wedged session
        // can't spin forever (each retry is a full provider round-trip).
        let mut retries_left: u8 = 2;
        match text {
            "" => continue,
            "/exit" | "/quit" => break,
            "/status" => {
                println!(
                    "pc: {:?}  seq: {}  entries: {}",
                    storage.state().pc,
                    storage.state().seq,
                    storage.entries().len()
                );
                continue;
            }
            _ => {}
        }

        // Dispatch by durable state: a boundary (Idle/Final) starts a new
        // turn; anything mid-flight is resolved via resume FIRST (looping
        // until it lands on a boundary), and only then does the freshly
        // typed message run as its own turn — user input is never dropped.
        //
        // Memory loop, pre-turn: the first fresh-session message carries
        // the recalled-context block; the durable log records exactly
        // what the provider sees.
        #[cfg(feature = "shell-tools")]
        if first_prompt.is_none() {
            first_prompt = Some(text.to_string());
        }
        #[cfg(feature = "shell-tools")]
        let turn_prompt: String = if fresh && !memory_injected {
            memory_injected = true;
            host.inject_memory(text)
        } else {
            text.to_string()
        };
        #[cfg(not(feature = "shell-tools"))]
        let turn_prompt = text.to_string();
        let outcome = loop {
            match storage.state().pc {
                tole_core::state::Pc::Idle | tole_core::state::Pc::Final => {
                    break run_turn(&mut storage, &mut provider, &registry, &turn_prompt);
                }
                _ => {
                    eprintln!("tole> (resolving the interrupted turn…)");
                    match resume_turn(&mut storage, &mut provider, &registry) {
                        // Landed on a boundary — dispatch the message now.
                        Ok(TurnOutcome::Final { .. }) => continue,
                        // Still stuck (re-denied, provider still down):
                        // surface it; the NEXT user message retries the
                        // resolve. The current message is preserved in
                        // this loop and will run once the session clears.
                        Ok(
                            other @ (TurnOutcome::ApprovalRequired { .. }
                            | TurnOutcome::ProviderFailed { .. }),
                        ) => {
                            if retries_left == 0 {
                                dropped_message = true;
                                break Ok(other);
                            }
                            retries_left -= 1;
                            continue;
                        }
                        Ok(other) => break Ok(other),
                        Err(e) => {
                            // Storage-level failure resolving: do not lose
                            // the user's message — report and keep the
                            // input buffered for the next attempt.
                            break Err(e);
                        }
                    }
                }
            }
        };

        match outcome {
            Ok(TurnOutcome::Final { text }) => {
                #[cfg(feature = "shell-tools")]
                {
                    last_answer = Some(text.clone());
                }
                println!("tole> {text}");
            }
            Ok(TurnOutcome::ApprovalRequired { name }) => eprintln!(
                "tole> (approval denied for '{name}' — turn aborted; your next message resumes)"
            ),
            Ok(TurnOutcome::UnknownTool { name }) => {
                eprintln!("tole> (unknown tool '{name}' — recorded; next message resumes)")
            }
            Ok(TurnOutcome::ProviderFailed { message }) => {
                eprintln!("tole> (provider failed: {message}; next message retries via resume)")
            }
            Ok(TurnOutcome::BudgetExhausted) => {
                eprintln!("tole> (step budget exhausted — turn aborted; next message resumes)")
            }
            Ok(TurnOutcome::LoopDetected { .. }) => eprintln!(
                "tole> (loop guard tripped — identical tool calls repeated; next message resumes)"
            ),
            Ok(TurnOutcome::Storage(e)) => anyhow::bail!("storage error: {e}"),
            Err(e) => anyhow::bail!("turn failed: {e}"),
        }
        if dropped_message {
            // The typed message never reached the durable log — saying
            // "your next message resumes" alone would let the operator
            // believe it was recorded (CodeCora scan 2026-09-18).
            eprintln!("tole> (note: the message you just typed was NOT recorded — resolve the session state, then resend it)");
        }
    }
    // Memory loop, post-session: a clean REPL exit with at least one
    // completed turn leaves a compact summary in the namespace.
    #[cfg(feature = "shell-tools")]
    if let (Some(fp), Some(la)) = (first_prompt.as_deref(), last_answer.as_deref()) {
        host.remember(&session_id, fp, la);
    }
    println!(
        "session {session_id} closed — entries: {}",
        storage.entries().len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// System prompt (B2)
// ---------------------------------------------------------------------------

/// Resolution order: --system flag > TOLE_SYSTEM_PROMPT env > none.
/// The flag is handled by clap (not parsed here); env is the fallback.
fn resolve_system_prompt() -> Option<String> {
    std::env::var("TOLE_SYSTEM_PROMPT")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Built-in default system prompt for fresh sessions (no `--system`, no
/// `TOLE_SYSTEM_PROMPT`). Tool discipline keeps the model on the dedicated,
/// guarded lanes: the file tools run inside the workspace jail with
/// hash-anchored editing, while `run_command` is a generic escape hatch
/// that no `--allow` pattern for file tools can cover (issue #103, from a
/// live E2E where the model routed a write through `bash -c`).
#[cfg(feature = "shell-tools")]
fn default_system_prompt() -> &'static str {
    "You are tole, a careful personal assistant. Tool discipline: for anything \
involving files, prefer the dedicated tools — read_file, write_file, \
edit_file — instead of run_command; they are safer and their approvals are \
what the user's --allow settings mean. Use run_command only for what those \
cannot do (pipes, builds, process control). Keep answers concise."
}

/// Same default without shell tools: `run_command` is not registered in
/// this profile, so the prompt must not advertise it.
#[cfg(not(feature = "shell-tools"))]
fn default_system_prompt() -> &'static str {
    "You are tole, a careful personal assistant. Tool discipline: for anything \
involving files, prefer the dedicated tools — read_file, write_file, \
edit_file. Keep answers concise."
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Human-readable exit summary. Non-Final outcomes exit non-zero so
/// scripts notice.
fn report_outcome(session_id: &str, outcome: TurnOutcome) {
    match outcome {
        TurnOutcome::Final { text } => {
            println!("{text}");
        }
        TurnOutcome::ApprovalRequired { name } => {
            eprintln!(
                "tole: approval denied for '{name}' — turn aborted, denial recorded \
                 (resume with: tole resume {session_id})"
            );
            std::process::exit(2);
        }
        TurnOutcome::UnknownTool { name } => {
            eprintln!("tole: unknown tool '{name}' — turn aborted, error recorded");
            std::process::exit(3);
        }
        TurnOutcome::ProviderFailed { message } => {
            eprintln!("tole: provider failed: {message} (resume with: tole resume {session_id})");
            std::process::exit(4);
        }
        TurnOutcome::BudgetExhausted => {
            eprintln!("tole: step budget exhausted (resume with: tole resume {session_id})");
            std::process::exit(5);
        }
        TurnOutcome::LoopDetected { tool, count } => {
            eprintln!(
                "tole: loop detected — tool `{tool}` called with identical input {count} times in a row (guard trips at {LOOP_TRIP_AFTER}); resume with: tole resume {session_id}"
            );
            std::process::exit(7);
        }
        TurnOutcome::Storage(e) => {
            eprintln!("tole: storage error: {e}");
            std::process::exit(6);
        }
    }
}

/// Time-ordered, filesystem-safe session id (no new deps).
fn new_session_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("s-{ms:x}-{pid:x}", pid = std::process::id())
}

#[cfg(all(test, feature = "shell-tools"))]
mod gh_repo_tests {
    use super::*;

    #[test]
    fn parses_https_ssh_and_git_suffix() {
        assert_eq!(
            github_repo_from_remote_url("https://github.com/foo/bar.git").as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            github_repo_from_remote_url("https://github.com/foo/bar").as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            github_repo_from_remote_url("git@github.com:foo/bar.git").as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            github_repo_from_remote_url("https://user:token@github.com/Foo/Bar.git").as_deref(),
            Some("Foo/Bar")
        );
    }

    #[test]
    fn rejects_non_github_and_garbage() {
        assert!(github_repo_from_remote_url("https://gitlab.com/foo/bar.git").is_none());
        assert!(github_repo_from_remote_url("https://github.com/only-owner").is_none());
        assert!(github_repo_from_remote_url("not a url").is_none());
        assert!(github_repo_from_remote_url("https://github.com/-bad/name").is_none());
    }

    #[test]
    fn detects_repo_from_checkout() {
        // A real checkout: init + remote origin, then detect.
        let dir = std::env::temp_dir().join(format!("tole-gh-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/detected/owner-name.git",
        ]);
        let detected = detect_github_repo(&dir);
        assert_eq!(detected.as_deref(), Some("detected/owner-name"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod default_prompt_tests {
    #[test]
    fn default_prompt_keeps_model_on_dedicated_file_tools() {
        let p = super::default_system_prompt();
        assert!(p.contains("prefer the dedicated tools"));
        #[cfg(feature = "shell-tools")]
        assert!(p.contains("run_command"));
    }

    #[test]
    fn default_prompt_is_short() {
        // Prompt discipline: a few lines, not a constitution.
        assert!(super::default_system_prompt().chars().count() < 600);
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::resolve_workspace_root;
    #[test]
    fn workspace_valid_dir_canonicalized() {
        let dir = std::env::temp_dir().join(format!("tole-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ws = dir.clone().into_os_string().into_string().unwrap();
        let resolved = resolve_workspace_root(Some(&ws)).unwrap();
        assert_eq!(resolved, dir.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_missing_dir_rejected() {
        let err = resolve_workspace_root(Some(&"/nonexistent/tole/ws".to_string()));
        assert!(err.is_err());
    }

    #[test]
    fn workspace_file_rejected() {
        let f = std::env::temp_dir().join(format!("tole-ws-file-{}", std::process::id()));
        let _ = std::fs::remove_file(&f);
        std::fs::write(&f, "x").unwrap();
        let ws = f.clone().into_os_string().into_string().unwrap();
        let err = resolve_workspace_root(Some(&ws));
        assert!(err.is_err());
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn workspace_none_falls_back_to_cwd() {
        let resolved = resolve_workspace_root(None).unwrap();
        assert_eq!(resolved, std::env::current_dir().unwrap());
    }
}

#[cfg(all(test, feature = "mcp"))]
mod mcp_preset_tests {
    use super::merge_mcp_specs;

    #[test]
    fn auto_spec_appended_when_no_explicit() {
        let merged = merge_mcp_specs(&[], false, vec!["cora=cora mcp".into()]);
        assert_eq!(merged, vec!["cora=cora mcp".to_string()]);
    }

    #[test]
    fn explicit_same_name_wins_over_auto() {
        let merged = merge_mcp_specs(
            &["cora=/opt/other/cora mcp --strict".to_string()],
            false,
            vec!["cora=cora mcp".into()],
        );
        assert_eq!(
            merged,
            vec!["cora=/opt/other/cora mcp --strict".to_string()]
        );
    }

    #[test]
    fn unrelated_explicit_and_auto_coexist() {
        let merged = merge_mcp_specs(
            &["fs=npx -y fs-server".to_string()],
            false,
            vec!["cora=cora mcp".into()],
        );
        assert_eq!(merged, vec!["fs=npx -y fs-server", "cora=cora mcp"]);
    }

    #[test]
    fn no_auto_flag_drops_presets_keeps_explicit() {
        let merged = merge_mcp_specs(
            &["fs=npx -y fs-server".to_string()],
            true,
            vec!["cora=cora mcp".into()],
        );
        assert_eq!(merged, vec!["fs=npx -y fs-server".to_string()]);
    }

    #[test]
    fn malformed_explicit_spec_treated_as_its_own_name() {
        // No '=' at all: the whole string counts as the name, so the
        // preset still attaches (parse() will reject the malformed spec
        // downstream with its normal error — this merge never panics).
        let merged = merge_mcp_specs(&["not-a-spec".into()], false, vec!["cora=cora mcp".into()]);
        assert_eq!(merged, vec!["not-a-spec", "cora=cora mcp"]);
    }
}
