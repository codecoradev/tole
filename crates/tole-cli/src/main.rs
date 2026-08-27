//! CLI host for tole-core: arg parsing and session wiring.

mod approver;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::approver::InteractiveApprover;
use crate::tools::WriteFileTool;
use tole_core::cora_search::CoraSearchTool;
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
use tole_core::gh::GhTool;
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::read_file::ReadFileTool;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{resume_turn, run_turn, TurnOutcome, LOOP_TRIP_AFTER};
use tole_core::uteke_search::UtekeSearchTool;

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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a new session and run one user turn.
    Run {
        /// The user prompt for this turn.
        prompt: String,

        /// Auto-allow Write tools matching this glob pattern without
        /// asking (e.g. --allow 'write_*'). Destructive tools are never
        /// auto-allowed. Repeatable.
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Auto-allow every Write call without prompting (heads-up
        /// mode. Destructive tools still prompt).
        #[arg(long)]
        yes: bool,
    },
    /// Resume an interrupted session (E5 crash-resume).
    Resume {
        /// Session id to resume.
        id: String,

        /// Same semantics as `run --allow`.
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Same semantics as `run --yes`.
        #[arg(long)]
        yes: bool,
    },
    /// Show durable state of a session.
    Status {
        /// Session id to inspect.
        id: String,
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
    match cli.command {
        Command::Run {
            prompt,
            allow_patterns,
            yes,
        } => run_command(&sessions_dir, &prompt, &allow_patterns, yes),
        Command::Resume {
            id,
            allow_patterns,
            yes,
        } => resume_command(&sessions_dir, &id, &allow_patterns, yes),
        Command::Status { id } => status_command(&sessions_dir, &id),
    }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

fn build_approver(
    allow_patterns: &[String],
    yes: bool,
) -> InteractiveApprover<approver::StdioPrompt> {
    InteractiveApprover::stdio()
        .with_allow_patterns(allow_patterns.to_vec())
        .with_auto_write(yes)
}

fn build_registry(approver: InteractiveApprover<approver::StdioPrompt>) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::with_approver(approver);
    let cwd = std::env::current_dir().context("resolving cwd")?;
    // ReadOnly tools: no approval needed.
    reg.register(Box::new(CoraSearchTool::new()))
        .map_err(|e| anyhow::anyhow!("registering cora_search: {e}"))?;
    reg.register(Box::new(UtekeSearchTool::new()))
        .map_err(|e| anyhow::anyhow!("registering uteke_search: {e}"))?;
    reg.register(Box::new(ReadFileTool::new(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering read_file: {e}"))?;
    // Write tools: gated per call. The jail root is the cwd.
    reg.register(Box::new(WriteFileTool::new(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering write_file: {e}"))?;
    reg.register(Box::new(EditFileTool::new(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering edit_file: {e}"))?;
    reg.register(Box::new(GhTool::new("codecoradev/tole")))
        .map_err(|e| anyhow::anyhow!("registering gh: {e}"))?;
    // Destructive tools: interactive-approver-only registration; every
    // call prompts — allowlists and --yes never apply (PRD risk table).
    reg.register(Box::new(DeleteFileTool::new(cwd)))
        .map_err(|e| anyhow::anyhow!("registering delete_file: {e}"))?;
    Ok(reg)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn run_command(
    sessions_dir: &Path,
    prompt: &str,
    allow_patterns: &[String],
    yes: bool,
) -> Result<()> {
    let cfg = OpenAiConfig::from_env()
        .context("missing CORAGENT_BASE_URL / CORAGENT_MODEL / CORAGENT_API_KEY")?;
    let session_id = new_session_id();
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    let mut storage = JsonlStorage::create(sessions_dir, &session_id, None)
        .with_context(|| format!("creating session {session_id}"))?;
    println!("session: {session_id}");

    let registry = build_registry(build_approver(allow_patterns, yes))?;
    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    let outcome = run_turn(&mut storage, &mut provider, &registry, prompt)?;
    report_outcome(&session_id, outcome);
    Ok(())
}

fn resume_command(
    sessions_dir: &Path,
    id: &str,
    allow_patterns: &[String],
    yes: bool,
) -> Result<()> {
    if !valid_session_id(id) {
        anyhow::bail!("invalid session id {id:?} (allowed: [a-z0-9-], max 64)");
    }
    let path = session_path(sessions_dir, id);
    if !path.exists() {
        anyhow::bail!("session {id} not found at {}", path.display());
    }
    let cfg = OpenAiConfig::from_env()
        .context("missing CORAGENT_BASE_URL / CORAGENT_MODEL / CORAGENT_API_KEY")?;
    let mut storage = JsonlStorage::open(&path).context("replaying session log")?;

    let registry = build_registry(build_approver(allow_patterns, yes))?;
    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    let outcome = resume_turn(&mut storage, &mut provider, &registry)?;
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
    Ok(())
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
