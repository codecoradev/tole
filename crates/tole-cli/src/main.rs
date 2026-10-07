//! CLI host for tole-core: arg parsing and session wiring.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use tole_cli::approver::{InteractiveApprover, StdioPrompt};
use tole_cli::tools::WriteFileTool;
#[cfg(feature = "mcp")]
use tole_core::approval::AllowlistApprover;

#[cfg(feature = "shell-tools")]
mod acp;
use tole_cli::approvals;
#[cfg(feature = "mcp-http")]
mod mcp_http;
mod mission;
#[cfg(feature = "shell-tools")]
mod serve;
mod upgrade;
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
use tole_core::turn::{resume_turn, run_turn, run_turn_with_cancel, TurnOutcome, LOOP_TRIP_AFTER};
#[cfg(feature = "shell-tools")]
use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};
use tole_core::verify_package::VerifyPackageTool;

/// Where sessions live unless the user overrides it.
const DEFAULT_SESSIONS_DIR: &str = ".tole/sessions";

/// Session id → path (`<dir>/<id>.jsonl`).
fn session_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// Session id validity — the one shared rule (`[A-Za-z0-9_-]`, 1..=64,
/// no traversal), identical to what serve/ACP/storage accept.
fn valid_session_id(id: &str) -> bool {
    tole_core::storage::is_valid_session_id(id)
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

    /// Plan mode: expose ONLY read-only tools for the session — write/
    /// delete/run tools are absent from the wire entirely (the model
    /// cannot even see them). For explore-and-plan runs before granting
    /// any mutation. The default system prompt gains a matching
    /// read-only instruction; explicit --system overrides still win.
    #[arg(long, global = true)]
    plan_mode: bool,

    /// Child agents (#171): PARENT-ONLY operator mode — spawned child
    /// agents each get their own git worktree + branch. Default OFF;
    /// the model cannot flip this per call (worktrees stay bounded by
    /// the child cap; merge-back stays human). Applies to run/chat/resume.
    #[arg(long, global = true)]
    agents_worktree: bool,

    /// Pre-tool-use process hook (issue #110): runs before every
    /// Write/Destructive tool executes. Receives one JSON object on
    /// stdin (`{"event":"pretool","tool":...,"input":...}`); exit code
    /// 2 = DENY the call (durable — the turn parks at the denial like
    /// an approval denial and is resumable; on resume WITH a new prompt
    /// the model sees the denial and replans); any other non-zero exit
    /// / timeout is a logged non-blocking hook failure.
    /// Example: --on-pretool /usr/local/bin/tole-guard.sh. Repeatable;
    /// default OFF.
    #[arg(long, global = true)]
    on_pretool: Vec<String>,

    /// Post-tool-use process hook (issue #110): runs after every
    /// Write/Destructive tool settles. Receives
    /// `{"event":"posttool","tool":...,"input":...,"ok":true|false}`;
    /// observe-only (output cannot block). Repeatable; default OFF.
    #[arg(long, global = true)]
    on_posttool: Vec<String>,

    /// Turn-end stop gate hook (issue #145): runs when the model
    /// produces its final message, BEFORE it commits. Receives one JSON
    /// object on stdin (`{"event":"turnend","final_text_preview":...,
    /// "tools":[{"tool":...,"risk":...}]}`); exit code 2 = DENY the
    /// finish — the reason becomes the model's next input and the loop
    /// continues (bounded: 3 denials per turn, then the turn settles as
    /// blocked). 30s timeout per hook (verification gates run lint/tests).
    /// Example: --on-turnend "cargo check". Repeatable; default OFF.
    #[arg(long, global = true)]
    on_turnend: Vec<String>,

    /// Trust preset: auto-allow the fleet's own ecosystem tools without
    /// per-call approval. `internal` = uteke_*/cora_search/mcp_cora_*/
    /// verify_package/job_*/tole_session_*; `read_only` = every safe
    /// read. Repeatable; flag wins over the TOLE_TRUST env; `none`
    /// (default) = today's behavior. Pure sugar over --allow patterns —
    /// Destructive tools are never auto-allowed.
    #[arg(long, global = true)]
    trust: Vec<String>,

    /// Load a SKILL.md file into the system prompt (issue #161). The
    /// file must have YAML frontmatter with `name` matching its parent
    /// directory (or just be a plain path). Repeatable.
    #[arg(long, global = true)]
    skill: Vec<PathBuf>,

    /// Disable skills support (discovery + load_skill tool).
    #[arg(long, global = true)]
    no_skills: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a new session and run one user turn.
    Run {
        /// The user prompt for this turn. Omit when --prompt-file is set.
        prompt: Option<String>,

        /// Read the prompt from a file (`-` = stdin). Mutually exclusive
        /// with the positional PROMPT (issue #216).
        #[arg(long = "prompt-file")]
        prompt_file: Option<String>,

        /// Operator alias stored in the session header — `tole sessions`
        /// shows it and `resume` accepts it instead of the generated id
        /// (issue #216).
        #[arg(long)]
        name: Option<String>,

        /// Wall-clock cap for the turn, in seconds. Expiry cancels the
        /// turn at the next checkpoint — it settles resumably, never
        /// dead (issue #216).
        #[arg(long)]
        timeout: Option<u64>,

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
    /// Run an autonomous mission: budgeted turn-chaining toward a goal
    /// (issue #199). Every chained turn is a normal durable turn —
    /// crash mid-mission resumes exactly where it stopped; Destructive
    /// tools stay un-auto-allowable. Termination: the model declares
    /// MISSION_COMPLETE (and `--verify` exits 0, when set), or a budget
    /// trips (`--max-steps` / `--max-minutes`), or the verify gate fails
    /// too many times. A durable summary lands on the session either way.
    Mission {
        /// The mission goal, verbatim.
        goal: String,

        /// Total provider steps across all chained turns (from the
        /// durable usage ledger). Default: budget tier (48; 96 with
        /// --trust internal).
        #[arg(long)]
        max_steps: Option<u64>,

        /// Wall-clock cap in minutes. Default: budget tier (15; 30 with
        /// --trust internal).
        #[arg(long)]
        max_minutes: Option<u64>,

        /// Total token ceiling (prompt + completion, from the usage
        /// ledger). Default: budget tier (200k; 500k with
        /// --trust internal).
        #[arg(long)]
        max_tokens: Option<u64>,

        /// Verification command run after each turn; exit 0 = goal
        /// achieved (overrides the model's completion marker). Failures
        /// return to the model with the output; 3 failures settle the
        /// mission as verify_failed.
        #[arg(long)]
        verify: Option<String>,

        /// Per-run timeout of the --verify command, in seconds.
        #[arg(long, default_value_t = 300)]
        verify_timeout: u64,

        /// Continue an interrupted mission instead of starting a new one.
        #[arg(long)]
        resume: Option<String>,

        /// Same semantics as `run --allow`.
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Auto-allow every Write call without prompting (heads-up
        /// mode. Destructive tools still prompt).
        #[arg(long)]
        yes: bool,
    },
    /// Check for updates and self-upgrade via cargo (issue #220).
    Upgrade {
        /// Only report whether an update is available; do not install.
        #[arg(long)]
        check: bool,

        /// Skip the confirmation prompt (CI/automation).
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
    /// Remote approvals (issue #200): list pending Write-approval
    /// requests on a `tole serve` instance and allow/deny them. URL
    /// defaults to TOLE_SERVE_URL or http://127.0.0.1:7801; token to
    /// TOLE_SERVE_TOKEN.
    Approvals {
        /// list | allow | deny
        action: String,

        /// The approval id (required for allow/deny).
        id: Option<String>,

        /// Base URL of the serve instance.
        #[arg(long, default_value = "http://127.0.0.1:7801")]
        url: String,

        /// Bearer token (defaults to TOLE_SERVE_TOKEN).
        #[arg(long)]
        token: Option<String>,
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
    /// Serve tole as an ACP agent over stdio (issue #95): editors and
    /// ACP clients drive durable tole sessions; tool approvals surface
    /// as permission requests in the client (Write calls also offer
    /// allow_always — remembered for the session, never for
    /// Destructive). An `approval` selector (ask/auto) is always
    /// advertised; TOLE_MODELS (comma-separated model ids) adds a model
    /// picker, persisted durably per session (issue #176).
    #[cfg(feature = "shell-tools")]
    Acp {
        /// Same semantics as `run --allow` (Write pre-authorization).
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Auto-allow every Write call (Destructive still prompts in the
        /// client).
        #[arg(long)]
        yes: bool,

        /// Default file-tools root; each session's jail is the client's
        /// session cwd.
        #[arg(long)]
        workspace: Option<String>,

        /// Memory loop backend (`uteke`) — same as `--memory uteke` on
        /// run/chat. Falls back to the TOLE_MEMORY env.
        #[arg(long)]
        memory: Option<String>,
    },
    /// Serve tole over HTTP (issue #96): token-authenticated daemon;
    /// `--transport mcp` serves the multi-session MCP surface (#137).
    #[cfg(feature = "shell-tools")]
    Serve {
        /// TCP port to listen on.
        #[arg(long, default_value_t = 7801)]
        port: u16,

        /// Transport: `rest` (default) or `mcp` (Streamable HTTP).
        #[arg(long, default_value = "rest")]
        transport: String,

        /// Bind address (default: 127.0.0.1 — local only).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Bearer token required on every request (or TOLE_SERVE_TOKEN
        /// env). Refuses to start without one.
        #[arg(long)]
        token: Option<String>,

        /// Same semantics as `run --allow` (Write pre-authorization).
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,

        /// Root directory for the file tools (same as run).
        #[arg(long)]
        workspace: Option<String>,

        /// Memory loop backend (`uteke`) — same as `--memory uteke` on
        /// run/chat. Falls back to the TOLE_MEMORY env.
        #[arg(long)]
        memory: Option<String>,
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
    // Startup update notification (issue #220): banner is best-effort,
    // cache-backed, and skipped entirely for `tole upgrade` (which does
    // its own check) and for TOLE_NO_UPDATE_CHECK=1 (checked inside).
    if !matches!(cli.command, Command::Upgrade { .. }) {
        if let Some(handle) = tole_core::update_check::check_and_notify() {
            // Do not join: a hanging network must never delay startup.
            // Detach — the thread dies with the process, which is fine
            // for a best-effort banner.
            drop(handle);
        }
    }
    let sessions_dir = PathBuf::from(
        cli.sessions_dir
            .clone()
            .unwrap_or_else(|| DEFAULT_SESSIONS_DIR.to_string()),
    );
    // The RAW override for the server faces: an explicit --sessions-dir
    // relocates serve/acp session storage; the default (None) keeps the
    // per-session-cwd layout those faces always had. The resolved
    // `sessions_dir` above stays the run/chat/sessions/status default.
    let sessions_dir_override = cli.sessions_dir.clone().map(PathBuf::from);
    #[cfg(feature = "mcp")]
    let mcp_specs = merge_mcp_specs(&cli.mcp_server, cli.no_auto_mcp, auto_mcp_specs());
    // Explicit --mcp-server flags only — the merged `mcp_specs` also
    // contains the cora auto-preset, which must NOT trigger the
    // server-face refusal below.
    #[cfg(feature = "mcp")]
    let explicit_mcp = !cli.mcp_server.is_empty();
    // scan-3 finding fix: the global --workspace/--memory flags now flow
    // into the host, so `tole serve/acp/mcp` honor them as fallbacks when
    // the subcommand-level flags are absent (previously they were
    // silently ignored by those subcommands).
    let host = HostConfig {
        workspace: cli.workspace.clone(),
        skills: cli.skill.clone(),
        no_skills: cli.no_skills,
        #[cfg(feature = "mcp")]
        mcp_server: mcp_specs,
        plan_mode: cli.plan_mode,
        agents_worktree: cli.agents_worktree,

        on_pretool: cli.on_pretool.clone(),
        on_posttool: cli.on_posttool.clone(),
        on_turnend: cli.on_turnend.clone(),
        #[cfg(feature = "shell-tools")]
        memory: resolve_memory(cli.memory.as_ref())?,
        #[cfg(not(feature = "shell-tools"))]
        memory: (),
    };
    // Trust presets (issue #159): expand once, before dispatch — every
    // allow_patterns-consuming subcommand appends these. The --trust
    // flag wins over the TOLE_TRUST env (documented); the env splits on
    // commas/whitespace so one variable can carry several presets.
    let trust_extra = expand_trust(&resolve_trust_flags(
        &cli.trust,
        std::env::var("TOLE_TRUST").ok(),
    ))?;

    match cli.command {
        Command::Run {
            prompt,
            prompt_file,
            name,
            timeout,
            system,
            allow_patterns: allow_patterns_in,
            yes,
        } => {
            let allow_patterns = with_trust(allow_patterns_in, &trust_extra);
            // Prompt resolution (issue #216): exactly one of positional
            // PROMPT / --prompt-file ('-' = stdin).
            let prompt = match (prompt, prompt_file.as_deref()) {
                (Some(p), None) => p,
                (None, Some("-")) => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .context("reading prompt from stdin")?;
                    buf
                }
                (None, Some(path)) => std::fs::read_to_string(path)
                    .with_context(|| format!("reading prompt file {path}"))?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("pass either a positional PROMPT or --prompt-file, not both")
                }
                (None, None) => {
                    anyhow::bail!("missing prompt (positional PROMPT or --prompt-file)")
                }
            };
            run_command(
                &sessions_dir,
                &prompt,
                system.as_deref(),
                &allow_patterns,
                yes,
                &host,
                name.as_deref(),
                timeout,
            )
        }
        Command::Upgrade { check, yes } => {
            let code = upgrade::run(check, yes)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Resume {
            id,
            prompt,
            allow_patterns: allow_patterns_in,
            yes,
        } => {
            let allow_patterns = with_trust(allow_patterns_in, &trust_extra);
            resume_command(
                &sessions_dir,
                &id,
                prompt.as_deref(),
                &allow_patterns,
                yes,
                &host,
            )
        }
        #[cfg(all(feature = "mcp", feature = "shell-tools"))]
        Command::Mcp {
            allow_patterns: allow_patterns_in,
            workspace,
        } => {
            let allow_patterns = with_trust(allow_patterns_in, &trust_extra);
            // Global flags must not SILENTLY no-op on this subcommand
            // (cora scan-3 #9): plan-mode filters the served registry to
            // read-only; hooks are not wired in server mode (no local
            // approver boundary — server mode pre-authorizes via
            // --allow), so --on-pretool/--on-posttool error out loudly
            // instead of being ignored.
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                anyhow::bail!(
                    "--on-pretool/--on-posttool are not supported by `tole mcp` \
                     (server mode pre-authorizes Write tools with --allow instead)"
                );
            }
            if !host.on_turnend.is_empty() {
                anyhow::bail!(
                    "--on-turnend is not supported by `tole mcp` (the tool server runs no \
                     turns; stop gates apply to run/chat/resume/serve/acp sessions)"
                );
            }
            #[cfg(feature = "mcp")]
            check_client_session_flags("mcp", &host.skills, host.no_skills, explicit_mcp)?;
            if host.plan_mode {
                eprintln!("tole mcp: --plan-mode is active — serving read-only tools only");
            }
            let workspace = workspace.or_else(|| host.workspace.clone());
            mcp_server_command(
                workspace.as_ref(),
                &allow_patterns,
                #[cfg(feature = "mcp")]
                host.plan_mode,
            )
        }
        Command::Approvals {
            action,
            id,
            url,
            token,
        } => {
            let token = token
                .or_else(|| std::env::var("TOLE_SERVE_TOKEN").ok())
                .context("approval decisions need a token (--token or TOLE_SERVE_TOKEN)")?;
            approvals::cli(&action, id.as_deref(), &url, &token)
        }
        Command::Mission {
            goal,
            max_steps,
            max_minutes,
            max_tokens,
            verify,
            verify_timeout,
            resume,
            allow_patterns: allow_patterns_in,
            yes,
        } => {
            let allow_patterns = with_trust(allow_patterns_in, &trust_extra);
            if host.plan_mode {
                anyhow::bail!(
                    "--plan-mode has no meaning for a mission (missions mutate by definition)"
                );
            }
            // Issue #258 (rescan #46, scan-3 #9 rule): global flags must
            // not SILENTLY no-op — missions build their own registry and
            // run no host memory loop today, so hook/memory flags are
            // loudly refused instead of being ignored.
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                anyhow::bail!(
                    "--on-pretool/--on-posttool are not supported by `tole run mission` \
                     (mission registries do not wire tool hooks yet)"
                );
            }
            if !host.on_turnend.is_empty() {
                anyhow::bail!(
                    "--on-turnend is not supported by `tole run mission` (mission turns \
                     settle through the mission loop, not the turn-end hook path)"
                );
            }
            // #[cfg]-mirrored like the HostConfig field itself: without
            // shell-tools the field is the unit type (cora round 1).
            #[cfg(feature = "shell-tools")]
            if host.memory.is_some() {
                anyhow::bail!(
                    "--memory is not supported by `tole run mission` (the mission loop does \
                     not run the harness memory loop yet)"
                );
            }
            #[cfg(feature = "mcp")]
            check_client_session_flags("mission", &host.skills, host.no_skills, explicit_mcp)?;
            // Budget tier (issue #201): the internal trust preset earns
            // the trusted tier's headroom; explicit flags always win.
            let trusted = trust_extra.iter().any(|p| p == "todo_write");
            let tier = mission::BudgetTier::resolve(trusted, max_steps, max_minutes, max_tokens);
            let sessions_dir = sessions_dir.clone();
            std::fs::create_dir_all(&sessions_dir)
                .with_context(|| format!("creating {}", sessions_dir.display()))?;
            #[cfg(feature = "mcp")]
            let mcp_cfgs: Vec<tole_core::mcp::McpServerConfig> = host
                .mcp_server
                .iter()
                .map(|s| tole_core::mcp::McpServerConfig::parse(s))
                .collect::<Result<Vec<_>, String>>()
                .map_err(anyhow::Error::msg)?;
            #[cfg(feature = "mcp")]
            let registry = build_registry(
                build_approver(&allow_patterns, yes),
                host.workspace.as_ref(),
                &mcp_cfgs,
                host.agents_worktree,
                &allow_patterns,
            )?;
            #[cfg(not(feature = "mcp"))]
            let registry = build_registry(
                build_approver(&allow_patterns, yes),
                host.workspace.as_ref(),
                host.agents_worktree,
                &allow_patterns,
            )?;
            mission::run_mission(
                mission::MissionConfig {
                    goal,
                    max_steps: tier.max_steps,
                    max_minutes: tier.max_minutes,
                    max_tokens: tier.max_tokens,
                    verify,
                    verify_timeout_secs: verify_timeout,
                    resume_id: resume,
                },
                registry,
                &sessions_dir,
            )
        }
        #[cfg(feature = "shell-tools")]
        Command::Acp {
            allow_patterns: allow_patterns_in,
            yes,
            workspace,
            memory,
        } => {
            let allow_patterns = with_trust(allow_patterns_in, &trust_extra);
            // Same loud-bail rule as `tole mcp` for hooks: the ACP host
            // does not wire local pre/post hooks — approvals happen in
            // the editor via permission requests instead.
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                anyhow::bail!(
                    "--on-pretool/--on-posttool are not supported by `tole acp` \
                     (approvals happen via session/request_permission in the client)"
                );
            }
            #[cfg(feature = "mcp")]
            check_client_session_flags("acp", &host.skills, host.no_skills, explicit_mcp)?;
            if host.plan_mode {
                eprintln!("tole acp: --plan-mode is active — serving read-only tools only");
            }
            let memory = match memory.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(_) => resolve_memory(memory.as_ref())?,
                None => host.memory.clone(),
            };
            let workspace = workspace.or_else(|| host.workspace.clone());
            crate::acp::run_acp(
                &allow_patterns,
                yes,
                workspace.as_ref(),
                host.plan_mode,
                memory,
                sessions_dir_override.clone(),
                host.on_turnend.clone(),
            )
        }
        #[cfg(feature = "shell-tools")]
        Command::Serve {
            port,
            bind,
            transport,
            token,
            allow_patterns: allow_patterns_in,
            workspace,
            memory,
        } => {
            let allow_patterns = with_trust(allow_patterns_in, &trust_extra);
            // Loud bails, same rule as `tole mcp`/`tole acp`: these host
            // flags have no server-face wiring, and a silent no-op is
            // worse than a startup error (scan-3 #9).
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                anyhow::bail!(
                    "--on-pretool/--on-posttool are not supported by `tole serve` \
                     (a server has no local human; pre-authorize Write tools with --allow)"
                );
            }
            #[cfg(feature = "mcp")]
            check_client_session_flags("serve", &host.skills, host.no_skills, explicit_mcp)?;
            let memory = match memory.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(_) => resolve_memory(memory.as_ref())?,
                None => host.memory.clone(),
            };
            let workspace = workspace.or_else(|| host.workspace.clone());
            // Both transports refuse to start without a token (#96/#137).
            let token = token
                .or_else(|| std::env::var("TOLE_SERVE_TOKEN").ok())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .context(
                    "refusing to start an unauthenticated server: set --token or TOLE_SERVE_TOKEN",
                )?;
            if transport == "mcp" {
                #[cfg(feature = "mcp-http")]
                {
                    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
                    return rt.block_on(crate::mcp_http::run_mcp_http(
                        &bind,
                        port,
                        &token,
                        allow_patterns,
                        host.plan_mode,
                        memory,
                        workspace.as_ref(),
                        sessions_dir_override.clone(),
                        host.on_turnend.clone(),
                    ));
                }
                #[cfg(not(feature = "mcp-http"))]
                anyhow::bail!("--transport mcp requires the mcp-http feature");
            }
            crate::serve::run_serve(crate::serve::ServeConfig {
                bind,
                port,
                token: Some(token),
                allow_patterns,
                workspace,
                plan_mode: host.plan_mode,
                memory,
                sessions_dir: sessions_dir_override,
                turnend: host.on_turnend.clone(),
            })
        }
        Command::Sessions => sessions_command(&sessions_dir),
        Command::Status { id } => status_command(&sessions_dir, &id),
        Command::Chat {
            system,
            resume,
            last,
            allow_patterns: allow_patterns_in,
            yes,
        } => {
            let allow_patterns = with_trust(allow_patterns_in, &trust_extra);
            chat_command(
                &sessions_dir,
                system.as_deref(),
                resume,
                last,
                &allow_patterns,
                yes,
                &host,
            )
        }
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
    /// Issue #161: explicit --skill files (loaded into the system prompt)
    skills: Vec<PathBuf>,
    /// Issue #161: --no-skills disables skills discovery + load_skill.
    no_skills: bool,
    #[cfg(feature = "mcp")]
    mcp_server: Vec<String>,
    /// Plan mode (issue #109): registry filtered to ReadOnly tools.
    plan_mode: bool,
    /// Child agents (#171): PARENT-ONLY operator mode — children spawn
    /// into per-child git worktrees. Default false; the model cannot
    /// flip this per call (storage boomerang, owner decision 2026-10-05).
    agents_worktree: bool,

    /// Tool-boundary hook command lines (issue #110), default empty.
    on_pretool: Vec<String>,
    on_turnend: Vec<String>,
    on_posttool: Vec<String>,
    /// shell-tools-only host knob. `not(feature = "shell-tools")` builds
    /// still assign `memory: None` in dispatch — the field stays so the
    /// assignments and helper signatures never fork per profile.
    #[cfg(not(feature = "shell-tools"))]
    memory: (),
    #[cfg(feature = "shell-tools")]
    memory: Option<tole_core::memory::MemoryConfig>,
}

