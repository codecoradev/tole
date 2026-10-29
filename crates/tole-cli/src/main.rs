//! CLI host for tole-core: arg parsing and session wiring.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use tole_cli::approver::{InteractiveApprover, StdioPrompt};
// The binary probe backs the uteke memory loop and the cora MCP preset.
#[cfg(any(feature = "shell-tools", feature = "mcp"))]
use tole_cli::tool_roster::binary_available;
use tole_cli::tool_roster::{self, AgentTools, Roster};
use tole_cli::trust::expand_trust;
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
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
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{
    resume_turn_with_cancel, run_turn_with_cancel, TurnOutcome, LOOP_TRIP_AFTER,
};

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
    /// (uteke room: repo-<dir>, override: TOLE_MEMORY_NAMESPACE). Falls
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
    /// read; `workspace` = `read_only` plus write_file/edit_file, which
    /// are jailed to the workspace (#437). Repeatable; flag wins over the TOLE_TRUST env; `none`
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

    /// Use this project config file instead of `<cwd>/.tole/config.toml`
    /// (#208). A path named on the command line is user intent, so it is
    /// trusted without a trust record. Precedence: flag > env > config >
    /// user config (`$CODECORA_HOME/tole/config.toml`) > default. Also selects the file for `tole config check|trust|untrust`.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Ignore the project config file AND the user config (#208, #433): no
    /// discovery, no trust check, no output.
    #[arg(long, global = true, conflicts_with = "config")]
    no_config: bool,

    /// Compact the conversation between turns once a prompt reaches this
    /// fraction of the model's context window (#440; `chat`, `resume`,
    /// `mission`). Default 0.8. The window comes from TOLE_CONTEXT_WINDOW /
    /// config `context_window`, else the provider's /models.
    #[arg(long, global = true, value_parser = parse_fraction)]
    compact_at: Option<f64>,

    /// Never compact the conversation (#440).
    #[arg(long, global = true)]
    no_compaction: bool,

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

        /// Per-run timeout of the --verify command, in seconds [default: 300]
        #[arg(long)]
        verify_timeout: Option<u64>,

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

        /// Base URL of the serve instance (defaults to TOLE_SERVE_URL,
        /// then http://127.0.0.1:7801).
        #[arg(long)]
        url: Option<String>,

        /// Bearer token (defaults to TOLE_SERVE_TOKEN).
        #[arg(long)]
        token: Option<String>,
    },
    /// List sessions in the sessions dir, newest first.
    Sessions {
        /// List sessions from every project (the global session index,
        /// #439) instead of this directory's sessions dir.
        #[arg(long)]
        all: bool,
        /// With --all: only sessions whose first prompt, name or cwd
        /// contains this text (case-insensitive).
        #[arg(long, requires = "all")]
        grep: Option<String>,
    },

    /// Is the configured provider/model actually usable? Tiered checks —
    /// offline (wiring), catalog (live /models), full (chat + streaming +
    /// a real tool-call loop), recorded in a coverage ledger. Exits
    /// non-zero when the requested tier is not reached, so `--tier full`
    /// is the release-validation gate.
    Doctor {
        /// offline | catalog | full (full spends a few real tokens).
        #[arg(long, default_value = "catalog")]
        tier: String,

        /// Do not record this run in the ledger.
        #[arg(long)]
        no_ledger: bool,

        /// Only print the ledger summary (READY n-of-m); run nothing.
        #[arg(long)]
        summary: bool,
    },

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
    /// Project config file `.tole/config.toml` (#208): `check` validates
    /// it and shows the effective values, `trust`/`untrust` manage the
    /// trust record. Every key is applied after trust (flag > env > config
    /// > default); `--no-config` ignores the file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

/// `tole config <action>` — an enum so later parts can add variants.
#[derive(Subcommand)]
enum ConfigAction {
    /// Validate `<cwd>/.tole/config.toml` (or --config) and print what was
    /// parsed plus the effective value and source of each applied key.
    /// Exits non-zero on any error.
    Check,
    /// Trust `<cwd>/.tole/config.toml` (or --config): shows the full
    /// content (and a diff vs the previously trusted version), then asks
    /// `[y/N]`. The approval is content-bound and stored outside the repo
    /// (`$CODECORA_HOME/tole/trusted-configs.json`).
    Trust {
        /// Skip the question (the content is still printed). Required
        /// when there is no terminal to ask on.
        #[arg(long)]
        yes: bool,
    },
    /// Forget the trust record for the config (no error if absent).
    Untrust,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli) {
        eprintln!("tole: {e:#}");
        std::process::exit(1);
    }
}

/// How the startup config gate treats a command (#208).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatePolicy {
    /// `config`, `upgrade`, `approvals`: no config is loaded.
    Skip,
    /// Interactive CLI commands: an untrusted config may be reviewed and
    /// approved at the terminal (still needs stdin AND stderr to be ttys).
    MayPrompt,
    /// Everything else (sessions/status, and the serve/acp/mcp protocol
    /// faces whose stdin/stdout are channels): fail closed, never ask.
    NoPrompt,
}

fn gate_policy(cmd: &Command) -> GatePolicy {
    match cmd {
        Command::Config { .. } | Command::Upgrade { .. } | Command::Approvals { .. } => {
            GatePolicy::Skip
        }
        // `--prompt-file -` consumes stdin as the prompt: a question on
        // the same stream would eat prompt text.
        Command::Run { prompt_file, .. } if prompt_file.as_deref() == Some("-") => {
            GatePolicy::NoPrompt
        }
        Command::Run { .. }
        | Command::Chat { .. }
        | Command::Resume { .. }
        | Command::Mission { .. } => GatePolicy::MayPrompt,
        _ => GatePolicy::NoPrompt,
    }
}

/// What the command line supplied for the config-applied keys. ONE place,
/// shared by startup and `tole config check`.
/// `--compact-at`: a fraction in (0, 1].
fn parse_fraction(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if v > 0.0 && v <= 1.0 {
        Ok(v)
    } else {
        Err(format!("must be greater than 0 and at most 1, got {v}"))
    }
}

