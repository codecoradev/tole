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
use tole_core::git::GitTool;
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
    /// Interactive multi-turn chat on one durable session (B1).
    Chat {
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
        Command::Chat {
            resume,
            last,
            allow_patterns,
            yes,
        } => chat_command(&sessions_dir, resume, last, &allow_patterns, yes),
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
    // Light git: status/diff/add/commit (push stays human).
    reg.register(Box::new(GitTool::new().in_dir(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering git: {e}"))?;
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
    let cfg = OpenAiConfig::from_env().context(
        "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY \
         (or the OPENAI_* equivalents)",
    )?;
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
    let cfg = OpenAiConfig::from_env().context(
        "missing provider config: set TOLE_BASE_URL / TOLE_MODEL / TOLE_API_KEY \
         (or the OPENAI_* equivalents)",
    )?;
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
// Chat (B1)
// ---------------------------------------------------------------------------

/// Most recently modified session id in `dir` (None when empty).
fn latest_session_id(dir: &Path) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".jsonl") || !valid_session_id(name.trim_end_matches(".jsonl")) {
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
    resume: Option<String>,
    last: bool,
    allow_patterns: &[String],
    yes: bool,
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

    if fresh {
        std::fs::create_dir_all(sessions_dir)
            .with_context(|| format!("creating {}", sessions_dir.display()))?;
    }
    let path = session_path(sessions_dir, &session_id);
    let mut storage = if fresh {
        JsonlStorage::create(sessions_dir, &session_id, None)
            .with_context(|| format!("creating session {session_id}"))?
    } else {
        JsonlStorage::open(&path).context("replaying session log")?
    };
    println!(
        "tole chat — session {session_id} (Ctrl-D exits, resume: tole chat --resume {session_id})"
    );

    let registry = build_registry(build_approver(allow_patterns, yes))?;
    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());

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
        // turn; anything mid-flight resolves via resume first, so a prior
        // denial/provider failure never wedges the conversation.
        let outcome = match storage.state().pc {
            tole_core::state::Pc::Idle | tole_core::state::Pc::Final => {
                run_turn(&mut storage, &mut provider, &registry, text)
            }
            _ => {
                // Mid-flight: resolve it, then (if it lands on a boundary)
                // immediately run this new message as its own turn.
                match resume_turn(&mut storage, &mut provider, &registry) {
                    Ok(TurnOutcome::Final { .. }) => {
                        run_turn(&mut storage, &mut provider, &registry, text)
                    }
                    Ok(other) => Ok(other),
                    Err(e) => Err(e),
                }
            }
        };

        match outcome {
            Ok(TurnOutcome::Final { text }) => println!("tole> {text}"),
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
    }
    println!(
        "session {session_id} closed — entries: {}",
        storage.entries().len()
    );
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