impl HostConfig {
    #[cfg(any(feature = "shell-tools", feature = "mcp"))]
    fn on_pretool_non_empty(&self) -> bool {
        !self.on_pretool.is_empty()
    }
    #[cfg(any(feature = "shell-tools", feature = "mcp"))]
    fn on_posttool_non_empty(&self) -> bool {
        !self.on_posttool.is_empty()
    }
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
    fn remember(&self, session_id: &str, first_prompt: &str, last_answer: &str, wrote: bool) {
        let Some(mem) = self.memory.as_ref() else {
            return;
        };
        match tole_core::memory::remember_session(mem, session_id, first_prompt, last_answer, wrote)
        {
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
    tole_cli::session_host::github_repo_from_remote_url(&String::from_utf8_lossy(&out.stdout))
}

/// Trust presets (issue #159): one word for "auto-allow the fleet's own
/// ecosystem tools". Pure sugar — the expanded patterns feed the SAME
/// AllowlistApprover machinery as `--allow`, so enforcement (and the
/// Destructive-never-allowed invariant) is unchanged. `internal` covers
/// the probe-gated native integrations (uteke_*, cora_search) plus the
/// cora MCP auto-preset surface (mcp_cora_*) and the always-safe
/// verify_package/job tools; it deliberately excludes the write-capable
/// native tools (write_file/edit_file/run_command/git/gh), which keep
/// prompting.
const TRUST_PRESETS: &[(&str, &[&str])] = &[
    (
        "internal",
        &[
            "uteke_*",
            "cora_search",
            "mcp_cora_*",
            "verify_package",
            "job_*",
            "tole_session_*",
            "todo_write",
        ],
    ),
    (
        "read_only",
        &[
            "read_file",
            "verify_package",
            "uteke_recall",
            "cora_search",
            "tole_session_status",
            "tole_session_list",
            "job_poll",
        ],
    ),
];

/// Effective trust-preset flag list: an explicit `--trust` flag wins
/// wholesale over the `TOLE_TRUST` env; with no flags, the env (split on
/// commas/whitespace) is the list. Found by activation testing
/// 2026-10-05: the env was documented ("flag wins over the TOLE_TRUST
/// env") but never read — same documented-but-unimplemented class as the
/// #138 ambiguity refusal.
fn resolve_trust_flags(flag: &[String], env_val: Option<String>) -> Vec<String> {
    if !flag.is_empty() {
        return flag.to_vec();
    }
    let Some(env_val) = env_val else {
        return Vec::new();
    };
    env_val
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Expand `--trust` preset names into extra allow patterns. Unknown
/// preset names are a hard error — a typo silently narrowing trust would
/// be worse than failing. `none`/empty → no extra patterns.
fn expand_trust(presets: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in presets {
        if p.eq_ignore_ascii_case("none") {
            continue;
        }
        let found = TRUST_PRESETS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(p))
            .map(|(_, patterns)| patterns);
        let Some(patterns) = found else {
            let names: Vec<&str> = TRUST_PRESETS.iter().map(|(n, _)| *n).collect();
            anyhow::bail!(
                "unknown trust preset {p:?} — available: {}",
                names.join(", ")
            );
        };
        out.extend(patterns.iter().map(|s| s.to_string()));
    }
    Ok(out)
}

/// Append the trust-preset patterns to the user's `--allow` list.
fn with_trust(mut allow_patterns: Vec<String>, trust_extra: &[String]) -> Vec<String> {
    allow_patterns.extend(trust_extra.iter().cloned());
    allow_patterns
}

/// Server faces (mcp/serve/acp) refuse client-session-only flags loudly
/// instead of silently ignoring them (scan-3 #9 rule; found by the
/// 2026-10-05 full-feature sweep passing these flags got no error and
/// no effect). `explicit_mcp_servers` must be the RAW `--mcp-server`
/// flag state, not the merged preset list — the cora auto-preset must
/// not trip this.
///
/// Every call site is `#[cfg(feature = "mcp")]`-gated (the `--mcp-server`
/// flag and its presets exist only there), so without the `mcp` feature
/// the function is unreachable and gated out to stay warning-free.
#[cfg(feature = "mcp")]
fn check_client_session_flags(
    face: &str,
    skills: &[PathBuf],
    no_skills: bool,
    explicit_mcp_servers: bool,
) -> Result<()> {
    if !skills.is_empty() {
        anyhow::bail!(
            "--skill is not supported by `tole {face}` (skills load into \
             run/chat/resume session system prompts)"
        );
    }
    if no_skills {
        anyhow::bail!(
            "--no-skills is not supported by `tole {face}` (server faces never load \
             skills — nothing to disable)"
        );
    }
    if explicit_mcp_servers {
        anyhow::bail!(
            "--mcp-server is not supported by `tole {face}` (external MCP clients \
             attach to run/chat/resume sessions; server faces serve their own registry)"
        );
    }
    Ok(())
}

fn build_registry(
    approver: InteractiveApprover<StdioPrompt>,
    workspace: Option<&String>,
    #[cfg(feature = "mcp")] mcp_servers: &[tole_core::mcp::McpServerConfig],
    agents_worktree: bool,
    parent_allows: &[String],
) -> Result<ToolRegistry> {
    // Child-agent depth (#171): spawned children carry TOLE_AGENT_DEPTH=1;
    // at depth >= 1 the agent tools vanish (structural cap) and the
    // spawn-yourself bypass closes in run_command/job_start.
    let agent_depth: u32 = std::env::var("TOLE_AGENT_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
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
    let run_cmd = RunCommandTool::new(cwd.clone());
    let run_cmd = if std::env::var("TOLE_AGENT_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0)
        >= 1
    {
        run_cmd.in_child_agent_mode()
    } else {
        run_cmd
    };
    reg.register(Box::new(run_cmd))
        .map_err(|e| anyhow::anyhow!("registering run_command: {e}"))?;
    // Long-running jobs (#59): detached spawn + poll, logs inside the
    // file-tools workspace so read_file can reach the full log.
    #[cfg(feature = "shell-tools")]
    let job_start = JobStartTool::new(file_root.clone());
    let job_start = if std::env::var("TOLE_AGENT_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0)
        >= 1
    {
        job_start.in_child_agent_mode()
    } else {
        job_start
    };
    reg.register(Box::new(job_start))
        .map_err(|e| anyhow::anyhow!("registering job_start: {e}"))?;
    #[cfg(feature = "shell-tools")]
    reg.register(Box::new(JobPollTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering job_poll: {e}"))?;
    reg.register(Box::new(VerifyPackageTool::new()))
        .map_err(|e| anyhow::anyhow!("registering verify_package: {e}"))?;
    // systemone_decide (#172): probe-gated — present iff SYSTEMONE_API_KEY
    // is set; absent key = silently off (owner decision 2026-10-05).
    if let Some(t) = tole_core::systemone::SystemOneTool::from_env() {
        reg.register(Box::new(t))
            .map_err(|e| anyhow::anyhow!("registering systemone_decide: {e}"))?;
    }
    // Child agents (#171): parent-only. Depth >= 1 = this IS a child —
    // no agent tools at all (the structural depth cap).
    if agent_depth == 0 {
        let bin = std::env::current_exe().context("resolving the tole binary for child agents")?;
        let start = tole_core::agents::AgentStartTool::new(bin, file_root.clone())
            .with_worktrees(agents_worktree)
            .with_parent_allows(parent_allows.to_vec());
        reg.register(Box::new(start))
            .map_err(|e| anyhow::anyhow!("registering agent_start: {e}"))?;
        reg.register(Box::new(tole_core::agents::AgentPollTool::new(
            file_root.clone(),
        )))
        .map_err(|e| anyhow::anyhow!("registering agent_poll: {e}"))?;
    }
    reg.register(Box::new(ReadFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering read_file: {e}"))?;
    // Web tools (issue #215): probe-first — fetch always; search only
    // with TOLE_WEB_SEARCH_URL.
    reg.register(Box::new(tole_core::web::WebFetchTool))
        .map_err(|e| anyhow::anyhow!("registering web_fetch: {e}"))?;
    if tole_core::web::WebSearchTool::from_env().is_some() {
        reg.register(Box::new(tole_core::web::WebSearchTool::from_env().unwrap()))
            .map_err(|e| anyhow::anyhow!("registering web_search: {e}"))?;
    }
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
        tole_cli::session_host::register_gitea(&mut reg, &cwd);
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
        let run_cmd = RunCommandTool::new(cwd.clone());
        let run_cmd = if std::env::var("TOLE_AGENT_DEPTH")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
            >= 1
        {
            run_cmd.in_child_agent_mode()
        } else {
            run_cmd
        };
        reg.register(Box::new(run_cmd))
            .map_err(|e| anyhow::anyhow!("registering run_command: {e}"))?;
        let job_start = JobStartTool::new(file_root.clone());
        let job_start = if std::env::var("TOLE_AGENT_DEPTH")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
            >= 1
        {
            job_start.in_child_agent_mode()
        } else {
            job_start
        };
        reg.register(Box::new(job_start))
            .map_err(|e| anyhow::anyhow!("registering job_start: {e}"))?;
        reg.register(Box::new(JobPollTool::new(file_root.clone())))
            .map_err(|e| anyhow::anyhow!("registering job_poll: {e}"))?;
    }
    reg.register(Box::new(VerifyPackageTool::new()))
        .map_err(|e| anyhow::anyhow!("registering verify_package: {e}"))?;
    // systemone_decide (#172): probe-gated — present iff SYSTEMONE_API_KEY
    // is set; absent key = silently off (owner decision 2026-10-05).
    if let Some(t) = tole_core::systemone::SystemOneTool::from_env() {
        reg.register(Box::new(t))
            .map_err(|e| anyhow::anyhow!("registering systemone_decide: {e}"))?;
    }
    reg.register(Box::new(ReadFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering read_file: {e}"))?;
    // Web tools (issue #215): probe-first — fetch always; search only
    // with TOLE_WEB_SEARCH_URL.
    reg.register(Box::new(tole_core::web::WebFetchTool))
        .map_err(|e| anyhow::anyhow!("registering web_fetch: {e}"))?;
    if tole_core::web::WebSearchTool::from_env().is_some() {
        reg.register(Box::new(tole_core::web::WebSearchTool::from_env().unwrap()))
            .map_err(|e| anyhow::anyhow!("registering web_search: {e}"))?;
    }
    reg.register(Box::new(WriteFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering write_file: {e}"))?;
    reg.register(Box::new(EditFileTool::new(file_root.clone())))
        .map_err(|e| anyhow::anyhow!("registering edit_file: {e}"))?;
    #[cfg(feature = "shell-tools")]
    {
        let gh_repo = detect_github_repo(&cwd).unwrap_or_else(|| "codecoradev/tole".into());
        reg.register(Box::new(GhTool::new(gh_repo)))
            .map_err(|e| anyhow::anyhow!("registering gh: {e}"))?;
        tole_cli::session_host::register_gitea(&mut reg, &cwd);
        reg.register(Box::new(GitTool::new().in_dir(cwd.clone())))
            .map_err(|e| anyhow::anyhow!("registering git: {e}"))?;
    }
    // delete_file (Destructive) is deliberately NOT registered: behind a
    // non-interactive approver the registry refuses it structurally.
    eprintln!("tole mcp: {} tool(s) registered", count(&reg));
    Ok(reg)
}

/// Server-mode registry for the MCP-over-HTTP host (#137): the same
/// hardened tools as stdio MCP (D1); the session tools join separately
/// via RegistryServer::with_extra_tools.
#[cfg(all(feature = "mcp-http", feature = "shell-tools"))]
fn build_server_registry_for_mcp(_plan_mode: bool) -> Result<ToolRegistry> {
    // Empty allowlist: the session tools carry their own approver per
    // session; registry Write tools stay pre-auth-off (deny by default).
    build_server_registry(None, &[])
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
fn mcp_server_command(
    workspace: Option<&String>,
    allow_patterns: &[String],
    plan_mode: bool,
) -> Result<()> {
    let registry = build_server_registry(workspace, allow_patterns)?;
    // Plan mode (issue #109) applies to server mode too (cora scan-3
    // #9): serve read-only tools only when the operator asked for it.
    let mut registry = registry;
    if plan_mode {
        let mut reg = registry;
        reg.retain_read_only();
        registry = reg;
    }
    tokio::runtime::Runtime::new()
        .context("creating tokio runtime")?
        .block_on(tole_core::mcp_server::serve_stdio(registry))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Skills wiring (issue #161): shared by run/chat/serve. Loads `--skill`
/// files (loud error on broken ones), and when the workspace exposes a
/// non-empty skills dir, registers the ReadOnly load_skill tool and
/// returns the index/section block to append to the system prompt.
/// `--no-skills` short-circuits everything.
///
/// Discovery root: an explicit `--workspace` wins; absent flags fall
/// back to the CURRENT DIRECTORY — the same default the file-tools
/// jail uses (found by the 2026-10-05 sweep: a project with
/// `<cwd>/skills/` and no flag silently got no discovery).
fn apply_skills(
    skills: &[PathBuf],
    workspace: Option<&String>,
    no_skills: bool,
    registry: &mut ToolRegistry,
) -> Result<String> {
    if no_skills {
        return Ok(String::new());
    }
    let ws_root = resolve_workspace_root(workspace)?;
    apply_skills_in(skills, Some(ws_root), false, registry)
}

/// Testable core of [`apply_skills`] taking the RESOLVED discovery
/// root (None = caller already resolved to cwd; kept Option so tests
/// can pass a tmpdir without changing the process cwd).
fn apply_skills_in(
    skills: &[PathBuf],
    ws_root: Option<PathBuf>,
    no_skills: bool,
    registry: &mut ToolRegistry,
) -> Result<String> {
    let mut sections = String::new();
    if no_skills {
        return Ok(sections);
    }
    let ws_path = ws_root;
    for path in skills {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("--skill {}: {e}", path.display()))?;
        let name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_string();
        let skill = tole_core::skills::parse_skill(&content, &name)
            .map_err(|e| anyhow::anyhow!("--skill {}: {e}", path.display()))?;
        sections.push_str(&format!("\n\n# Skill: {}\n{}", skill.name, skill.body));
    }
    if !tole_core::skills::available_skills(ws_path.as_deref()).is_empty() {
        sections.push_str(&tole_core::skills::index_block(ws_path.as_deref()));
        registry
            .register(Box::new(tole_core::skills::LoadSkillTool::new(ws_path)))
            .map_err(|e| anyhow::anyhow!("registering load_skill: {e}"))?;
    }
    Ok(sections)
}

#[allow(clippy::too_many_arguments)]
fn run_command(
    sessions_dir: &Path,
    prompt: &str,
    system: Option<&str>,
    allow_patterns: &[String],
    yes: bool,
    host: &HostConfig,
    name: Option<&str>,
    timeout_secs: Option<u64>,
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
    let mut registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        &mcp_cfgs,
        host.agents_worktree,
        allow_patterns,
    )?;
    #[cfg(not(feature = "mcp"))]
    let mut registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        host.agents_worktree,
        allow_patterns,
    )?;
    // Plan mode (issue #109): the guarantee is ABSENCE on the wire, not
    // approval — filtered tools never appear in specs().
    if host.plan_mode {
        registry.retain_read_only();
    }
    // Skills (issue #161): --skill files + discovery; load_skill registers ReadOnly.
    let skill_sections = apply_skills(
        &host.skills,
        host.workspace.as_ref(),
        host.no_skills,
        &mut registry,
    )?;
    // Opt-in tool-boundary hooks (issue #110): deny-only policy
    // injection for Write/Destructive calls, default OFF.
    #[cfg(feature = "shell-tools")]
    if !host.on_pretool.is_empty() || !host.on_posttool.is_empty() || !host.on_turnend.is_empty() {
        let mut hooks = tole_core::hooks::ToolHooks::from_cli(&host.on_pretool, &host.on_posttool);
        hooks.turnend = host
            .on_turnend
            .iter()
            .map(|c| tole_core::hooks::turnend_hook(c))
            .collect();
        registry.set_hooks(hooks);
    }

    let session_id = new_session_id();
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    let system_prompt = system
        .map(str::to_string)
        .or_else(resolve_system_prompt)
        .or_else(|| Some(build_default_prompt(host.plan_mode)));
    // Skills (issue #161): skill sections append after the base prompt.
    let system_prompt = system_prompt.map(|p| format!("{p}{skill_sections}"));
    let mut storage = JsonlStorage::create_named(
        sessions_dir,
        &session_id,
        None,
        system_prompt.as_deref(),
        name,
    )
    .with_context(|| format!("creating session {session_id}"))?;
    println!("session: {session_id}");
    if let Some(alias) = name {
        println!("name: {alias}");
    }
    // Task-list tools (issue #198): fresh session → empty state; both
    // tools join the registry (todo_write absent in plan mode via the
    // retain_read_only filter above — registration here is additive).
    let todo_state = tole_core::todo::TodoState::new();
    registry
        .register(Box::new(tole_core::todo::TodoReadTool::new(
            std::sync::Arc::clone(&todo_state),
        )))
        .map_err(|e| anyhow::anyhow!("registering todo_read: {e}"))?;
    if !host.plan_mode {
        registry
            .register(Box::new(tole_core::todo::TodoWriteTool::new(
                std::sync::Arc::clone(&todo_state),
            )))
            .map_err(|e| anyhow::anyhow!("registering todo_write: {e}"))?;
    }

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
    // Wall-clock cap (issue #216): a timer thread cancels the turn's
    // token at the deadline; the loop's checkpoints unwind it into a
    // durable Cancelled — resumable, never dead.
    let cancel = tole_core::cancel::CancelToken::default();
    let timer = timeout_secs.map(|secs| {
        let token = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            token.cancel();
        })
    });
    let outcome = run_turn_with_cancel(&mut storage, &mut provider, &registry, &prompt, &cancel)?;
    // Join ONLY when the timer fired (cora MAJOR): join otherwise blocks
    // for the remaining budget after the turn already finished. Detach —
    // the process exits right after run_command anyway.
    if timer.is_some() && cancel.is_cancelled() {
        if let Some(t) = timer {
            t.join().ok();
        }
    }
    // Memory loop, post-session: a settled Final turn leaves a compact
    // summary behind for the next session's recall.
    #[cfg(feature = "shell-tools")]
    if let TurnOutcome::Final { text, wrote } = &outcome {
        host.remember(&session_id, &raw_prompt, text, *wrote);
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
    // Resolve by NAME first (issue #216): an alias from `run --name`
    // is accepted anywhere an id is; exact ids still win when the file
    // exists directly.
    let id = if valid_session_id(id) && session_path(sessions_dir, id).exists() {
        id.to_string()
    } else {
        let mut best: Option<(std::time::SystemTime, String)> = None;
        for entry in std::fs::read_dir(sessions_dir)
            .with_context(|| format!("listing {}", sessions_dir.display()))?
            .flatten()
        {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = file_name.strip_suffix(".jsonl") else {
                continue;
            };
            if !valid_session_id(stem) {
                continue;
            }
            let Ok(s) = JsonlStorage::open(entry.path()) else {
                continue;
            };
            if s.session_name() == Some(id) {
                let mtime = entry.metadata().ok().and_then(|mm| mm.modified().ok());
                match &best {
                    Some((t, _)) if mtime.map(|mt| mt >= *t) != Some(true) => {}
                    _ => best = Some((mtime.unwrap_or(std::time::UNIX_EPOCH), stem.to_string())),
                }
            }
        }
        match best {
            Some((_, resolved)) => resolved,
            None => anyhow::bail!(
                "no session with id or name {id:?} in {}",
                sessions_dir.display()
            ),
        }
    };
    let path = session_path(sessions_dir, &id);
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
    let mut registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        &mcp_cfgs,
        host.agents_worktree,
        allow_patterns,
    )?;
    #[cfg(not(feature = "mcp"))]
    let mut registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        host.agents_worktree,
        allow_patterns,
    )?;
    // Task-list tools (issue #198): state hydrated from the replayed
    // transcript so a resumed mission keeps its plan. Registered BEFORE
    // the plan-mode filter — todo_write must be ABSENT on the wire under
    // --plan-mode (the retain_read_only guarantee), not merely gated.
    {
        let todo_state = tole_core::todo::TodoState::new();
        {
            use tole_core::storage::Storage;
            todo_state.hydrate(storage.entries());
        }
        registry
            .register(Box::new(tole_core::todo::TodoReadTool::new(
                std::sync::Arc::clone(&todo_state),
            )))
            .map_err(|e| anyhow::anyhow!("registering todo_read: {e}"))?;
        registry
            .register(Box::new(tole_core::todo::TodoWriteTool::new(
                std::sync::Arc::clone(&todo_state),
            )))
            .map_err(|e| anyhow::anyhow!("registering todo_write: {e}"))?;
    }
    // Plan mode (issue #109): the guarantee is ABSENCE on the wire, not
    // approval — filtered tools never appear in specs().
    if host.plan_mode {
        registry.retain_read_only();
    }
    // Skills (issue #161): --skill files + discovery; load_skill registers ReadOnly.
    let _skill_sections = apply_skills(
        &host.skills,
        host.workspace.as_ref(),
        host.no_skills,
        &mut registry,
    )?;
    // Opt-in tool-boundary hooks (issue #110): deny-only policy
    // injection for Write/Destructive calls, default OFF.
    #[cfg(feature = "shell-tools")]
    if !host.on_pretool.is_empty() || !host.on_posttool.is_empty() || !host.on_turnend.is_empty() {
        let mut hooks = tole_core::hooks::ToolHooks::from_cli(&host.on_pretool, &host.on_posttool);
        hooks.turnend = host
            .on_turnend
            .iter()
            .map(|c| tole_core::hooks::turnend_hook(c))
            .collect();
        registry.set_hooks(hooks);
    }
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
            if let TurnOutcome::Final {
                text: answer,
                wrote,
            } = &outcome
            {
                host.remember(&id, text, answer, *wrote);
            }
            outcome
        }
        _ => resume_turn(&mut storage, &mut provider, &registry)?,
    };
    report_outcome(&id, outcome);
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
    // Mission cost report (issue #201): the durable fact/mission register
    // a settled mission leaves behind.
    if let Some(mission) = storage.get_register("fact", "mission") {
        println!("mission: {}", mission);
    }
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
    // (epoch_secs, id, pc, seq, turns, name) — epoch secs first so a
    // plain sort_by_key ascending gives newest-first via Reverse.
    let mut rows: Vec<(u64, String, String, u64, usize, Option<String>)> = Vec::new();
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
            s.session_name().map(str::to_string),
        ));
    }
    if rows.is_empty() {
        println!("no sessions in {}", sessions_dir.display());
        return Ok(());
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0)); // newest first
    println!(
        "{:<26} {:<18} {:<12} {:>5} {:>6}  mtime",
        "session", "name", "pc", "seq", "turns"
    );
    for row in &rows {
        let mtime = fmt_mtime(std::time::UNIX_EPOCH + std::time::Duration::from_secs(row.0));
        let name = row.5.clone().unwrap_or_else(|| "-".to_string());
        println!(
            "{:<26} {:<18} {:<12} {:>5} {:>6}  {mtime}",
            row.1, name, row.2, row.3, row.4
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
    format!("{} {h:02}:{m:02}", civil_from_days(days as i64))
}

/// Civil date `YYYY-mm-dd` from days since the Unix epoch (Howard
/// Hinnant's algorithm — no chrono dep).
fn civil_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}")
}

/// Today's UTC date, `YYYY-mm-dd`.
fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86400) as i64)
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
/// Register `todo_read`/`todo_write` with state hydrated from `entries`
/// (issue #276). `todo_write` is skipped in plan mode so it stays absent
/// on the wire, matching the `retain_read_only` guarantee.
fn register_todo_tools(
    registry: &mut ToolRegistry,
    entries: &[tole_core::entry::Entry],
    plan_mode: bool,
) -> Result<()> {
    let todo_state = tole_core::todo::TodoState::new();
    todo_state.hydrate(entries);
    registry
        .register(Box::new(tole_core::todo::TodoReadTool::new(
            std::sync::Arc::clone(&todo_state),
        )))
        .map_err(|e| anyhow::anyhow!("registering todo_read: {e}"))?;
    if !plan_mode {
        registry
            .register(Box::new(tole_core::todo::TodoWriteTool::new(
                std::sync::Arc::clone(&todo_state),
            )))
            .map_err(|e| anyhow::anyhow!("registering todo_write: {e}"))?;
    }
    Ok(())
}

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
    let mut registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        &mcp_cfgs,
        host.agents_worktree,
        allow_patterns,
    )?;
    #[cfg(not(feature = "mcp"))]
    let mut registry = build_registry(
        build_approver(allow_patterns, yes),
        host.workspace.as_ref(),
        host.agents_worktree,
        allow_patterns,
    )?;
    // Plan mode (issue #109): the guarantee is ABSENCE on the wire, not
    // approval — filtered tools never appear in specs().
    if host.plan_mode {
        registry.retain_read_only();
    }
    // Skills (issue #161): --skill files + discovery; load_skill registers ReadOnly.
    let skill_sections = apply_skills(
        &host.skills,
        host.workspace.as_ref(),
        host.no_skills,
        &mut registry,
    )?;
    // Opt-in tool-boundary hooks (issue #110): deny-only policy
    // injection for Write/Destructive calls, default OFF.
    #[cfg(feature = "shell-tools")]
    if !host.on_pretool.is_empty() || !host.on_posttool.is_empty() || !host.on_turnend.is_empty() {
        let mut hooks = tole_core::hooks::ToolHooks::from_cli(&host.on_pretool, &host.on_posttool);
        hooks.turnend = host
            .on_turnend
            .iter()
            .map(|c| tole_core::hooks::turnend_hook(c))
            .collect();
        registry.set_hooks(hooks);
    }

    if fresh {
        std::fs::create_dir_all(sessions_dir)
            .with_context(|| format!("creating {}", sessions_dir.display()))?;
    }
    let path = session_path(sessions_dir, &session_id);
    let mut storage = if fresh {
        let system_prompt = system
            .map(str::to_string)
            .or_else(resolve_system_prompt)
            .or_else(|| Some(build_default_prompt(host.plan_mode)));
        // Skills (issue #161): sections append after the base prompt.
        let system_prompt = system_prompt.map(|p| format!("{p}{skill_sections}"));
        JsonlStorage::create_with(sessions_dir, &session_id, None, system_prompt.as_deref())
            .with_context(|| format!("creating session {session_id}"))?
    } else {
        JsonlStorage::open(&path).context("replaying session log")?
    };
    // Task-list tools (issues #198/#276): per-session state hydrated from
    // the replayed transcript so a resumed chat keeps its plan.
    {
        use tole_core::storage::Storage;
        register_todo_tools(&mut registry, storage.entries(), host.plan_mode)?;
    }
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
    // Issue #143: did any turn of this chat execute a Write/Destructive tool?
    #[cfg(feature = "shell-tools")]
    let mut chat_wrote = false;
    // Set when the typed message could not run because the session was
    // stuck mid-flight and the bounded resolve retries ran out — the
    // message is NOT in the durable log, so the operator must resend it.
    // SCOPED PER MESSAGE (cora scan-3 #8): declared inside the loop —
    // an outer flag never reset, so one drop warned forever after.
    let stdin = std::io::stdin();
    loop {
        let mut dropped_message = false;
        print!("you>");
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
                        Ok(other) => {
                            // UnknownTool / BudgetExhausted / LoopDetected
                            // / Storage: the typed message never reached
                            // the durable log (run_turn was never
                            // reached). Flag it so the operator gets the
                            // not-recorded note below (cora full-scan
                            // #7 — silently continuing would let them
                            // believe the input was recorded).
                            dropped_message = true;
                            break Ok(other);
                        }
                        Err(e) => {
                            // Storage-level failure resolving: do not lose
                            // the user's message — report and keep the
                            // input buffered for the next attempt.
                            dropped_message = true;
                            break Err(e);
                        }
                    }
                }
            }
        };

        match outcome {
            Ok(TurnOutcome::Final { text, wrote }) => {
                #[cfg(feature = "shell-tools")]
                {
                    last_answer = Some(text.clone());
                    chat_wrote |= wrote;
                }
                #[cfg(not(feature = "shell-tools"))]
                let _ = wrote;
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
            Ok(TurnOutcome::StopGateBlocked { reason }) => {
                eprintln!(
                    "tole> (stop gate blocked: {reason} — turn aborted; next message resumes)"
                )
            }
            Ok(TurnOutcome::Cancelled) => {
                eprintln!("tole> (turn cancelled — next message resumes)")
            }
            Ok(TurnOutcome::LoopDetected { .. }) => eprintln!(
                "tole> (loop guard tripped — identical tool calls repeated; next message resumes)"
            ),
            Ok(TurnOutcome::Storage(e)) => anyhow::bail!("storage error: {e}"),
            Err(e) => {
                // Resolve-path failure: the typed message was NOT
                // recorded. Print the same not-recorded note the
                // dropped_message path uses, then surface the error
                // (previously this bailed silently on the note).
                eprintln!("tole> (note: the message you just typed was NOT recorded — resolve the session state, then resend it)");
                anyhow::bail!("turn failed: {e}");
            }
        }
        if dropped_message {
            // The typed message never reached the durable log — saying
            // "your next message resumes" alone would let the operator
            // believe it was recorded (CodeCora scan 2026-09-18).
            eprintln!("tole> (note: the message you just typed was NOT recorded — resolve the session state, then resend it)");
        }
        // #143 (cora): a writing turn that ABORTED (provider failure,
        // loop guard, budget) still wrote to disk — the durable
        // session-scoped register survives the abort, so fold it in here
        // too, not only on the Final arm.
        #[cfg(feature = "shell-tools")]
        if storage
            .get_register("fact", "wrote_this_turn")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            chat_wrote = true;
        }
    }
    // Memory loop, post-session: a clean REPL exit with at least one
    // completed turn leaves a compact summary in the namespace.
    #[cfg(feature = "shell-tools")]
    if let (Some(fp), Some(la)) = (first_prompt.as_deref(), last_answer.as_deref()) {
        host.remember(&session_id, fp, la, chat_wrote);
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
cannot do (pipes, builds, process control). Before running any package \
install (cargo add, bun add, npm install), verify the package name exists \
with the verify_package tool — hallucinated package names are a real \
supply-chain attack vector. Keep answers concise."
}

