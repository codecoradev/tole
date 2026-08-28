//! CLI host for tole-core: arg parsing and session wiring.

mod approver;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::approver::InteractiveApprover;
use crate::tools::WriteFileTool;
#[cfg(feature = "shell-tools")]
use tole_core::cora_search::CoraSearchTool;
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
#[cfg(feature = "shell-tools")]
use tole_core::gh::GhTool;
#[cfg(feature = "shell-tools")]
use tole_core::git::GitTool;
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
    /// List sessions in the sessions dir (B3).
    Sessions,

    /// Show durable state of a session.
    Status {
        /// Session id to inspect.
        id: String,
    },
    /// Interactive multi-turn chat on one durable session (B1).
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
        ),
        Command::Resume {
            id,
            allow_patterns,
            yes,
        } => resume_command(&sessions_dir, &id, &allow_patterns, yes),
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
        ),
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

/// B4 startup probing: a binary exists on PATH (or is an executable
/// absolute path). Used to skip CLI-backed tools whose engine is not
/// installed, instead of registering phantom tools that fail on every
/// call.
fn binary_available(name: &str) -> bool {
    if name.contains('/') {
        let p = std::path::Path::new(name);
        return p.is_file();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join(name).is_file() {
                return true;
            }
        }
    }
    false
}

fn build_registry(approver: InteractiveApprover<approver::StdioPrompt>) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::with_approver(approver);
    let cwd = std::env::current_dir().context("resolving cwd")?;
    // ReadOnly tools: no approval needed.
    #[cfg(feature = "shell-tools")]
    reg.register(Box::new(CoraSearchTool::new()))
        .map_err(|e| anyhow::anyhow!("registering cora_search: {e}"))?;
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
    reg.register(Box::new(ReadFileTool::new(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering read_file: {e}"))?;
    // Write tools: gated per call. The jail root is the cwd.
    reg.register(Box::new(WriteFileTool::new(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering write_file: {e}"))?;
    reg.register(Box::new(EditFileTool::new(cwd.clone())))
        .map_err(|e| anyhow::anyhow!("registering edit_file: {e}"))?;
    #[cfg(feature = "shell-tools")]
    reg.register(Box::new(GhTool::new("codecoradev/tole")))
        .map_err(|e| anyhow::anyhow!("registering gh: {e}"))?;
    // Light git: status/diff/add/commit (push stays human).
    #[cfg(feature = "shell-tools")]
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
    system: Option<&str>,
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
    let system_prompt = system.map(str::to_string).or_else(resolve_system_prompt);
    let mut storage =
        JsonlStorage::create_with(sessions_dir, &session_id, None, system_prompt.as_deref())
            .with_context(|| format!("creating session {session_id}"))?;
    println!("session: {session_id}");

    let registry = build_registry(build_approver(allow_patterns, yes))?;
    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    if let Some(sys) = system_prompt.as_deref() {
        provider = provider.with_system_prompt(sys);
    }
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
    // B2: the system prompt is pinned in the session header — resume
    // re-applies exactly what the session was created with (never the
    // ambient env, which may have changed since).
    if let Some(sys) = storage.system_prompt() {
        provider = provider.with_system_prompt(sys);
    }
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
            "no sessions in {} (dir does not exist)",
            sessions_dir.display()
        );
        return Ok(());
    }
    // (epoch_secs, id, pc, seq, turns) — epoch secs first so a plain
    // sort_by_key ascending gives newest-first via Reverse.
    let mut rows: Vec<(u64, String, String, u64, usize)> = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let stem = name.trim_end_matches(".jsonl");
        if !name.ends_with(".jsonl") || !valid_session_id(stem) {
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
    system: Option<&str>,
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
        let system_prompt = system.map(str::to_string).or_else(resolve_system_prompt);
        JsonlStorage::create_with(sessions_dir, &session_id, None, system_prompt.as_deref())
            .with_context(|| format!("creating session {session_id}"))?
    } else {
        JsonlStorage::open(&path).context("replaying session log")?
    };
    println!(
        "tole chat — session {session_id} (Ctrl-D exits, resume: tole chat --resume {session_id})"
    );

    let registry = build_registry(build_approver(allow_patterns, yes))?;
    let mut provider = OpenAiProvider::new(cfg).with_tool_specs(registry.specs());
    // B2: fresh sessions pin the resolved prompt; resumed sessions re-apply
    // the header-pinned one (see create_with above / JsonlStorage::open).
    if let Some(sys) = storage.system_prompt() {
        provider = provider.with_system_prompt(sys);
    }

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
        let outcome = loop {
            match storage.state().pc {
                tole_core::state::Pc::Idle | tole_core::state::Pc::Final => {
                    break run_turn(&mut storage, &mut provider, &registry, text);
                }
                mid => {
                    eprintln!("tole> (resolving interrupted turn, pc={mid:?}…)");
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
// System prompt (B2)
// ---------------------------------------------------------------------------

/// Resolution order: --system flag > TOLE_SYSTEM_PROMPT env > none.
/// The flag is handled by clap (not parsed here); env is the fallback.
fn resolve_system_prompt() -> Option<String> {
    std::env::var("TOLE_SYSTEM_PROMPT")
        .ok()
        .filter(|s| !s.trim().is_empty())
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