fn config_flags(cli: &Cli) -> tole_cli::config_apply::Flags {
    let mut f = tole_cli::config_apply::Flags {
        sessions_dir: cli.sessions_dir.clone(),
        workspace: cli.workspace.clone(),
        trust: cli.trust.clone(),
        on_pretool: cli.on_pretool.clone(),
        on_posttool: cli.on_posttool.clone(),
        on_turnend: cli.on_turnend.clone(),
        skill: cli.skill.clone(),
        plan_mode: cli.plan_mode,
        no_skills: cli.no_skills,
        compact_at: cli.compact_at,
        no_compaction: cli.no_compaction,
        ..Default::default()
    };
    #[cfg(feature = "shell-tools")]
    {
        f.memory = cli.memory.clone();
    }
    #[cfg(feature = "mcp")]
    {
        f.mcp_server = cli.mcp_server.clone();
        f.no_auto_mcp = cli.no_auto_mcp;
    }
    // Every subcommand with its own `--allow` feeds it here: the config
    // `allow` list is the fallback when THAT flag list is empty.
    match &cli.command {
        Command::Run {
            system,
            allow_patterns,
            ..
        }
        | Command::Chat {
            system,
            allow_patterns,
            ..
        } => {
            f.system = system.clone();
            f.allow = allow_patterns.clone();
        }
        Command::Resume { allow_patterns, .. } => f.allow = allow_patterns.clone(),
        Command::Mission {
            max_steps,
            max_minutes,
            max_tokens,
            verify,
            verify_timeout,
            allow_patterns,
            ..
        } => {
            f.max_steps = *max_steps;
            f.max_minutes = *max_minutes;
            f.max_tokens = *max_tokens;
            f.verify = verify.clone();
            f.verify_timeout = *verify_timeout;
            f.allow = allow_patterns.clone();
        }
        #[cfg(all(feature = "mcp", feature = "shell-tools"))]
        Command::Mcp { allow_patterns, .. } => f.allow = allow_patterns.clone(),
        #[cfg(feature = "shell-tools")]
        Command::Acp { allow_patterns, .. } | Command::Serve { allow_patterns, .. } => {
            f.allow = allow_patterns.clone()
        }
        _ => {}
    }
    f
}