/// Same default without shell tools: `run_command` is not registered in
/// this profile, so the prompt must not advertise it.
#[cfg(not(feature = "shell-tools"))]
fn default_system_prompt() -> &'static str {
    "You are tole, a careful personal assistant. Tool discipline: for anything \
involving files, prefer the dedicated tools — read_file, write_file, \
edit_file. Before running any package install, verify the package name \
exists with the verify_package tool. Keep answers concise."
}

/// The default prompt for the session's mode (issue #109). Plan mode
/// appends the read-only instruction to the SAME incumbent text — the
/// identity/tool-discipline section is shared, so the non-plan default
/// never drifts from what the replay scorer greps out of this file.
pub fn default_prompt_for(plan_mode: bool) -> String {
    let base = default_system_prompt();
    if plan_mode {
        format!(
            "{base} PLAN MODE: only read-only tools exist in this \
session; explore and produce a plan — mutation tools are absent \
until the session is started without --plan-mode."
        )
    } else {
        base.to_string()
    }
}

/// Default prompt for the session's mode + dynamic context sections
/// (issues #109 + #111): the shared incumbent text, the plan-mode
/// read-only sentence when planning, then a ONE-LINE context section
/// (working directory, today's UTC date). ONE context line MAXIMUM —
/// no env dumps, no fingerprints. Session start only: the assembled
/// prompt is pinned in the session header, so within a session the
/// wire body stays append-only (KV-cache prefix property untouched).
fn build_default_prompt(plan_mode: bool) -> String {
    let mut p = default_prompt_for(plan_mode);
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    p.push_str(&format!(
        "\nContext: working directory {cwd}; today is {} (UTC).",
        today_utc()
    ));
    p
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Human-readable exit summary. Non-Final outcomes exit non-zero so
/// scripts notice.
fn report_outcome(session_id: &str, outcome: TurnOutcome) {
    match outcome {
        TurnOutcome::Final { text, .. } => {
            println!("{text}");
        }
        TurnOutcome::StopGateBlocked { reason } => {
            eprintln!(
                "tole: stop gate blocked the turn: {reason} (resume with: tole resume {session_id})"
            );
            std::process::exit(6);
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
        TurnOutcome::Cancelled => {
            eprintln!("tole: turn cancelled (resume with: tole resume {session_id})");
            std::process::exit(8);
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
            tole_cli::session_host::github_repo_from_remote_url("https://github.com/foo/bar.git")
                .as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            tole_cli::session_host::github_repo_from_remote_url("https://github.com/foo/bar")
                .as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            tole_cli::session_host::github_repo_from_remote_url("git@github.com:foo/bar.git")
                .as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            tole_cli::session_host::github_repo_from_remote_url(
                "https://user:token@github.com/Foo/Bar.git"
            )
            .as_deref(),
            Some("Foo/Bar")
        );
    }

    #[test]
    fn rejects_non_github_and_garbage() {
        assert!(tole_cli::session_host::github_repo_from_remote_url(
            "https://gitlab.com/foo/bar.git"
        )
        .is_none());
        assert!(tole_cli::session_host::github_repo_from_remote_url(
            "https://github.com/only-owner"
        )
        .is_none());
        assert!(tole_cli::session_host::github_repo_from_remote_url("not a url").is_none());
        assert!(tole_cli::session_host::github_repo_from_remote_url(
            "https://github.com/-bad/name"
        )
        .is_none());
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
mod session_id_rule_tests {
    /// Issue #295: the CLI (`--resume`/`status`) and serve/ACP must agree
    /// on every id, including uppercase and `_`.
    #[test]
    fn cli_and_serve_acp_session_id_rules_agree() {
        let long = "a".repeat(65);
        let cases = [
            ("s-abc123", true),
            ("Session_ID-1", true),
            ("UPPER", true),
            ("under_score", true),
            ("", false),
            ("..", false),
            ("../evil", false),
            ("a/b", false),
            ("a\\b", false),
            (long.as_str(), false),
        ];
        for (id, ok) in cases {
            assert_eq!(super::valid_session_id(id), ok, "valid_session_id({id:?})");
            assert_eq!(
                tole_cli::session_host::validate_session_id(id).is_some(),
                ok,
                "validate_session_id({id:?})"
            );
        }
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

    #[test]
    fn plan_mode_prompt_extends_the_incumbent_without_touching_it() {
        let plan = super::default_prompt_for(true);
        let base = super::default_system_prompt();
        // The incumbent text is untouched and still the prefix...
        assert!(plan.starts_with(base));
        // ...with the read-only instruction appended.
        assert!(plan.contains("PLAN MODE"));
        assert!(plan.contains("read-only"));
        // Non-plan default is byte-identical to the incumbent fn.
        assert_eq!(super::default_prompt_for(false), base);
    }

    #[test]
    fn context_sections_append_date_and_cwd_without_mutating_the_mode_prompt() {
        let built = super::build_default_prompt(false);
        let mode = super::default_prompt_for(false);
        assert!(built.starts_with(&mode));
        assert!(built.contains("working directory "));
        assert!(built.contains("today is "));
        // Exactly ONE appended context line.
        assert_eq!(built.matches('\n').count(), mode.matches('\n').count() + 1);
        // Date shape YYYY-mm-dd (civil-from-days output).
        let tail = built
            .rsplit("today is ")
            .next()
            .unwrap()
            .trim_end_matches(" (UTC).");
        assert_eq!(tail.len(), 10);
        assert_eq!(tail.as_bytes()[4], b'-');
        assert_eq!(tail.as_bytes()[7], b'-');
        // Plan mode composes: context rides AFTER the plan sentence.
        let planned = super::build_default_prompt(true);
        assert!(planned.contains("PLAN MODE"));
        assert!(planned.rfind("Context:").unwrap() > planned.rfind("PLAN MODE").unwrap());
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // Day 0 = 1970-01-01; leap-year boundary (2024-01-01) and a
        // mid-2026 date (computed, not guessed).
        assert_eq!(super::civil_from_days(0), "1970-01-01");
        assert_eq!(super::civil_from_days(19_723), "2024-01-01");
        assert_eq!(super::civil_from_days(20_646), "2026-07-12");
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

#[cfg(test)]
mod trust_preset_tests {
    use super::*;

    #[test]
    fn internal_expands_to_expected_patterns() {
        let pats = expand_trust(&["internal".to_string()]).unwrap();
        for want in [
            "uteke_*",
            "cora_search",
            "mcp_cora_*",
            "verify_package",
            "job_*",
            "tole_session_*",
        ] {
            assert!(
                pats.iter().any(|p| p == want),
                "internal missing {want}: {pats:?}"
            );
        }
        // must NOT include write-capable native tools
        assert!(!pats.iter().any(|p| p == "write_file"));
        assert!(!pats.iter().any(|p| p == "run_command"));
    }

    #[test]
    fn none_expands_to_empty() {
        assert!(expand_trust(&["none".to_string()]).unwrap().is_empty());
        assert!(expand_trust(&[]).unwrap().is_empty());
    }

    #[test]
    fn unknown_preset_is_a_loud_error() {
        let err = expand_trust(&["internalx".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown trust preset"));
        assert!(err.to_string().contains("internal"));
    }

    #[test]
    fn case_insensitive_and_union() {
        let pats = expand_trust(&["INTERNAL".to_string(), "read_only".to_string()]).unwrap();
        assert!(pats.iter().any(|p| p == "uteke_*"));
        assert!(pats.iter().any(|p| p == "read_file"));
    }

    #[test]
    fn with_trust_appends_preserving_user_patterns() {
        let out = with_trust(
            vec!["write_file".to_string()],
            &["uteke_*".to_string(), "cora_search".to_string()],
        );
        assert_eq!(out, vec!["write_file", "uteke_*", "cora_search"]);
    }

    #[test]
    fn trust_env_used_when_flag_absent() {
        let flags = resolve_trust_flags(&[], Some("internal".to_string()));
        assert_eq!(flags, vec!["internal"]);
        // commas and whitespace both separate presets
        let flags = resolve_trust_flags(&[], Some("internal, read_only".to_string()));
        assert_eq!(flags, vec!["internal", "read_only"]);
    }

    #[test]
    fn trust_flag_wins_over_env() {
        let flags = resolve_trust_flags(&["read_only".to_string()], Some("internal".to_string()));
        assert_eq!(flags, vec!["read_only"], "explicit flag replaces the env");
    }

    #[test]
    fn trust_env_end_to_end_expands() {
        // the dispatch path: env-only resolves, then expands
        let flags = resolve_trust_flags(&[], Some(" internal ".to_string()));
        let pats = expand_trust(&flags).unwrap();
        assert!(pats.iter().any(|p| p == "uteke_*"));
    }

    #[test]
    fn trust_env_typo_is_a_loud_error() {
        let flags = resolve_trust_flags(&[], Some("internalx".to_string()));
        let err = expand_trust(&flags).unwrap_err();
        assert!(err.to_string().contains("unknown trust preset"));
    }
}

#[cfg(test)]
mod skills_wiring_tests {
    use super::*;

    #[test]
    fn apply_skills_loads_explicit_skill_files_and_registers_tool() {
        let dir = std::env::temp_dir().join(format!("tole-skills-wire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("skills").join("demo")).unwrap();
        std::fs::write(
            dir.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\n---\nbody",
        )
        .unwrap();
        let mut reg = ToolRegistry::new();
        let skill_file = dir.join("skills").join("demo").join("SKILL.md");
        let sections = apply_skills(
            &[skill_file],
            Some(&dir.to_string_lossy().to_string()),
            false,
            &mut reg,
        )
        .unwrap();
        assert!(sections.contains("# Skill: demo"));
        assert!(sections.contains("body"));
        assert!(reg.get("load_skill").is_some(), "load_skill registered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_skills_no_skills_short_circuits() {
        let dir = std::env::temp_dir().join(format!("tole-skills-off-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("skills").join("demo")).unwrap();
        std::fs::write(
            dir.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\n---\nbody",
        )
        .unwrap();
        let mut reg = ToolRegistry::new();
        let sections = apply_skills(
            &[],
            Some(&dir.to_string_lossy().to_string()),
            true,
            &mut reg,
        )
        .unwrap();
        assert!(sections.is_empty());
        assert!(
            reg.get("load_skill").is_none(),
            "--no-skills must not register"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F4 (2026-10-05): discovery must work from the RESOLVED root —
    /// `apply_skills` with no --workspace now feeds the cwd in here, the
    /// same default the file-tools jail uses. Pin the resolved-root
    /// path (no explicit --skill files, discovery only).
    #[test]
    fn apply_skills_in_discovers_from_resolved_root() {
        let dir = std::env::temp_dir().join(format!("tole-skills-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("skills").join("demo")).unwrap();
        std::fs::write(
            dir.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\n---\nbody",
        )
        .unwrap();
        let mut reg = ToolRegistry::new();
        let sections = apply_skills_in(&[], Some(dir.clone()), false, &mut reg).unwrap();
        assert!(
            sections.contains("demo"),
            "discovery index must list the skill: {sections}"
        );
        assert!(reg.get("load_skill").is_some(), "load_skill registered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_workspace_root_none_defaults_to_cwd() {
        let root = resolve_workspace_root(None).unwrap();
        assert_eq!(root, std::env::current_dir().unwrap());
    }
}

#[cfg(all(test, feature = "mcp"))]
mod server_face_flag_tests {
    use super::*;

    #[test]
    fn clean_flags_pass() {
        assert!(check_client_session_flags("serve", &[], false, false).is_ok());
    }

    #[test]
    fn skill_flag_bails_loudly() {
        let err =
            check_client_session_flags("mcp", &[PathBuf::from("/tmp/SKILL.md")], false, false)
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--skill"), "{msg}");
        assert!(msg.contains("tole mcp"), "{msg}");
    }

    #[test]
    fn no_skills_flag_bails_loudly() {
        let err = check_client_session_flags("acp", &[], true, false).unwrap_err();
        assert!(err.to_string().contains("--no-skills"));
    }

    #[test]
    fn explicit_mcp_server_bails_loudly() {
        let err = check_client_session_flags("serve", &[], false, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--mcp-server"), "{msg}");
        assert!(msg.contains("tole serve"), "{msg}");
    }
}

#[cfg(test)]
mod chat_todo_tests {
    use super::*;

    /// Issue #276 regression: the chat face registers both todo tools
    /// (and only todo_read in plan mode).
    #[test]
    fn chat_registers_todo_tools_and_respects_plan_mode() {
        let mut reg =
            ToolRegistry::with_approver(tole_core::approval::AllowlistApprover::allow_only(vec![]));
        register_todo_tools(&mut reg, &[], false).unwrap();
        assert!(reg.get("todo_read").is_some());
        assert!(reg.get("todo_write").is_some());
        let mut plan = ToolRegistry::new();
        register_todo_tools(&mut plan, &[], true).unwrap();
        assert!(plan.get("todo_read").is_some());
        assert!(plan.get("todo_write").is_none());
    }
}