fn dispatch(cli: Cli) -> Result<()> {
    use tole_cli::config_apply::{self, Settings};
    // `tole config ...` is self-contained: it reads no session/env state
    // and must not trigger the update banner.
    if let Command::Config { action } = &cli.command {
        let cwd = std::env::current_dir().context("cannot determine the cwd")?;
        let explicit = cli.config.as_deref();
        return match action {
            ConfigAction::Check => {
                let user = tole_cli::config::user_config_path();
                let out = config_apply::check(
                    &cwd,
                    explicit,
                    user.as_deref(),
                    &config_flags(&cli),
                    &config_apply::real_env,
                )?;
                println!("{out}");
                // Validity != trust: the exit code stays 0 either way.
                if tole_cli::config::discover(&cwd, explicit).is_some() {
                    let (subject, _) = tole_cli::config_trust::Subject::resolve(&cwd, explicit)?;
                    let store = tole_cli::config_trust::default_store_path()?;
                    println!("{}", tole_cli::config_trust::trust_line(&subject, &store)?);
                }
                Ok(())
            }
            ConfigAction::Trust { yes } => {
                let store = tole_cli::config_trust::default_store_path()?;
                tole_cli::config_trust::trust_cmd(
                    &cwd,
                    explicit,
                    *yes,
                    &store,
                    &mut tole_cli::config_trust::StdIo,
                )
            }
            ConfigAction::Untrust => {
                let store = tole_cli::config_trust::default_store_path()?;
                tole_cli::config_trust::untrust_cmd(
                    &cwd,
                    explicit,
                    &store,
                    &mut tole_cli::config_trust::StdIo,
                )
            }
        };
    }
    // Project config (#208): gate + parse BEFORE anything else runs (no
    // update banner, no session work). `--no-config` and the skip-list
    // commands never touch the file; with no config file at all this is
    // a silent no-op.
    let policy = gate_policy(&cli.command);
    let project_config = if policy == GatePolicy::Skip || cli.no_config {
        None
    } else {
        match std::env::current_dir() {
            Ok(cwd) => config_apply::load_project_config(
                &cwd,
                cli.config.as_deref(),
                &tole_cli::config_trust::default_store_path,
                &mut config_apply::StartupIo::new(policy == GatePolicy::MayPrompt),
            )?,
            // No usable cwd: nothing to discover (as before, relative
            // paths just fail later); an explicit --config still reports.
            Err(e) if cli.config.is_some() => {
                return Err(anyhow::Error::new(e).context("cannot determine the cwd"))
            }
            Err(_) => None,
        }
    };
    // User config (#433): same commands, same `--no-config`; no trust gate.
    let user_config = if policy == GatePolicy::Skip || cli.no_config {
        None
    } else {
        config_apply::load_user_config(
            tole_cli::config::user_config_path().as_deref(),
            &mut config_apply::StartupIo::new(false),
        )?
    };
    let cfg = match (project_config, user_config) {
        (None, None) => None,
        (p, u) => Some(tole_cli::config::layered(
            &p.map(|l| l.config).unwrap_or_default(),
            &u.map(|l| l.config).unwrap_or_default(),
        )),
    };
    if let Some(cfg) = &cfg {
        // A key this build cannot honor is a loud startup error, never a
        // silent ignore (the matching flag does not exist here).
        if let Some(msg) = config_apply::unsupported_keys(
            cfg,
            cfg!(feature = "mcp"),
            cfg!(feature = "shell-tools"),
        ) {
            anyhow::bail!(msg);
        }
        config_apply::install_fill(Settings::fill_from(cfg));
    }
    let settings = Settings::resolve(
        &config_flags(&cli),
        cfg.as_ref().unwrap_or(&Default::default()),
        &config_apply::real_env,
    );
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
    // flag > config > default; the ONE resolution `sessions`, `status`,
    // `run`, `chat`, `resume` and `mission` share.
    let sessions_dir = PathBuf::from(settings.sessions_dir.value.clone());
    // The RAW override for the server faces: an explicit --sessions-dir
    // relocates serve/acp session storage; the default (None) keeps the
    // per-session-cwd layout those faces always had. The resolved
    // `sessions_dir` above stays the run/chat/sessions/status default.
    // Only the shell-tools serve/acp faces read this (#330).
    #[cfg_attr(not(feature = "shell-tools"), allow(unused_variables))]
    let sessions_dir_override = (settings.sessions_dir.source != config_apply::Source::Default)
        .then(|| PathBuf::from(settings.sessions_dir.value.clone()));
    #[cfg(feature = "mcp")]
    let mcp_specs = merge_mcp_specs(
        &config_apply::values(&settings.mcp_server),
        settings.no_auto_mcp.value,
        auto_mcp_specs(),
    );
    // Explicit servers only (--mcp-server flag or the config `mcp_server`
    // list) — the merged `mcp_specs` also contains the cora auto-preset,
    // which must NOT trigger the server-face refusal below. Always false
    // in a build without the `mcp` feature (the config key is refused at
    // startup there, and the flag does not exist).
    let explicit_mcp = settings.mcp_server.is_some();
    // scan-3 finding fix: the global --workspace/--memory flags now flow
    // into the host, so `tole serve/acp/mcp` honor them as fallbacks when
    // the subcommand-level flags are absent (previously they were
    // silently ignored by those subcommands).
    let host = HostConfig {
        workspace: settings.workspace.as_ref().map(|w| w.value.clone()),
        skills: config_apply::values(&settings.skill),
        no_skills: settings.no_skills.value,
        #[cfg(feature = "mcp")]
        mcp_server: mcp_specs,
        plan_mode: settings.plan_mode.value,
        agents_worktree: cli.agents_worktree,

        on_pretool: config_apply::values(&settings.on_pretool),
        on_posttool: config_apply::values(&settings.on_posttool),
        on_turnend: config_apply::values(&settings.on_turnend),
        #[cfg(feature = "shell-tools")]
        memory: resolve_memory(settings.memory.as_ref().map(|m| m.value.as_str()))?,
        #[cfg(not(feature = "shell-tools"))]
        memory: (),
        compaction: tole_cli::compaction_host::CompactionSettings {
            enabled: !settings.no_compaction.value,
            at: settings.compact_at.value,
            window: settings.context_window.as_ref().map(|w| w.value),
        },
    };
    // Trust presets (issue #159): expand once, before dispatch — every
    // allow_patterns-consuming subcommand appends these. --trust >
    // TOLE_TRUST > config `trust`, the higher layer replacing the whole
    // list (the env splits on commas/whitespace so one variable can carry
    // several presets).
    let trust_extra = expand_trust(&config_apply::values(&settings.trust))?;
    // `--trust workspace` (#437) needs an explicit workspace that is not a
    // home / filesystem root; resolved only when the preset is used.
    let trust_names = config_apply::values(&settings.trust);
    if trust_names
        .iter()
        .any(|p| p.eq_ignore_ascii_case("workspace"))
    {
        let root = match &settings.workspace {
            Some(w) => Some(resolve_workspace_root(Some(&w.value))?),
            None => None,
        };
        tole_cli::trust::check_workspace_root(
            &trust_names,
            root.as_deref(),
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        )?;
    }

    match cli.command {
        // Handled at the top of `dispatch`, before any session/env state.
        Command::Config { .. } => unreachable!("`tole config` returns early"),
        Command::Run {
            prompt,
            prompt_file,
            name,
            timeout,
            system: _,
            allow_patterns: _,
            yes,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
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
                // --system > TOLE_SYSTEM_PROMPT > config (already resolved).
                settings.system_prompt.as_ref().map(|e| e.value.as_str()),
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
            allow_patterns: _,
            yes,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
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
            allow_patterns: _,
            workspace,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            // Global flags must not SILENTLY no-op on this subcommand
            // (cora scan-3 #9): plan-mode filters the served registry to
            // read-only; hooks are not wired in server mode (no local
            // approver boundary — server mode pre-authorizes via
            // --allow), so --on-pretool/--on-posttool error out loudly
            // instead of being ignored.
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                return Err(refuse(
                    "--on-pretool/--on-posttool are not supported by `tole mcp` \
                     (server mode pre-authorizes Write tools with --allow instead)",
                    pretool_from_config(&settings),
                ));
            }
            if !host.on_turnend.is_empty() {
                return Err(refuse(
                    "--on-turnend is not supported by `tole mcp` (the tool server runs no \
                     turns; stop gates apply to run/chat/resume/serve/acp sessions)",
                    from_config(&settings.on_turnend),
                ));
            }
            check_server_face_flags("mcp", &host, &settings, explicit_mcp)?;
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
            let url = resolve_serve_url(url, std::env::var("TOLE_SERVE_URL").ok());
            approvals::cli(&action, id.as_deref(), &url, &token)
        }
        Command::Mission {
            goal,
            max_steps: _,
            max_minutes: _,
            max_tokens: _,
            verify: _,
            verify_timeout: _,
            resume,
            allow_patterns: _,
            yes,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            if host.plan_mode {
                return Err(refuse(
                    "--plan-mode has no meaning for a mission (missions mutate by definition)",
                    config_layer(settings.plan_mode.source),
                ));
            }
            // Issue #258 (rescan #46, scan-3 #9 rule): global flags must
            // not SILENTLY no-op — missions build their own registry and
            // run no host memory loop today, so hook/memory flags are
            // loudly refused instead of being ignored.
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                return Err(refuse(
                    "--on-pretool/--on-posttool are not supported by `tole run mission` \
                     (mission registries do not wire tool hooks yet)",
                    pretool_from_config(&settings),
                ));
            }
            if !host.on_turnend.is_empty() {
                return Err(refuse(
                    "--on-turnend is not supported by `tole run mission` (mission turns \
                     settle through the mission loop, not the turn-end hook path)",
                    from_config(&settings.on_turnend),
                ));
            }
            // #[cfg]-mirrored like the HostConfig field itself: without
            // shell-tools the field is the unit type (cora round 1).
            #[cfg(feature = "shell-tools")]
            if host.memory.is_some() {
                return Err(refuse(
                    "--memory is not supported by `tole run mission` (the mission loop does \
                     not run the harness memory loop yet)",
                    settings
                        .memory
                        .as_ref()
                        .and_then(|m| config_layer(m.source)),
                ));
            }
            check_server_face_flags("mission", &host, &settings, explicit_mcp)?;
            // Budget tier (issue #201): the internal trust preset earns
            // the trusted tier's headroom; explicit flags always win.
            let trusted = trust_extra.iter().any(|p| p == "todo_write");
            // flag > config > budget tier (no env for the budgets).
            let tier = mission::BudgetTier::resolve(
                trusted,
                settings.max_steps.as_ref().map(|e| e.value),
                settings.max_minutes.as_ref().map(|e| e.value),
                settings.max_tokens.as_ref().map(|e| e.value),
            );
            let sessions_dir = sessions_dir.clone();
            std::fs::create_dir_all(&sessions_dir)
                .with_context(|| format!("creating {}", sessions_dir.display()))?;
            let registry =
                build_registry(build_approver(&allow_patterns, yes), &host, &allow_patterns)?;
            mission::run_mission(
                mission::MissionConfig {
                    goal,
                    max_steps: tier.max_steps,
                    max_minutes: tier.max_minutes,
                    max_tokens: tier.max_tokens,
                    // flag > config; timeout flag > config > 300.
                    verify: settings.verify.as_ref().map(|v| v.value.clone()),
                    verify_timeout_secs: settings.verify_timeout.value,
                    resume_id: resume,
                    compaction: host.compaction,
                },
                registry,
                &sessions_dir,
            )
        }
        #[cfg(feature = "shell-tools")]
        Command::Acp {
            allow_patterns: _,
            yes,
            workspace,
            memory,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            // Same loud-bail rule as `tole mcp` for hooks: the ACP host
            // does not wire local pre/post hooks — approvals happen in
            // the editor via permission requests instead.
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                return Err(refuse(
                    "--on-pretool/--on-posttool are not supported by `tole acp` \
                     (approvals happen via session/request_permission in the client)",
                    pretool_from_config(&settings),
                ));
            }
            check_server_face_flags("acp", &host, &settings, explicit_mcp)?;
            if host.plan_mode {
                eprintln!("tole acp: --plan-mode is active — serving read-only tools only");
            }
            let memory = match memory.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(m) => resolve_memory(Some(m))?,
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
            allow_patterns: _,
            workspace,
            memory,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            // Loud bails, same rule as `tole mcp`/`tole acp`: these host
            // flags have no server-face wiring, and a silent no-op is
            // worse than a startup error (scan-3 #9).
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                return Err(refuse(
                    "--on-pretool/--on-posttool are not supported by `tole serve` \
                     (a server has no local human; pre-authorize Write tools with --allow)",
                    pretool_from_config(&settings),
                ));
            }
            check_server_face_flags("serve", &host, &settings, explicit_mcp)?;
            let memory = match memory.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(m) => resolve_memory(Some(m))?,
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
        Command::Sessions { all: false, .. } => sessions_command(&sessions_dir),
        Command::Sessions { all: true, grep } => all_sessions_command(grep.as_deref()),
        Command::Doctor {
            tier,
            no_ledger,
            summary,
        } => {
            if summary {
                tole_cli::doctor::print_summary().map_err(|e| anyhow!(e))?;
                return Ok(());
            }
            let Some(tier) = tole_cli::doctor::Tier::parse(&tier) else {
                bail!("--tier must be offline, catalog or full (got {tier:?})");
            };
            let cfg = config_apply::provider_config();
            let ok =
                tole_cli::doctor::run_cli(cfg.as_ref(), &config_apply::real_env, tier, no_ledger);
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Status { id } => status_command(&sessions_dir, &id),
        Command::Chat {
            system: _,
            resume,
            last,
            allow_patterns: _,
            yes,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            chat_command(
                &sessions_dir,
                settings.system_prompt.as_ref().map(|e| e.value.as_str()),
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
    #[allow(dead_code)] // placeholder field, never read in the bare profile (#330)
    memory: (),
    #[cfg(feature = "shell-tools")]
    memory: Option<tole_core::memory::MemoryConfig>,
    /// Context compaction between turns (#440).
    compaction: tole_cli::compaction_host::CompactionSettings,
}

impl HostConfig {
    fn on_pretool_non_empty(&self) -> bool {
        !self.on_pretool.is_empty()
    }
    fn on_posttool_non_empty(&self) -> bool {
        !self.on_posttool.is_empty()
    }
}

#[cfg(feature = "shell-tools")]
impl HostConfig {
    /// Pre-turn recall injection (memory loop), see
    /// [`tole_cli::turn_runner::with_recalled_memory`].
    fn inject_memory(&self, prompt: &str) -> String {
        tole_cli::turn_runner::with_recalled_memory(self.memory.as_ref(), prompt)
    }

    /// Post-session summary (memory loop), see
    /// [`tole_cli::turn_runner::remember`].
    fn remember(&self, session_id: &str, first_prompt: &str, last_answer: &str, wrote: bool) {
        tole_cli::turn_runner::remember(
            self.memory.as_ref(),
            session_id,
            first_prompt,
            last_answer,
            wrote,
        );
    }
}

/// Memory backend resolution: `chosen` is the already-resolved backend
/// name (`--memory` flag > `TOLE_MEMORY` env > project config, see
/// `config_apply::Settings`); `uteke` is the only backend. The room follows
/// the ecosystem `repo-<dir>` convention unless `TOLE_MEMORY_NAMESPACE`
/// (kept under its old name) overrides it. A missing uteke binary degrades to a warning + no-op
/// (the same probe contract as the uteke tools).
#[cfg(feature = "shell-tools")]
fn resolve_memory(chosen: Option<&str>) -> Result<Option<tole_core::memory::MemoryConfig>> {
    let chosen = chosen
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
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
    if let Ok(ns) = std::env::var(tole_core::child_env::MEMORY_ROOM) {
        let ns = ns.trim();
        if !ns.is_empty() {
            cfg.room = tole_core::memory::sanitize_namespace(ns);
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

/// Flag-over-env trust list (the pre-config rule, #159), kept as a thin
/// test oracle over the shared [`config_apply::resolve_list`]: startup
/// resolves `trust` through `Settings::resolve` (flag > TOLE_TRUST env >
/// config), whose first two layers must stay exactly this behavior.
#[cfg(test)]
fn resolve_trust_flags(flag: &[String], env_val: Option<String>) -> Vec<String> {
    let env = env_val
        .map(|v| tole_cli::config_apply::split_env_list(&v))
        .unwrap_or_default();
    tole_cli::config_apply::resolve_list(flag, &env, None)
        .map(|(v, _)| v)
        .unwrap_or_default()
}

/// Default base URL of a `tole serve` instance for `tole approvals`.
const DEFAULT_SERVE_URL: &str = "http://127.0.0.1:7801";

/// Effective serve URL for `tole approvals` (#339): an explicit `--url`
/// wins over the `TOLE_SERVE_URL` env, which wins over the default. An
/// empty env value counts as unset.
fn resolve_serve_url(flag: Option<String>, env_val: Option<String>) -> String {
    flag.or_else(|| env_val.filter(|v| !v.is_empty()))
        .unwrap_or_else(|| DEFAULT_SERVE_URL.to_string())
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
/// Ungated since #208 part 3b: a config `skill` / `no_skills` must be
/// refused on these faces in EVERY build profile, not only with `mcp`
/// (without the feature `explicit_mcp_servers` is always false).
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

/// A refusal that may stem from a config file rather than the command
/// line: the message is the flag's, plus (for config-sourced values) the
/// file it came from and the way out. A safety hook / policy that arrives
/// from a config must never be silently dropped on a face that cannot
/// honor it.
fn refuse(msg: &str, layer: Option<tole_cli::config_apply::Source>) -> anyhow::Error {
    match layer {
        Some(layer) => anyhow::anyhow!(
            "{msg} (the value comes from the {}; \
             use --no-config to ignore the config files)",
            match layer {
                tole_cli::config_apply::Source::UserConfig => "user config",
                _ => "project config",
            }
        ),
        None => anyhow::anyhow!("{msg}"),
    }
}

/// The config layer a value came from, if it came from a config file
/// (project or user, #433).
fn config_layer(s: tole_cli::config_apply::Source) -> Option<tole_cli::config_apply::Source> {
    use tole_cli::config_apply::Source;
    matches!(s, Source::Config | Source::UserConfig).then_some(s)
}

fn from_config<T>(
    e: &tole_cli::config_apply::ListEff<T>,
) -> Option<tole_cli::config_apply::Source> {
    e.as_ref().and_then(|e| config_layer(e.source))
}

fn pretool_from_config(
    s: &tole_cli::config_apply::Settings,
) -> Option<tole_cli::config_apply::Source> {
    from_config(&s.on_pretool).or_else(|| from_config(&s.on_posttool))
}

/// [`check_client_session_flags`] over the EFFECTIVE (post-config) values;
/// a config-sourced offender gets the `--no-config` hint.
fn check_server_face_flags(
    face: &str,
    host: &HostConfig,
    settings: &tole_cli::config_apply::Settings,
    explicit_mcp: bool,
) -> Result<()> {
    check_client_session_flags(face, &host.skills, host.no_skills, explicit_mcp).map_err(|e| {
        // Same order as the checks inside: skill, no_skills, mcp_server.
        let cfg = if !host.skills.is_empty() {
            from_config(&settings.skill)
        } else if host.no_skills {
            config_layer(settings.no_skills.source)
        } else {
            from_config(&settings.mcp_server)
        };
        // An exported TOLE_SKILL reaches every face: name it, or the
        // refusal would point at a --skill flag nobody typed.
        let from_env = !host.skills.is_empty()
            && settings
                .skill
                .as_ref()
                .is_some_and(|s| s.source == tole_cli::config_apply::Source::Env);
        if from_env {
            return anyhow::anyhow!(
                "{e} (the value comes from {}; unset it for this face)",
                tole_cli::config_apply::SKILL_ENV
            );
        }
        refuse(&e.to_string(), cfg)
    })
}

/// The CLI faces' registry: the shared tool roster behind the
/// interactive approver.
fn build_registry(
    approver: InteractiveApprover<StdioPrompt>,
    host: &HostConfig,
    allow_patterns: &[String],
) -> Result<ToolRegistry> {
    let mut roster = Roster::new(resolve_workspace_root(host.workspace.as_ref())?);
    // CLI faces run shell tools in the process cwd; files stay jailed to
    // the workspace.
    roster.shell_dir = std::env::current_dir().context("resolving cwd")?;
    roster.plan_mode = host.plan_mode;
    roster.agents = Some(AgentTools {
        worktrees: host.agents_worktree,
        parent_allows: allow_patterns.to_vec(),
    });
    #[cfg(feature = "mcp")]
    {
        roster.mcp_servers = host
            .mcp_server
            .iter()
            .map(|s| tole_core::mcp::McpServerConfig::parse(s))
            .collect::<Result<Vec<_>, String>>()
            .map_err(anyhow::Error::msg)?;
    }
    tool_roster::build(approver, roster)
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
    plan_mode: bool,
) -> Result<ToolRegistry> {
    let mut roster = Roster::new(resolve_workspace_root(workspace)?);
    roster.shell_dir = std::env::current_dir().context("resolving cwd")?;
    roster.plan_mode = plan_mode;
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
fn build_server_registry_for_mcp(
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
fn mcp_server_command(
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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The cora agent-as-reviewer flow (#422, cora >= 0.19): the calling
/// model judges a diff itself, so cora needs no LLM key. Named in the
/// system prompt only when both tools are registered (absent in plan
/// mode, where MCP tools are filtered out, and without `cora mcp`).
const CORA_REVIEW_SECTION: &str = "\n\n# Code review\nTo review a change: get its \
unified diff (e.g. `git diff --cached` or `git diff <base>`), call \
mcp_cora_cora_review_prepare with it, judge the diff yourself following \
the returned instructions and response_schema, then call \
mcp_cora_cora_review_finalize with the SAME diff and your findings (a JSON \
array, [] when none) and report its verdict, including should_block.";

fn review_section(registry: &ToolRegistry) -> &'static str {
    let attached = [
        "mcp_cora_cora_review_prepare",
        "mcp_cora_cora_review_finalize",
    ]
    .iter()
    .all(|name| registry.get(name).is_some());
    if attached {
        CORA_REVIEW_SECTION
    } else {
        ""
    }
}

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
    let cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;
    // Build everything that can fail BEFORE the session file exists, so a
    // failed startup does not leave a stray empty session polluting
    // `sessions` / `--last` (CodeCora scan 2026-09-18).
    let mut registry = build_registry(build_approver(allow_patterns, yes), host, allow_patterns)?;
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
    // Skills (issue #161): skill sections append after the base prompt,
    // after the cora review flow when its tools are attached (#422).
    let review = review_section(&registry);
    let system_prompt = system_prompt.map(|p| format!("{p}{review}{skill_sections}"));
    let mut storage = JsonlStorage::create_named(
        sessions_dir,
        &session_id,
        None,
        system_prompt.as_deref(),
        name,
    )
    .with_context(|| format!("creating session {session_id}"))?;
    tole_cli::session_index::record(sessions_dir, &session_id, "run");
    println!("session: {session_id}");
    if let Some(alias) = name {
        println!("name: {alias}");
    }
    // Task-list tools (issue #198): a fresh session starts with an empty list.
    tool_roster::register_todo(&mut registry, storage.entries(), host.plan_mode)?;

    let mut provider = tole_cli::turn_runner::session_provider(
        cfg,
        &storage,
        registry.specs(),
        system_prompt.as_deref(),
        None,
    )
    .map_err(anyhow::Error::msg)?;
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
    // Child agents (#380): the watcher queues sibling change notices on
    // the same token. A no-op outside a child.
    let (cancel, _sibling_watcher) = tole_cli::turn_runner::cli_turn_token();
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
        // No sessions dir here is no local match (the index may still know
        // the id, #439); any other listing error is real.
        let listing = match std::fs::read_dir(sessions_dir) {
            Ok(rd) => Some(rd),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("listing {}", sessions_dir.display()))
                )
            }
        };
        for entry in listing.into_iter().flatten().flatten() {
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
            None => {
                // Not here: the global index (#439) knows which project the
                // session belongs to. It is NOT resumed from here: this
                // process's workspace jail, project config and trust were
                // resolved for THIS directory, and the turn would act on
                // the wrong tree. Name the exact command instead, so the
                // owning project's config and trust gate apply.
                let indexed = valid_session_id(id)
                    .then(tole_cli::session_index::index_path)
                    .flatten()
                    .and_then(|idx| tole_cli::session_index::find(&idx, id));
                if let Some(e) = indexed {
                    anyhow::bail!(
                        "session {id} belongs to another project; resume it there so its \
                         workspace, config and trust apply:\n  {}",
                        tole_cli::session_index::resume_hint(&e)
                    );
                }
                anyhow::bail!(
                    "no session with id or name {id:?} in {} (nor in the session index)",
                    sessions_dir.display()
                )
            }
        }
    };
    let path = session_path(sessions_dir, &id);
    if !path.exists() {
        anyhow::bail!("session {id} not found at {}", path.display());
    }
    let cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;
    let mut storage = JsonlStorage::open(&path).context("replaying session log")?;

    let mut registry = build_registry(build_approver(allow_patterns, yes), host, allow_patterns)?;
    // Task-list tools (issue #198): state hydrated from the replayed
    // transcript so a resumed session keeps its plan; todo_write is
    // absent under --plan-mode.
    tool_roster::register_todo(&mut registry, storage.entries(), host.plan_mode)?;
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
    // B2: the system prompt is pinned in the session header — resume
    // re-applies exactly what the session was created with (never the
    // ambient env, which may have changed since).
    let pinned = storage.system_prompt().map(str::to_string);
    let mut compactor = tole_cli::compaction_host::for_session(host.compaction, &cfg, &storage);
    let mut provider = tole_cli::turn_runner::session_provider(
        cfg,
        &storage,
        registry.specs(),
        pinned.as_deref(),
        None,
    )
    .map_err(anyhow::Error::msg)?;
    // The session token: sibling notices (#380) reach a resumed child too.
    let (cancel, _sibling_watcher) = tole_cli::turn_runner::cli_turn_token();
    let outcome = match prompt {
        Some(text) if !text.trim().is_empty() => {
            // New instructions on a settled session (issue #55): the
            // machine accepts a user message at a turn boundary, so
            // reuse run_turn on the resumed storage instead of the
            // approvals-only resume protocol.
            // Context compaction (#440) between the settled turn and the
            // new instruction.
            compactor.before_turn(&mut storage, &mut *provider);
            let outcome =
                run_turn_with_cancel(&mut storage, &mut provider, &registry, text, &cancel)?;
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
        _ => resume_turn_with_cancel(&mut storage, &mut provider, &registry, &cancel)?,
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
    let turns = tole_core::transcript::turn_count(storage.entries());
    println!("turns:   {turns} (user messages)");
    for line in tole_cli::compaction_host::status_lines(storage.entries()) {
        println!("{line}");
    }
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
    // Issue #211: usage report derived from the ledger (additive block,
    // rendered after the pre-existing lines; older sessions read `n/a`).
    for line in tole_cli::usage_report::usage_report(usage).render_lines() {
        println!("{line}");
    }
    // Issue #379: per user-turn stats from the same ledger.
    for line in tole_cli::usage_report::turn_lines(storage.entries(), usage) {
        println!("{line}");
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
        let turns = tole_core::transcript::turn_count(s.entries());
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

/// `tole sessions --all` (#439): every indexed session whose file still
/// exists, newest first, with its project directory.
fn all_sessions_command(grep: Option<&str>) -> Result<()> {
    let index = tole_cli::session_index::index_path()
        .context("cannot locate the session index: set CODECORA_HOME or HOME")?;
    let rows = tole_cli::session_index::rows(&index, grep);
    if rows.is_empty() {
        println!("no indexed sessions in {}", index.display());
        return Ok(());
    }
    println!(
        "{:<26} {:<16} {:<10} {:>5}  {:<16}  cwd / first prompt",
        "session", "name", "pc", "turns", "started"
    );
    for r in &rows {
        let started =
            fmt_mtime(std::time::UNIX_EPOCH + std::time::Duration::from_secs(r.entry.created_at));
        println!(
            "{:<26} {:<16} {:<10} {:>5}  {:<16}  {}",
            r.entry.id,
            r.name.as_deref().unwrap_or("-"),
            r.pc,
            r.turns,
            started,
            r.entry.cwd.display()
        );
        if !r.preview.is_empty() {
            println!("{:<26} {}", "", r.preview);
        }
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

    let cfg = tole_cli::turn_runner::provider_config().map_err(anyhow::Error::msg)?;

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
    let mut registry = build_registry(build_approver(allow_patterns, yes), host, allow_patterns)?;
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
        // Skills (issue #161): sections append after the base prompt,
        // after the cora review flow when its tools are attached (#422).
        let review = review_section(&registry);
        let system_prompt = system_prompt.map(|p| format!("{p}{review}{skill_sections}"));
        let storage =
            JsonlStorage::create_with(sessions_dir, &session_id, None, system_prompt.as_deref())
                .with_context(|| format!("creating session {session_id}"))?;
        tole_cli::session_index::record(sessions_dir, &session_id, "chat");
        storage
    } else {
        JsonlStorage::open(&path).context("replaying session log")?
    };
    // Task-list tools (issues #198/#276): per-session state hydrated from
    // the replayed transcript so a resumed chat keeps its plan.
    {
        use tole_core::storage::Storage;
        tool_roster::register_todo(&mut registry, storage.entries(), host.plan_mode)?;
    }
    println!(
        "tole chat — session {session_id} (Ctrl-D exits, resume: tole chat --resume {session_id})"
    );

    // B2: fresh sessions pin the resolved prompt; resumed sessions re-apply
    // the header-pinned one (see create_with above / JsonlStorage::open).
    let pinned = storage.system_prompt().map(str::to_string);
    let mut compactor = tole_cli::compaction_host::for_session(host.compaction, &cfg, &storage);
    let mut provider = tole_cli::turn_runner::session_provider(
        cfg,
        &storage,
        registry.specs(),
        pinned.as_deref(),
        None,
    )
    .map_err(anyhow::Error::msg)?;
    // One token for the whole chat session: sibling notices (#380) reach
    // a child running `tole chat` at each turn's next safe point.
    let (cancel, _sibling_watcher) = tole_cli::turn_runner::cli_turn_token();

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
                    // Context compaction (#440): only here, between
                    // settled turns.
                    compactor.before_turn(&mut storage, &mut *provider);
                    break run_turn_with_cancel(
                        &mut storage,
                        &mut provider,
                        &registry,
                        &turn_prompt,
                        &cancel,
                    );
                }
                _ => {
                    eprintln!("tole> (resolving the interrupted turn…)");
                    match resume_turn_with_cancel(&mut storage, &mut provider, &registry, &cancel) {
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
                eprintln!("tole> (unknown tool '{name}' — recorded; your next message continues)")
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

    /// #352: only a host that is EXACTLY github.com counts; the string
    /// "github.com" inside a longer hostname, userinfo or the path must not
    /// redirect the gh tool to an attacker-named repo.
    #[test]
    fn host_must_be_exactly_github_com() {
        let parse = tole_cli::session_host::github_repo_from_remote_url;
        for bad in [
            "https://notgithub.com/owner/name",
            "https://github.com.evil.io/owner/name",
            "ssh://git@github.com.evil.io/o/r",
            "https://evil.io/github.com/owner/name",
            "https://gitea.example/github.com/owner/name.git",
            "https://github.com@evil.io/owner/name",
            "https://user:github.com@evil.io/owner/name",
            "ssh://git@evil.io:github.com/o/r",
            "git@evil.io:github.com/o/r",
            "git@github.com.evil.io:o/r",
            "evil.io:github.com:o/r",
            "https://githubxcom/owner/name",
            "https://gist.github.com/owner/name",
            "github.com/owner/name",
        ] {
            assert!(parse(bad).is_none(), "{bad} must not resolve to a repo");
        }
    }

    #[test]
    fn accepts_the_real_github_forms() {
        let parse = tole_cli::session_host::github_repo_from_remote_url;
        for (url, want) in [
            ("https://github.com/o/r", "o/r"),
            ("https://github.com/o/r/", "o/r"),
            ("https://GitHub.COM/o/r.git", "o/r"),
            ("https://github.com:443/o/r.git", "o/r"),
            ("https://user:tok@github.com/o/r.git", "o/r"),
            ("ssh://git@github.com/o/r.git", "o/r"),
            ("ssh://git@github.com:22/o/r.git", "o/r"),
            ("git@github.com:o/r.git", "o/r"),
            ("github.com:o/r", "o/r"),
            ("git://github.com/o/r.git", "o/r"),
        ] {
            assert_eq!(parse(url).as_deref(), Some(want), "{url}");
        }
    }

    #[test]
    fn gh_tool_not_registered_without_detected_repo() {
        // Issue #293: a non-GitHub cwd must yield NO gh tool (no
        // hardcoded fallback repo).
        let dir = std::env::temp_dir().join(format!("tole-gh-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let reg = tool_roster::build(
            tole_core::approval::AllowlistApprover::allow_only(vec![]),
            Roster::new(dir.clone()),
        )
        .unwrap();
        assert!(reg.get("gh").is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
        let detected = tole_cli::session_host::detect_github_repo(&dir);
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
    fn review_section_only_when_both_cora_review_tools_are_registered() {
        use tole_core::tool::{Risk, Tool};
        struct Named(&'static str);
        impl Tool for Named {
            fn name(&self) -> &str {
                self.0
            }
            fn risk(&self) -> Risk {
                Risk::ReadOnly
            }
            fn execute(&self, _input: serde_json::Value) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
        }
        let approver = || tole_core::approval::AllowlistApprover::allow_only(vec![]);
        let mut reg = tole_core::tool::ToolRegistry::with_approver(approver());
        assert_eq!(super::review_section(&reg), "");
        reg.register(Box::new(Named("mcp_cora_cora_review_prepare")))
            .unwrap();
        assert_eq!(super::review_section(&reg), "", "finalize missing");
        reg.register(Box::new(Named("mcp_cora_cora_review_finalize")))
            .unwrap();
        let section = super::review_section(&reg);
        assert!(section.contains("mcp_cora_cora_review_prepare"));
        assert!(section.contains("SAME diff"));
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
            "agent_poll",
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
        // agent_poll (Write since #300) is allowlisted by exact name so the
        // poll loop stays unattended; agent_start must keep prompting.
        assert!(!pats.iter().any(|p| p == "agent_*" || p == "agent_start"));
    }

    #[test]
    fn workspace_is_read_only_plus_the_jailed_file_writers() {
        let ws = expand_trust(&["workspace".to_string()]).unwrap();
        let ro = expand_trust(&["read_only".to_string()]).unwrap();
        for p in &ro {
            assert!(ws.contains(p), "workspace lacks read_only's {p}");
        }
        // The writers only through the path guard, never bare.
        assert!(ws.iter().any(|p| p == "write_file@workspace"));
        assert!(ws.iter().any(|p| p == "edit_file@workspace"));
        for never in [
            "write_file",
            "edit_file",
            "delete_file",
            "run_command",
            "git",
            "gh",
            "agent_start",
        ] {
            assert!(!ws.iter().any(|p| p == never), "{never}: {ws:?}");
        }
        assert!(!ws.iter().any(|p| p.contains('*')), "{ws:?}");
    }

    #[test]
    fn workspace_preset_refuses_home_and_filesystem_root() {
        use tole_cli::trust::check_workspace_root;
        let ws = vec!["workspace".to_string()];
        let home = std::env::temp_dir().join(format!("tole-ws-home-{}", std::process::id()));
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        assert!(check_workspace_root(&ws, Some(&project), Some(&home)).is_ok());
        let e = check_workspace_root(&ws, Some(&home), Some(&home)).unwrap_err();
        assert!(
            e.to_string().contains("your home or a filesystem root"),
            "{e}"
        );
        assert!(check_workspace_root(&ws, Some(Path::new("/")), None).is_err());
        // The implicit cwd is never enough.
        let e = check_workspace_root(&ws, None, Some(&home)).unwrap_err();
        assert!(e.to_string().contains("needs an explicit workspace"), "{e}");
        // Other presets never look at the root.
        assert!(check_workspace_root(&["read_only".into()], None, None).is_ok());
        let _ = std::fs::remove_dir_all(&home);
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
    fn serve_url_flag_wins_over_env() {
        let u = resolve_serve_url(
            Some("http://flag:1".to_string()),
            Some("http://env:2".to_string()),
        );
        assert_eq!(u, "http://flag:1");
    }

    #[test]
    fn serve_url_env_wins_over_default() {
        let u = resolve_serve_url(None, Some("http://remote:9".to_string()));
        assert_eq!(u, "http://remote:9");
    }

    #[test]
    fn serve_url_empty_env_is_ignored() {
        assert_eq!(
            resolve_serve_url(None, Some(String::new())),
            DEFAULT_SERVE_URL
        );
    }

    #[test]
    fn serve_url_default_when_neither() {
        assert_eq!(resolve_serve_url(None, None), "http://127.0.0.1:7801");
    }

    #[test]
    fn approvals_parse_without_url_leaves_it_unset() {
        let cli = Cli::try_parse_from(["tole", "approvals", "list"]).unwrap();
        match cli.command {
            Command::Approvals { url, .. } => assert!(url.is_none()),
            _ => panic!("expected Approvals"),
        }
        let cli =
            Cli::try_parse_from(["tole", "approvals", "list", "--url", "http://x:1"]).unwrap();
        match cli.command {
            Command::Approvals { url, .. } => assert_eq!(url.as_deref(), Some("http://x:1")),
            _ => panic!("expected Approvals"),
        }
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
        tool_roster::register_todo(&mut reg, &[], false).unwrap();
        assert!(reg.get("todo_read").is_some());
        assert!(reg.get("todo_write").is_some());
        let mut plan = ToolRegistry::new();
        tool_roster::register_todo(&mut plan, &[], true).unwrap();
        assert!(plan.get("todo_read").is_some());
        assert!(plan.get("todo_write").is_none());
    }

    /// Issue #294 regression: the server-level registry of the MCP HTTP
    /// face must be read-only under plan mode (absent, not just denied).
    #[cfg(all(feature = "mcp-http", feature = "shell-tools"))]
    #[test]
    fn mcp_server_registry_is_read_only_under_plan_mode() {
        use tole_core::tool::Risk;
        let normal = build_server_registry_for_mcp(None, false).unwrap();
        assert!(
            normal.specs().iter().any(|s| normal
                .get(s["function"]["name"].as_str().unwrap())
                .unwrap()
                .risk()
                != Risk::ReadOnly),
            "non-plan registry must expose write tools"
        );
        let plan = build_server_registry_for_mcp(None, true).unwrap();
        let specs = plan.specs();
        assert!(!specs.is_empty());
        for spec in &specs {
            let name = spec["function"]["name"].as_str().unwrap();
            assert_eq!(
                plan.get(name).unwrap().risk(),
                Risk::ReadOnly,
                "{name} must not survive plan mode"
            );
        }
    }

    /// cora 0.19 scan MAJOR: the MCP HTTP face's server-level file tools
    /// must be rooted at `--workspace`, not the process cwd.
    #[cfg(all(feature = "mcp-http", feature = "shell-tools"))]
    #[test]
    fn mcp_server_registry_file_tools_are_rooted_at_workspace() {
        let ws = std::env::temp_dir().join(format!("tole-mcp-ws-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("only-in-ws.txt"), "rooted").unwrap();
        let ws_arg = ws.to_str().unwrap().to_string();
        let reg = build_server_registry_for_mcp(Some(&ws_arg), false).unwrap();
        let out = reg
            .get("read_file")
            .unwrap()
            .execute(serde_json::json!({ "path": "only-in-ws.txt" }))
            .unwrap();
        assert_eq!(out["content"], serde_json::json!("rooted"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
#[path = "main_gate_tests.rs"]
mod gate_policy_tests;
