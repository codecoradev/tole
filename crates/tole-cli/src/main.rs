//! CLI host for tole-core: arg parsing and session wiring.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use tole_cli::session::assemble::{default_prompt_for, resolve_workspace_root, review_section};
use tole_cli::trust::expand_trust;

// Test-only scratch directory shared with the integration tests (#781).
#[cfg(feature = "shell-tools")]
mod acp;
#[cfg(test)]
#[path = "../tests/common/test_dir.rs"]
mod test_dir;
use tole_cli::approvals;
mod chat;
mod cli_actions;
mod flags;
mod gate;
mod host_config;
#[cfg(feature = "mcp-http")]
mod mcp_http;
#[cfg(feature = "mcp")]
mod mcp_specs;
#[cfg(feature = "shell-tools")]
mod memory;
mod mission;
mod owner_cmds;
mod registry;
mod report;
mod run;
#[cfg(feature = "shell-tools")]
mod serve;
mod sessions;
mod setup;
#[cfg(feature = "tui")]
mod tui;
mod upgrade;
use chat::chat_command;
use cli_actions::{ConfigAction, DelegationAction, DiscussAction};
#[cfg(feature = "shell-tools")]
use flags::refuse_unattended;
use flags::{
    check_server_face_flags, config_layer, from_config, pretool_from_config, refuse,
    resolve_serve_url, with_trust,
};
use gate::{config_flags, gate_policy, runs_cron_jobs, sandbox_flag_class, GatePolicy};
use host_config::HostConfig;
#[cfg(feature = "mcp")]
use mcp_specs::{auto_mcp_specs, helper_mcp_servers, merge_mcp_specs};
#[cfg(feature = "shell-tools")]
use memory::resolve_memory;
use owner_cmds::{delegation_command, discuss_command};
#[cfg(all(feature = "mcp-http", feature = "shell-tools"))]
use registry::build_server_registry_for_mcp;
#[cfg(feature = "shell-tools")]
use registry::install_owner_mcp;
#[cfg(all(feature = "mcp", feature = "shell-tools"))]
use registry::mcp_server_command;
use registry::{build_approver, build_registry};
use report::report_outcome;
use run::{resume_command, run_command};
use sessions::{
    all_sessions_command, branch_command, import_claude_command, latest_session_id, new_session_id,
    session_path, sessions_command, sessions_tree_command, status_command, valid_session_id,
};
#[cfg(feature = "tui")]
use tui::tui_command;

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

    /// Register an external MCP server: name=command [args...] (stdio) or
    /// name=https://host/path (Streamable HTTP; the bearer token comes from
    /// TOLE_MCP_<NAME>_TOKEN, never from the spec). Repeatable. Every MCP
    /// tool joins the registry as Risk::Write and goes through the normal
    /// approval gate. Requires the `mcp` feature.
    #[cfg(feature = "mcp")]
    #[arg(long, global = true)]
    mcp_server: Vec<String>,

    /// Review gate before `git commit` (#534), active while the cora review
    /// tools are attached: `block` (default) refuses a commit whose staged
    /// diff was not reviewed, `warn` lets it through with a note, `off`
    /// never gates. Same as TOLE_REVIEW_GATE; the flag wins. Needs cora
    /// >= 0.20.0 (`cora hook verify`).
    #[cfg(feature = "shell-tools")]
    #[arg(long, global = true, value_enum)]
    review_gate: Option<tole_cli::review_gate::Policy>,

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
    /// an approval denial and is resumable: `tole resume <id>` re-asks
    /// the model, `tole resume <id> "<prompt>"` settles the parked turn
    /// and runs the new prompt; either way the model sees the denial and
    /// replans); any other non-zero exit
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
    /// discovery, no trust check, no output. Owner at a terminal only
    /// (#855): refused under an agent session and for `serve`, `acp` and
    /// `mcp`.
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

    /// Reasoning effort sent as `reasoning_effort` on chat completions
    /// (#457): minimal | low | medium | high. Unset (default) sends
    /// nothing. Precedence: flag > TOLE_REASONING_EFFORT > config
    /// `reasoning_effort` > user config. A session's `fact/reasoning_effort`
    /// register overrides it on resume.
    #[arg(long, global = true, value_name = "LEVEL", value_parser = tole_core::openai::parse_reasoning_effort)]
    effort: Option<String>,

    /// Run as a profile (#584): a `[profiles.<name>]` table of the USER
    /// config with a persona file, allow list, skills, memory room and model
    /// tiers. Its values sit below the per-key flags (`--system`, `--allow`,
    /// `--skill`) and above the environment and the config files; its
    /// `default` tier's model beats TOLE_MODEL. Profiles are dormant:
    /// selecting one only reads its persona file.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,

    /// Run every stdio MCP server in this sandbox profile (#750): built in
    /// (`server`, `strict`) or defined in `[sandbox.profiles]` of the USER
    /// config. It only selects: it does not define a profile, loosen one or
    /// turn the sandbox on, and a session (agent, child, cron job) cannot
    /// pass it.
    #[arg(
        long,
        global = true,
        value_name = "PROFILE",
        conflicts_with = "no_sandbox"
    )]
    sandbox: Option<String>,

    /// Start stdio MCP servers unsandboxed for this run (#750). Owner at a
    /// terminal only: refused under an agent session, in `tole serve`/`acp`
    /// and in any run without a terminal while `[sandbox] mode` is
    /// `required` or `optional`. `tole doctor` and the startup log say it
    /// overrode the config.
    #[arg(long, global = true)]
    no_sandbox: bool,

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

        /// Attach an image to the first message: a file path inside the
        /// workspace jail, or an inline `data:image/…;base64,…` URL (png,
        /// jpeg, gif, webp; 10 MiB max). Needs a vision-capable model;
        /// repeatable (#212).
        #[arg(long = "image")]
        images: Vec<String>,
    },
    /// Run an autonomous mission: budgeted turn-chaining toward a goal
    /// (issue #199). Every chained turn is a normal durable turn —
    /// crash mid-mission: `tole resume` first, then `--resume`; Destructive
    /// tools stay un-auto-allowable. Termination: the model declares
    /// MISSION_COMPLETE (and `--verify` exits 0, when set), or a budget
    /// trips (`--max-steps` / `--max-minutes`), or the verify gate fails
    /// too many times. A durable summary lands on the session either way.
    Mission {
        /// The mission goal, verbatim.
        goal: String,

        /// Total provider steps across all chained turns (from the
        /// durable usage ledger). Default: 160 (config `[mission]
        /// max_steps`).
        #[arg(long)]
        max_steps: Option<u64>,

        /// Wall-clock cap in minutes. Default: 60 (config `[mission]
        /// max_minutes`).
        #[arg(long)]
        max_minutes: Option<u64>,

        /// Total token ceiling (prompt + completion, from the usage
        /// ledger). Default: 4,000,000 (config `[mission]
        /// max_tokens`).
        #[arg(long)]
        max_tokens: Option<u64>,

        /// Verification command run after each turn; exit 0 = goal
        /// achieved (overrides the model's completion marker). Failures
        /// return to the model with the output; 3 failures settle the
        /// mission as verify_failed. A command the owner's sandbox
        /// refuses ([sandbox.consumers] verify) settles it as
        /// verify_refused, without a retry.
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

        /// Attach an image to the first message/turn (#212); same
        /// semantics as `run --image`.
        #[arg(long = "image")]
        images: Vec<String>,
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
    /// Install or upgrade the cora binary the review flow needs (#535).
    /// Runs `cargo install cora-code --locked` after a confirmation; never
    /// silent. `tole doctor` reports whether it is needed.
    Setup {
        /// What to set up.
        #[arg(value_enum)]
        tool: setup::Tool,

        /// Only report whether an install or upgrade is needed.
        #[arg(long)]
        check: bool,

        /// Skip the confirmation prompt (CI/automation).
        #[arg(long)]
        yes: bool,
    },
    /// Resume an interrupted session. With an optional
    /// PROMPT, appends it as a new user message and runs one full turn
    /// (issue #55): headless flows can continue a mission without a
    /// separate `run` session (a turn parked at a denied approval or
    /// pre-tool hook is settled first; the model sees the denial).
    /// Without PROMPT, behaves as before: approvals-only recovery of a
    /// mid-flight turn.
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
    /// Skills: the owner's approvals of skill content (#591: `approve`,
    /// `revoke`, `list`; required by default, `TOLE_SKILL_APPROVAL=off` opts out) and,
    /// in shell-tools builds, the uteke skill index (#590: `index`,
    /// `retire`, `search`; used with `TOLE_SKILL_INDEX=uteke`).
    Skills {
        #[command(subcommand)]
        action: tole_cli::skill_approve::SkillsAction,
    },
    /// Keep the skill library small (#593): what the curator manages, the
    /// sweep that archives skills unused past the thresholds (`run
    /// --dry-run` to look first), restore, pin, lock, adopt, pause. It
    /// never edits a skill's text.
    Curator {
        #[command(subcommand)]
        action: tole_cli::curator_cmd::CuratorAction,
    },
    /// The plugin lock of your MCP servers (#599): list their state, diff a
    /// changed server, approve or revoke one. Never starts a session.
    #[cfg(feature = "mcp")]
    Plugin {
        #[command(subcommand)]
        action: tole_cli::plugin_cmd::PluginAction,
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
        /// Show forks (#210) nested under the session they were cut
        /// from instead of a flat list.
        #[arg(long, conflicts_with = "all")]
        tree: bool,
    },
    /// Fork a session (#210): a NEW session holding the source's history
    /// up to a settled entry, to try another approach without touching
    /// the original. Continue it with `tole resume <new-id> "<prompt>"`.
    Branch {
        /// Session id to fork.
        id: String,
        /// Entry id to cut at (a final assistant answer; see `status`).
        /// Default: the last settled entry.
        #[arg(long)]
        at: Option<String>,
    },

    /// Import a Claude Code session transcript (#381) as a NEW tole
    /// session (`cc-<claude-session-id>`); continue it with
    /// `tole resume <new-id> "<prompt>"`. Accepts the `~/.claude/projects`
    /// JSONL or `claude -p --output-format stream-json` output. Existing
    /// sessions are untouched; importing the same session twice is refused.
    ImportClaude {
        /// Path to the Claude Code transcript (JSONL).
        path: PathBuf,
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
        /// Session id to inspect (default: the most recently modified).
        id: Option<String>,
    },
    /// Print this workspace's delegation tree (#585), read-only: goals, or
    /// one goal's nodes with profile, mode, status, budget and use, allow
    /// lists (and dropped patterns), leases, report summary and reason.
    /// Output is capped and redacted; nothing is written.
    /// With `--discussions` or `--discussion <ID>`, the multi-round
    /// discussions (#588) instead: goal, rounds, digests, gate decisions
    /// and tokens used, never a raw contribution.
    Audit {
        /// The goal to show (default: list the goals).
        goal: Option<String>,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// List the multi-round discussions.
        #[arg(long, conflicts_with_all = ["goal", "discussion"])]
        discussions: bool,
        /// Show one multi-round discussion.
        #[arg(long, value_name = "ID", conflicts_with = "goal")]
        discussion: Option<String>,
    },
    /// Multi-round discussion (#588), owner-only entry point: start one
    /// from a shell with an explicit question, acceptance criteria,
    /// participants, token budget and max rounds, resume it after a crash,
    /// list or show them. The contract is what you type; participants are
    /// read-only inline voices (profiles from your user config) and the
    /// budget is a hard cap.
    Discuss {
        #[command(subcommand)]
        action: DiscussAction,
    },
    /// The owner's side of delegation (#714): read and answer the approval
    /// requests of delegated children. Owner-only: refuses under an agent
    /// session.
    Delegation {
        #[command(subcommand)]
        action: DelegationAction,
    },
    /// Show which model tier `[router]` would pick for a task (#586): a dry
    /// run, no provider is called. `--profile` selects the profile's tiers.
    Route {
        /// The task text (only its size and the rules' keywords are read).
        prompt: String,
        /// chat, run, mission, cron or delegate.
        #[arg(long, default_value = "run")]
        kind: String,
        /// The cron job's name.
        #[arg(long)]
        job: Option<String>,
        /// The highest tool risk the run may use: read_only, write or
        /// destructive.
        #[arg(long, default_value = "write")]
        risk: String,
        /// Tokens the run may still spend.
        #[arg(long)]
        budget_left: Option<u64>,
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

        /// Listen on this Unix domain socket instead of TCP (#1043; macOS and
        /// Linux; a Windows named pipe is not built yet). The directory is
        /// created 0700; serve refuses a directory that is wider, owned by
        /// someone else or a link, and a socket path that is a link. REST
        /// only. A bearer token is still required: pass it with --token-fd
        /// or --token-file.
        #[arg(long, conflicts_with_all = ["bind", "port"])]
        socket: Option<std::path::PathBuf>,

        /// Transport: `rest` (default) or `mcp` (Streamable HTTP).
        #[arg(long, default_value = "rest")]
        transport: String,

        /// Bind address (default: 127.0.0.1 — local only).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Bearer token required on every request (or TOLE_SERVE_TOKEN
        /// env, or --token-file). Refuses to start without one. Prefer the
        /// env or the file: a command-line value is visible to every local
        /// process (`ps`, /proc/<pid>/cmdline).
        #[arg(long, conflicts_with = "token_file")]
        token: Option<String>,

        /// Read the bearer token from this file (a regular file, at most
        /// 4 KiB, one trailing newline dropped).
        #[arg(long)]
        token_file: Option<std::path::PathBuf>,

        /// Read the bearer token from this inherited file descriptor (#1043),
        /// to the end, then close it: the per-launch credential of a desktop
        /// that starts serve, in neither the arguments nor the environment.
        #[arg(long, conflicts_with_all = ["token", "token_file"])]
        token_fd: Option<i32>,

        /// Authentication (#742): `token` (default; the bearer token above)
        /// or `basic` (HTTP Basic, one user, a password hash; the bearer
        /// token, when one is set, keeps working next to it). TLS is not
        /// served here: terminate it at a reverse proxy.
        #[arg(long, default_value = "token", value_parser = ["token", "basic"])]
        auth: String,

        /// `--auth basic`: the user name (or TOLE_SERVE_BASIC_USER).
        #[arg(long)]
        basic_user: Option<String>,

        /// `--auth basic`: read the password hash made by `tole serve-hash`
        /// from this file (or TOLE_SERVE_BASIC_HASH). There is no hash flag:
        /// a command-line value is visible to every local process.
        #[arg(long)]
        basic_hash_file: Option<std::path::PathBuf>,

        /// `--auth basic` on a non-loopback `--bind`: accept that the
        /// credentials cross the network in clear text unless TLS is
        /// terminated in front of this server. Refused without it.
        #[arg(long)]
        allow_insecure_basic: bool,

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

        /// Host the cron ticker in-process (#579): one scheduler pass per
        /// minute over `$TOLE_CRON_DIR`. Agent runs park non-allowlisted
        /// Writes in this server's `/approvals` queue (never auto-approved).
        #[arg(long)]
        cron: bool,

        /// Seconds a SIGTERM/SIGINT shutdown waits for running turns (they are
        /// detached and stay resumable) before exiting anyway with a non-zero
        /// code (#1014). Falls back to TOLE_SERVE_SHUTDOWN_GRACE; default 20;
        /// 0 does not wait. Applies to both transports.
        #[arg(long, value_name = "SECS")]
        shutdown_grace: Option<u64>,
    },
    /// Print a password hash for `tole serve --auth basic` (#742): reads the
    /// password from stdin (a terminal is asked without echo, twice).
    #[cfg(feature = "shell-tools")]
    ServeHash {
        /// PBKDF2 iterations (at least 10000).
        #[arg(long, default_value_t = tole_cli::auth::DEFAULT_ITERATIONS)]
        iterations: u32,
    },
    /// Interactive terminal UI (what bare `tole` opens on a TTY).
    #[cfg(feature = "tui")]
    #[command(hide = true)]
    Tui {
        /// Resume an existing session by id instead of starting new.
        #[arg(long)]
        resume: Option<String>,
        /// Same semantics as `run --allow`.
        #[arg(long = "allow")]
        allow_patterns: Vec<String>,
        /// Same semantics as `run --yes`.
        #[arg(long)]
        yes: bool,
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

        /// Attach an image to the first message (#212); same semantics as
        /// `run --image`. Later messages: type `/image <path>` in the chat.
        #[arg(long = "image")]
        images: Vec<String>,
    },
    /// Scheduled jobs (#579): `create`, `edit`, `list`, `status`, `pause`,
    /// `resume`, `remove`, `run`, `cancel`, `runs`, and `tick` (one
    /// scheduler pass, for cron(8) or a systemd timer). Script jobs run argv without a shell and spend zero
    /// tokens. Jobs live in `$TOLE_CRON_DIR` (default `<codecora home>/tole/cron`).
    #[cfg(feature = "shell-tools")]
    Cron {
        #[command(subcommand)]
        action: tole_cli::cron::CronAction,
    },
    /// Webhook routes of `tole serve` (#582): `add`, `list`, `remove` and
    /// `test` (a local dry run; `--fire` to a loopback serve). Routes live
    /// in the user config only; keys are never given or shown.
    #[cfg(feature = "shell-tools")]
    Hook {
        #[command(subcommand)]
        action: tole_cli::hook_cli::HookAction,
    },
    /// Outbound webhooks (#911): `test NAME` sends one signed `webhook_test`
    /// event to a destination of the user config and prints the status the
    /// receiver answered. Destinations are declared in the user config only.
    Webhook {
        #[command(subcommand)]
        action: tole_cli::webhook_cmd::WebhookAction,
    },
    /// Per-server sandbox profiles (#810): `list` the profiles and bindings,
    /// `show` the profile one server runs in, `set` binds a server to a
    /// profile. Writes the user config only; never starts a session.
    Sandbox {
        #[command(subcommand)]
        action: tole_cli::sandbox_cmd::SandboxAction,
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

/// Bare `tole` (no arguments at all) on an interactive terminal opens the
/// TUI (#459/#460) through the hidden `tui` subcommand, so it shares the
/// config gate and settings resolution with every other command. Anything
/// else, including a non-TTY, falls through to clap unchanged.
#[cfg(feature = "tui")]
fn bare_tui_args() -> Option<Vec<std::ffi::OsString>> {
    use std::io::IsTerminal;
    (std::env::args_os().count() == 1
        && std::io::stdout().is_terminal()
        && std::io::stdin().is_terminal())
    .then(|| vec!["tole".into(), "tui".into()])
}

fn main() {
    // First, before any thread or child exists (#675): a same-user child
    // must not read this process's environment through /proc/<ppid>/environ
    // or its memory. Every tole process (serve, mission, cron, child
    // agents) starts here.
    if let Err(e) = tole_core::process_guard::deny_inspection() {
        eprintln!("tole: warning: could not deny same-user inspection of this process: {e}");
    }
    #[cfg(feature = "tui")]
    let cli = match bare_tui_args() {
        Some(args) => Cli::parse_from(args),
        None => Cli::parse(),
    };
    #[cfg(not(feature = "tui"))]
    let cli = Cli::parse();
    // `--no-config` drops the owner's user config: owner at a terminal only
    // (#855). Before anything reads the config or starts a command.
    // stdin and stderr both terminals: the `--no-sandbox` rule (#867).
    let has_terminal = {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    };
    let no_config_ctx = tole_cli::no_config_guard::Context {
        under_session: tole_core::child_env::under_session(&|k| std::env::var(k).ok()),
        unattended_command: sandbox_flag_class(&cli.command).0,
        tty: has_terminal,
        // The user config is read only to decide, and only when the flag is
        // given and there is no terminal; unreadable keeps the strict rule.
        user_protects: (cli.no_config && !has_terminal)
            .then(|| {
                tole_cli::no_config_guard::user_config_protects(
                    tole_cli::config::user_config_path().as_deref(),
                )
            })
            .flatten(),
    };
    if let Err(e) = tole_cli::no_config_guard::check(cli.no_config, &no_config_ctx) {
        eprintln!("tole: {e}");
        std::process::exit(1);
    }
    // `--review-gate` travels as the env var the roster reads, so child
    // agents (which inherit the environment) are gated the same way.
    #[cfg(feature = "shell-tools")]
    if let Some(p) = cli.review_gate {
        std::env::set_var(tole_cli::review_gate::ENV, p.as_str());
    }
    if let Err(e) = dispatch(cli) {
        eprintln!("tole: {e:#}");
        std::process::exit(1);
    }
}

/// An error of `tole plugin`, redacted and terminal-safe.
#[cfg(feature = "mcp")]
fn ctx_clean(e: &str) -> String {
    let set = tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
    tole_core::plugin_lock::terminal_safe(&set.redact(e))
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
                // An unset verify or turnend is not "off" (#748): say what the
                // default is, since the listing above shows only written keys.
                for k in ["verify", "turnend"] {
                    if !out.contains(&format!("sandbox.consumers.{k} = ")) {
                        println!(
                            "sandbox.consumers.{k}: not set, so optional (default): sandboxed \
                             where bubblewrap works, unsandboxed with a warning only where the \
                             host has no sandbox; set it to \"off\" in the user config to opt out"
                        );
                    }
                }
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
    // The commands that execute cron jobs read it too even though they are on
    // the skip list (#863): their agent runs and scripts need the owner's
    // sandbox, consumers, secret release and sensitive files like `tole run`.
    // A user config that does not parse stops them here (fail closed).
    let user_config = if (policy == GatePolicy::Skip && !runs_cron_jobs(&cli.command))
        || cli.no_config
    {
        None
    } else {
        // `tole cron tick` is what an OS cron launches: a `using user config`
        // line on every successful run would be one mail a minute under
        // MAILTO. Without a terminal on stderr the line is dropped for the
        // two job-running commands; an error is never dropped.
        let quiet = runs_cron_jobs(&cli.command) && {
            use std::io::IsTerminal;
            !std::io::stderr().is_terminal()
        };
        let path = tole_cli::config::user_config_path();
        let loaded = config_apply::load_user_config(
            path.as_deref(),
            &mut config_apply::StartupIo::new(false).quiet_inform(quiet),
        )?;
        // A missing file is announced only where it may be a wrong
        // directory rather than a choice (#864); an owner on the default
        // path, and a cron tick there, stay silent.
        if loaded.is_none() {
            let ctx = config_apply::AbsentCtx {
                root_overridden: std::env::var("CODECORA_HOME").is_ok_and(|v| !v.trim().is_empty()),
                under_session: tole_core::child_env::under_session(&|k| std::env::var(k).ok()),
                unattended_command: sandbox_flag_class(&cli.command).0,
            };
            if let Some(note) = config_apply::absent_user_config_notice(path.as_deref(), &ctx) {
                eprintln!("{note}");
            }
        }
        loaded
    };
    // The raw USER config, for the narrow-only guard below (the layered one
    // cannot tell the user's lists from a project's).
    let user_cfg_for_guard = user_config.as_ref().map(|l| l.config.clone());
    let cfg = match (project_config, user_config) {
        (None, None) => None,
        (p, u) => {
            let shown = p
                .as_ref()
                .map(|l| tole_cli::config_trust::sanitize(&l.shown.display().to_string()));
            let (p, u) = (
                p.map(|l| l.config).unwrap_or_default(),
                u.map(|l| l.config).unwrap_or_default(),
            );
            if let Some(shown) = shown {
                for w in tole_cli::config::secrets_widening_warnings(&p, &u, &shown) {
                    eprintln!("warning: {w}");
                }
            }
            Some(tole_cli::config::layered(&p, &u))
        }
    };
    // Scoped secret release (#598): which tools may hand which secret to
    // the processes they start. Every launcher reads it from here.
    tole_core::secret_release::set_release(cfg.as_ref().map(|c| c.release()).unwrap_or_default());
    // Sandbox of stdio MCP servers (#601): off unless the user config sets
    // `[sandbox] mode`; the config was validated at load.
    let sandbox_cfg = cfg.as_ref().and_then(|c| c.sandbox.as_ref());
    let sandbox_policy = sandbox_cfg.and_then(|s| s.policy().ok());
    // `--sandbox <profile>` / `--no-sandbox` (#750): owner at a terminal
    // only; a refusal stops the run before any server starts.
    {
        use std::io::IsTerminal;
        let flags = tole_cli::sandbox_flags::Flags {
            sandbox: cli.sandbox.clone(),
            no_sandbox: cli.no_sandbox,
        };
        let (unattended_command, no_server) = sandbox_flag_class(&cli.command);
        let ctx = tole_cli::sandbox_flags::Context {
            under_session: tole_core::child_env::under_session(&|k| std::env::var(k).ok()),
            unattended_command,
            meaningless: no_server,
            tty: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        };
        match tole_cli::sandbox_flags::apply(&flags, sandbox_policy.as_ref(), sandbox_cfg, &ctx) {
            Err(e) => return Err(anyhow!("{e}")),
            Ok(None) => {}
            Ok(Some(o)) => {
                let _ = tole_core::sandbox::flags::set(o.over);
                if let Some(n) = tole_core::sandbox::flags::note() {
                    eprintln!("tole: {n}");
                }
            }
        }
    }
    if let Err(e) = tole_core::sandbox::set_policy(sandbox_policy) {
        eprintln!("tole: warning: {e}");
    }
    if let Err(e) =
        tole_core::sandbox::set_lazy_start(sandbox_cfg.and_then(|s| s.lazy_start).unwrap_or(false))
    {
        eprintln!("tole: warning: {e}");
    }
    // Sandbox of the commands a model makes the host run (#748): off unless
    // `[sandbox.consumers]` says so; validated at load, user layer only.
    if let Err(e) = tole_core::sandbox::command::set_consumers(
        cfg.as_ref()
            .and_then(|c| c.sandbox.as_ref())
            .and_then(|s| s.consumers().ok())
            .unwrap_or_else(tole_core::sandbox::command::Consumers::host_default),
    ) {
        eprintln!("tole: warning: {e}");
    }
    // Probe the backend NOW when a consumer is only optional by default (#748):
    // the default degrades to "no sandbox" on a negative answer, so the answer
    // must be had before the model runs a command (serve, acp, mcp and the stop
    // gates have no start-up check of their own), and a definite one is cached.
    {
        use tole_core::sandbox::command::{consumers, Consumer};
        let c = consumers();
        if c.implicit(Consumer::Verify) || c.implicit(Consumer::TurnEnd) {
            let _ = tole_core::sandbox::detect();
        }
    }
    // The default flip's fallback (verify or turnend, optional by default, no
    // sandbox backend on this host) goes to the ledger as well as stderr.
    tole_core::sandbox::command::set_fallback_sink(
        tole_cli::plugin::gate::record_consumer_fallback,
    );
    // Extra sensitive file names (#683): user globs plus the project's.
    tole_core::sensitive::set_extra(
        cfg.as_ref()
            .and_then(|c| c.sensitive_files.clone())
            .unwrap_or_default(),
    );
    // Secret-shape detector allowlist (#597): the user layer's regexes,
    // validated at config load.
    if let Some(a) = cfg.as_ref().and_then(|c| c.redaction_allowlist.as_ref()) {
        if let Err(e) = tole_core::secrets::shape::set_allowlist(a) {
            eprintln!("tole: redaction_allowlist: {e}");
            std::process::exit(2);
        }
    }
    // The selected profile (#584): resolved once, from the layered config
    // (the user layer is its only source). Reads its persona file and
    // nothing else; an unknown name is a startup error. `tole cron`
    // records --profile in a job instead and checks it against the user
    // config itself (#579).
    let profile = match &cli.profile {
        None => None,
        #[cfg(feature = "shell-tools")]
        Some(_) if matches!(cli.command, Command::Cron { .. }) => None,
        // `tole sandbox set --profile` names a sandbox profile (#810).
        Some(_) if matches!(cli.command, Command::Sandbox { .. }) => None,
        Some(name) => {
            let base = tole_cli::config::user_config_path()
                .and_then(|p| p.parent().map(Path::to_path_buf));
            let home = std::env::var_os("HOME").map(PathBuf::from);
            Some(
                tole_cli::profile::Profile::resolve(
                    cfg.as_ref().unwrap_or(&Default::default()),
                    name,
                    base.as_deref(),
                    home.as_deref(),
                )
                .map_err(anyhow::Error::msg)?,
            )
        }
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
        config_apply::install_fill(Settings::fill_with_profile(cfg, profile.as_ref()));
    }
    // Under an agent session a flag or the environment may only NARROW the
    // owner's policy lists (#865); the same marker as the other #684 guards,
    // not a boundary (the command sandbox is).
    {
        let env = &config_apply::real_env;
        let tty = {
            use std::io::IsTerminal;
            std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
        };
        let ctx =
            tole_cli::narrow_guard::Context::from_env(env, sandbox_flag_class(&cli.command).0, tty);
        let inputs = tole_cli::narrow_guard::Inputs::from_flags(&config_flags(&cli), env);
        tole_cli::narrow_guard::check(user_cfg_for_guard.as_ref(), &inputs, &ctx)
            .map_err(|e| anyhow!("{e}"))?;
    }
    let settings = Settings::resolve(
        &config_apply::Flags {
            profile: profile.clone(),
            ..config_flags(&cli)
        },
        cfg.as_ref().unwrap_or(&Default::default()),
        &config_apply::real_env,
    );
    if let Some(msg) = settings.effort_error() {
        anyhow::bail!(msg);
    }
    if let Some(msg) = settings.discussion_timeout_error() {
        anyhow::bail!(msg);
    }
    if let Some(msg) = settings.chat_timeout_error() {
        anyhow::bail!(msg);
    }
    config_apply::install_discussion_timeout(&settings.request_timeout);
    config_apply::install_chat_timeout(&settings.chat_timeout);
    config_apply::install_effort(settings.reasoning_effort.as_ref().map(|e| e.value.clone()));
    // A config `review_gate` reaches the roster the same way the flag does:
    // as the env var (child agents inherit it). Flag and env already won
    // inside `Settings::resolve`, so only a config value is exported here.
    #[cfg(feature = "shell-tools")]
    if let Some(g) = &settings.review_gate {
        if matches!(
            g.source,
            config_apply::Source::Config | config_apply::Source::UserConfig
        ) {
            std::env::set_var(tole_cli::review_gate::ENV, &g.value);
        }
    }
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
    // The EFFECTIVE sessions directory is told to the command sandbox, which
    // binds it read-only when it is inside the workspace and refuses a layout it
    // cannot protect (#866). Not only a moved one: the default `.tole/sessions`
    // is relative to the working directory, and with `--workspace` naming a
    // parent of it (cwd `/repo/sub`, workspace `/repo`) it is NOT under the
    // workspace's own `.tole`, which `job_start` (whose sandbox workspace is
    // `--workspace`) would leave writable. serve, acp and mcp keep sessions per
    // session workspace unless `-s` moves them, so only an explicit one counts
    // there. A directory already under `.tole` adds nothing to the plan.
    {
        let per_session_face = sandbox_flag_class(&cli.command).0;
        let effective = match (&sessions_dir_override, per_session_face) {
            (Some(dir), _) => Some(dir.clone()),
            (None, false) => Some(sessions_dir.clone()),
            (None, true) => None,
        };
        if let Some(dir) = effective {
            let told = if dir.is_absolute() {
                Ok(dir)
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(&dir))
                    .map_err(|e| {
                        format!(
                            "cannot resolve the sessions directory {} against the working \
                             directory ({e})",
                            dir.display()
                        )
                    })
            }
            .and_then(tole_core::sandbox::command::set_sessions_dir);
            if let Err(e) = told {
                // Never fail open: where a command sandbox is required (or the
                // consumer is always unattended: verify, stop gates, cron) the run
                // does not start; an `optional` run_command/job_start with a
                // human attached gets a loud warning instead.
                use tole_core::sandbox::command::Consumer;
                use tole_core::sandbox::Mode;
                let c = tole_core::sandbox::command::consumers();
                // A `verify` or `turnend` that is `optional` only by default
                // (#748) does not stop the start, it warns: a default must
                // not refuse every run over a sessions path nobody chose.
                let (v_dflt, t_dflt) =
                    (c.implicit(Consumer::Verify), c.implicit(Consumer::TurnEnd));
                let needs_it = c.run_command == Mode::Required
                    || c.job_start == Mode::Required
                    || (c.verify != Mode::Off && !v_dflt)
                    || (c.turnend != Mode::Off && !t_dflt)
                    || c.cron != Mode::Off;
                if needs_it {
                    return Err(anyhow!(
                        "{e}; refusing to start with a command sandbox that cannot protect the \
                         sessions directory (set the sessions directory with -s to an absolute \
                         path, or turn the consumer off)"
                    ));
                }
                // Nothing sandboxed, nothing to say.
                if c.run_command != Mode::Off || c.job_start != Mode::Off || v_dflt || t_dflt {
                    eprintln!(
                        "tole: warning: {e}: sandboxed commands are not kept from writing the \
                         sessions directory"
                    );
                }
            }
        }
    }
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
        profile_name: profile.as_ref().map(|p| p.name.clone()),
        max_tokens: settings.max_tokens.as_ref().map(|t| t.value),
        workspace: settings.workspace.as_ref().map(|w| w.value.clone()),
        skills: config_apply::values(&settings.skill),
        no_skills: settings.no_skills.value,
        #[cfg(feature = "mcp")]
        mcp_server: mcp_specs,
        plan_mode: settings.plan_mode.value,
        agents_worktree: cli.agents_worktree,
        delegation: tole_cli::delegate::Setup {
            cfg: cfg.clone().unwrap_or_default(),
            base: tole_cli::config::user_config_path()
                .and_then(|p| p.parent().map(Path::to_path_buf)),
            home: std::env::var_os("HOME").map(PathBuf::from),
            data_dir: tole_core::paths::tole_data_dir(),
            probe: None,
            thresholds: None,
        },

        on_pretool: config_apply::values(&settings.on_pretool),
        on_posttool: config_apply::values(&settings.on_posttool),
        on_turnend: config_apply::values(&settings.on_turnend),
        #[cfg(feature = "shell-tools")]
        memory: resolve_memory(
            settings.memory.as_ref().map(|m| m.value.as_str()),
            settings.memory_room.as_ref().map(|r| r.value.as_str()),
        )?,
        #[cfg(not(feature = "shell-tools"))]
        memory: (),
        compaction: tole_cli::compaction_host::CompactionSettings {
            enabled: !settings.no_compaction.value,
            at: settings.compact_at.value,
            window: settings.context_window.as_ref().map(|w| w.value),
        },
    };
    // #212: where attached images may be read from; best effort — a bad
    // --workspace is reported by the commands that need it.
    tole_cli::turn_runner::set_image_roots(resolve_workspace_root(host.workspace.as_ref()).ok());
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

    // The plugin lock (#599): before an interactive terminal session starts
    // (never a mission, a child agent or a non-TTY), every MCP server that
    // is unapproved or changed is shown in full and the owner is asked.
    #[cfg(feature = "mcp")]
    {
        use std::io::IsTerminal;
        let interactive = match &cli.command {
            Command::Run { prompt_file, .. } => prompt_file.as_deref() != Some("-"),
            Command::Chat { .. } | Command::Resume { .. } => true,
            #[cfg(feature = "tui")]
            Command::Tui { .. } => true,
            _ => false,
        };
        if interactive
            && std::io::stdin().is_terminal()
            && std::io::stderr().is_terminal()
            && tole_core::child_env::depth(&|k| std::env::var(k).ok()) == 0
        {
            let specs: Vec<_> = host
                .mcp_server
                .iter()
                .filter_map(|s| tole_core::mcp::McpServerConfig::parse(s).ok())
                .collect();
            let root = resolve_workspace_root(host.workspace.as_ref())?;
            let mut gate = tole_cli::plugin_gate::Gate::load(
                tole_core::plugin_lock::Lockfile::user_path(),
                &root,
            );
            tole_cli::plugin_gate::preflight(&specs, &mut gate, &mut |text| {
                use std::io::Write;
                eprint!("{text}");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).is_ok()
                    && matches!(line.trim(), "y" | "Y" | "yes")
            });
        }
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
            images,
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
                &images,
            )
        }
        Command::Upgrade { check, yes } => {
            let code = upgrade::run(check, yes)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Setup { tool, check, yes } => {
            let code = setup::run(tool, check, yes)?;
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
                return Err(refuse_unattended(
                    "--on-pretool/--on-posttool are not supported by `tole mcp` \
                     (server mode pre-authorizes Write tools with --allow instead)",
                    pretool_from_config(&settings),
                ));
            }
            if !host.on_turnend.is_empty() {
                return Err(refuse_unattended(
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
        #[cfg(feature = "shell-tools")]
        Command::Cron { action } => tole_cli::cron::cli(
            action,
            tole_cli::cron::JobGlobals {
                skills: cli.skill.iter().map(|p| p.display().to_string()).collect(),
                profile: cli.profile.clone(),
            },
        ),
        #[cfg(feature = "shell-tools")]
        Command::Hook { action } => tole_cli::hook_cli::cli(action),
        Command::Webhook { action } => tole_cli::webhook_cmd::cli(action),
        #[cfg(feature = "shell-tools")]
        Command::ServeHash { iterations } => crate::serve::hash_command(iterations),
        Command::Sandbox { action } => tole_cli::sandbox_cmd::cli(action, cli.profile.as_deref()),
        Command::Skills { action } => tole_cli::skill_approve::cli(action),
        Command::Curator { action } => tole_cli::curator_cmd::cli(action),
        #[cfg(feature = "mcp")]
        Command::Plugin { action } => {
            use std::io::IsTerminal;
            let specs = host
                .mcp_server
                .iter()
                .filter_map(|s| tole_core::mcp::McpServerConfig::parse(s).ok())
                .collect();
            let mut ask = |q: &str| {
                use std::io::Write;
                eprint!("{q}");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).is_ok()
                    && matches!(line.trim(), "y" | "Y" | "yes")
            };
            // Typed confirmation (`add`): the line the owner types.
            let mut typed = |q: &str| {
                use std::io::Write;
                eprint!("{q}");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                line
            };
            let mut ctx = tole_cli::plugin_cmd::Ctx {
                typed: &mut typed,
                probe_one: &tole_core::mcp::probe_manifest,
                specs,
                user: tole_core::plugin_lock::Lockfile::user_path(),
                workspace: resolve_workspace_root(host.workspace.as_ref())?,
                list: &tole_core::mcp::list_manifest,
                interactive: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
                ask: &mut ask,
                secrets: tole_cli::provider_chain::process_secrets(&config_apply::real_env),
                first_party: tole_cli::plugin_gate::first_party_names(),
                sandbox: tole_core::sandbox::policy(),
                backend: tole_core::sandbox::detect(),
                mcp_grants: {
                    let release = tole_core::secret_release::release();
                    release
                        .held_grants()
                        .into_iter()
                        .filter(|n| release.allows(n, "mcp"))
                        .collect()
                },
                probe: !tole_core::child_env::under_session(&config_apply::real_env),
            };
            match tole_cli::plugin_cmd::run(action, &mut ctx) {
                Ok(out) => {
                    print!("{out}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("tole plugin: {}", ctx_clean(&e));
                    std::process::exit(1);
                }
            }
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
            images,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            let images = tole_cli::turn_runner::resolve_images(&images)?;
            // An independent discussion participant (#704) is launched as
            // `mission --plan-mode` by its host: read-only is the point of it.
            // Only the host's participant marker lifts the refusal; plan mode
            // there removes tools, it never adds one.
            let participant = tole_core::child_env::discussion_participant(&config_apply::real_env);
            if host.plan_mode && !participant {
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
            // A delegated child parks at an approval instead of denying at
            // EOF, only when `[delegation] approvals` is on (#714).
            let cancel = tole_core::cancel::CancelToken::new();
            let parked = tole_cli::delegated_approver::for_child(
                &config_apply::real_env,
                host.delegation.cfg.delegation.as_ref(),
                tole_cli::delegated_approver::Params {
                    patterns: allow_patterns.clone(),
                    auto_write: yes,
                    root: resolve_workspace_root(host.workspace.as_ref())?.canonicalize()?,
                    data_dir: tole_core::paths::tole_data_dir(),
                    secrets: std::sync::Arc::new(tole_core::secrets::SecretSet::from_env()),
                    cancel: cancel.clone(),
                    timing: Default::default(),
                },
            );
            let registry = match parked {
                Some(a) => build_registry(a, &host, &allow_patterns)?,
                None => {
                    build_registry(build_approver(&allow_patterns, yes), &host, &allow_patterns)?
                }
            };
            // Model routing (#586): a delegated child (TOLE_AGENT_DEPTH >= 1)
            // routes as `delegate`; a broken router fails before the
            // mission's session exists.
            tole_cli::routing::Routing::from_config(&host.delegation.cfg)
                .map_err(anyhow::Error::msg)?;
            let route_kind = tole_cli::routing::mission_kind(&config_apply::real_env);
            let route_goal = goal.clone();
            let route = |s: &mut dyn tole_core::storage::Storage,
                         r: &tole_core::tool::ToolRegistry| {
                host.route(s, route_kind, &route_goal, r, false, Some(tier.max_tokens))
            };
            mission::run_mission(
                mission::MissionConfig {
                    cancel,
                    goal,
                    max_steps: tier.max_steps,
                    max_minutes: tier.max_minutes,
                    max_tokens: tier.max_tokens,
                    // flag > config; timeout flag > config > 300.
                    verify: settings.verify.as_ref().map(|v| v.value.clone()),
                    verify_timeout_secs: settings.verify_timeout.value,
                    resume_id: resume,
                    fresh_id: None,
                    compaction: host.compaction,
                    images,
                    // A profile's persona replaces the default base prompt.
                    persona: profile.as_ref().and_then(|p| p.system_prompt.clone()),
                    // The profile's advisor (#587); a child agent gets one
                    // only when its own profile declares it.
                    advisor: profile.as_ref().and_then(|p| {
                        p.advisor
                            .clone()
                            .map(|m| (p.name.clone(), m, p.advisor_max_uses))
                    }),
                },
                registry,
                &sessions_dir,
                &route,
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
                return Err(refuse_unattended(
                    "--on-pretool/--on-posttool are not supported by `tole acp` \
                     (approvals happen via session/request_permission in the client)",
                    pretool_from_config(&settings),
                ));
            }
            check_server_face_flags("acp", &host, &settings, false)?;
            install_owner_mcp("acp", &settings)?;
            tole_cli::session_host::set_server_skills(
                tole_cli::session::assemble::ServerSkills::new(host.skills.clone(), host.no_skills)
                    .map_err(anyhow::Error::msg)?,
            );
            if host.plan_mode {
                eprintln!("tole acp: --plan-mode is active — serving read-only tools only");
            }
            let memory = match memory.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(m) => resolve_memory(
                    Some(m),
                    settings.memory_room.as_ref().map(|r| r.value.as_str()),
                )?,
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
            socket,
            bind,
            transport,
            token,
            token_file,
            token_fd,
            auth,
            basic_user,
            basic_hash_file,
            allow_insecure_basic,
            allow_patterns: _,
            workspace,
            memory,
            cron,
            shutdown_grace,
        } => {
            // Read first so a bad value stops the start-up, not the shutdown.
            let shutdown_grace = tole_cli::serve_support::shutdown::grace(
                shutdown_grace,
                std::env::var(tole_cli::serve_support::shutdown::GRACE_ENV).ok(),
            )
            .map_err(anyhow::Error::msg)?;
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            // Loud bails, same rule as `tole mcp`/`tole acp`: these host
            // flags have no server-face wiring, and a silent no-op is
            // worse than a startup error (scan-3 #9).
            if host.on_pretool_non_empty() || host.on_posttool_non_empty() {
                return Err(refuse_unattended(
                    "--on-pretool/--on-posttool are not supported by `tole serve` \
                     (a server has no local human; pre-authorize Write tools with --allow)",
                    pretool_from_config(&settings),
                ));
            }
            check_server_face_flags("serve", &host, &settings, false)?;
            install_owner_mcp("serve", &settings)?;
            tole_cli::session_host::set_server_skills(
                tole_cli::session::assemble::ServerSkills::new(host.skills.clone(), host.no_skills)
                    .map_err(anyhow::Error::msg)?,
            );
            let memory = match memory.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(m) => resolve_memory(
                    Some(m),
                    settings.memory_room.as_ref().map(|r| r.value.as_str()),
                )?,
                None => host.memory.clone(),
            };
            let workspace = workspace.or_else(|| host.workspace.clone());
            // Both transports refuse to start without credentials (#96/#137).
            if token.is_some() {
                eprintln!(
                    "tole serve: warning: --token on the command line is visible to every local \
                     process (ps, /proc/<pid>/cmdline); use TOLE_SERVE_TOKEN or --token-file"
                );
            }
            if socket.is_some() && transport == "mcp" {
                bail!("--socket serves the REST transport only; --transport mcp listens on TCP");
            }
            // The local socket's own credential rules (#1043): a token file is the
            // owner's alone, and a descriptor is read (and closed) before anything else.
            if socket.is_some() {
                if let Some(f) = &token_file {
                    tole_cli::serve_support::transport::check_private_token_file(f)
                        .map_err(anyhow::Error::msg)?;
                }
            }
            let token = match token_fd {
                Some(fd) => Some(
                    tole_cli::serve_support::transport::read_token_fd(fd, 4096)
                        .map_err(anyhow::Error::msg)?,
                ),
                None => token,
            };
            let env_token = std::env::var("TOLE_SERVE_TOKEN").ok();
            let auth = if auth == "basic" {
                let basic = tole_cli::auth::resolve_basic(
                    basic_user,
                    basic_hash_file.as_deref(),
                    std::env::var("TOLE_SERVE_BASIC_USER").ok(),
                    std::env::var("TOLE_SERVE_BASIC_HASH").ok(),
                )
                .map_err(anyhow::Error::msg)?;
                // Basic sends a reusable password on every request: in clear
                // text it is worse than the token, so a bind beyond loopback
                // needs the owner's word that TLS ends in front of it.
                if !tole_cli::auth::is_loopback_bind(&bind) && !allow_insecure_basic {
                    bail!(
                        "refusing --auth basic on --bind {bind}: HTTP Basic over plain HTTP \
                         exposes the password to the network. Keep the default loopback bind \
                         and put a TLS reverse proxy in front, or pass --allow-insecure-basic \
                         if TLS ends in front of this server"
                    );
                }
                // The bearer token is optional here; one that is set works too.
                let token =
                    crate::serve::serve_token_optional(token, token_file.as_deref(), env_token)
                        .map_err(anyhow::Error::msg)?;
                tole_cli::auth::Authenticator::new(token, Some(basic))
            } else {
                if basic_user.is_some() || basic_hash_file.is_some() || allow_insecure_basic {
                    bail!(
                        "--basic-user, --basic-hash-file and --allow-insecure-basic need \
                         --auth basic"
                    );
                }
                tole_cli::auth::Authenticator::token(
                    crate::serve::serve_token(token, token_file.as_deref(), env_token)
                        .map_err(anyhow::Error::msg)?,
                )
            };
            if transport == "mcp" {
                #[cfg(feature = "mcp-http")]
                {
                    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
                    let result = rt.block_on(crate::mcp_http::run_mcp_http(
                        &bind,
                        port,
                        auth,
                        allow_patterns,
                        host.plan_mode,
                        memory,
                        workspace.as_ref(),
                        sessions_dir_override.clone(),
                        host.on_turnend.clone(),
                        shutdown_grace,
                    ));
                    // Dropping the runtime waits for blocking tasks (a turn that
                    // outlived the grace period): do not, the drain has already
                    // given its verdict (#1014).
                    rt.shutdown_timeout(std::time::Duration::ZERO);
                    return result;
                }
                #[cfg(not(feature = "mcp-http"))]
                anyhow::bail!("--transport mcp requires the mcp-http feature");
            }
            // #573: the status snapshot resolves the window as `tole status`.
            tole_cli::status::set_configured_window(
                settings.context_window.as_ref().map(|w| w.value),
            );
            crate::serve::run_serve(crate::serve::ServeConfig {
                bind,
                socket,
                port,
                auth,
                allow_patterns,
                workspace,
                plan_mode: host.plan_mode,
                memory,
                sessions_dir: sessions_dir_override,
                turnend: host.on_turnend.clone(),
                cron,
                // Per-session profiles (#740): the user config only, and
                // only when it is in use (`--no-config` has none).
                profiles: tole_cli::serve_support::profiles::ProfileSource {
                    config: if cli.no_config {
                        None
                    } else {
                        tole_cli::config::user_config_path()
                    },
                    allow: config_apply::values(&settings.allow),
                    trust: trust_extra.clone(),
                    allow_pinned: settings
                        .allow
                        .as_ref()
                        .is_some_and(|e| e.source == config_apply::Source::Flag),
                },
                // Missions over serve (#947): the owner's configuration, read
                // here like `tole mission` reads it (user config, the same
                // trusted tier); a request can only ask for less.
                mission: crate::serve::missions::MissionSettings {
                    verify: settings.verify.as_ref().map(|v| v.value.clone()),
                    verify_timeout_secs: settings.verify_timeout.value,
                    ceiling: mission::BudgetTier::resolve(
                        trust_extra.iter().any(|p| p == "todo_write"),
                        settings.max_steps.as_ref().map(|e| e.value),
                        settings.max_minutes.as_ref().map(|e| e.value),
                        settings.max_tokens.as_ref().map(|e| e.value),
                    ),
                    compaction: host.compaction,
                },
                shutdown_grace,
            })
        }
        Command::Sessions { tree: true, .. } => sessions_tree_command(&sessions_dir),
        Command::Sessions { all: false, .. } => sessions_command(&sessions_dir),
        Command::Sessions {
            all: true, grep, ..
        } => all_sessions_command(grep.as_deref()),
        Command::Branch { id, at } => branch_command(&sessions_dir, &id, at.as_deref()),
        Command::ImportClaude { path } => import_claude_command(&sessions_dir, &path),
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
            let window = settings.context_window.as_ref().map(|w| w.value);
            let ok = tole_cli::doctor::run_cli(
                cfg.as_ref(),
                &config_apply::real_env,
                tier,
                no_ledger,
                window,
            );
            // The delegation resource guard: headroom against its thresholds.
            let cwd = std::env::current_dir().ok();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            for l in tole_cli::machine_guard::doctor_lines(&host.delegation, cwd.as_deref(), now) {
                println!("{l}");
            }
            println!(
                "{}",
                tole_cli::approvals_owner::doctor_line(&host.delegation.cfg)
            );
            // Delegation in use with `approvals` unset (#813): say what the
            // default `off` means and how to opt in.
            let log = host.delegation.data_dir.as_deref().zip(cwd.as_deref());
            let log = log.and_then(|(d, c)| tole_cli::delegate::log_path(d, c));
            if let Some(h) =
                tole_cli::approvals_owner::doctor_hint(&host.delegation.cfg, log.as_deref())
            {
                println!("{h}");
            }
            // The plugin lock (#599): each configured server's approval.
            #[cfg(feature = "mcp")]
            if let Some(cwd) = &cwd {
                let names: Vec<String> = host
                    .mcp_server
                    .iter()
                    .filter_map(|s| s.split('=').next().map(str::to_string))
                    .collect();
                let gate = tole_cli::plugin_gate::Gate::load(
                    tole_core::plugin_lock::Lockfile::user_path(),
                    cwd,
                );
                for l in gate.doctor_lines(&names) {
                    println!("{l}");
                }
                // Unpinned launchers, a third-party server no sandbox
                // applies to, broad grants (#600).
                let configured: Vec<tole_cli::plugin::warn::Configured> = host
                    .mcp_server
                    .iter()
                    .filter_map(|s| tole_core::mcp::McpServerConfig::parse(s).ok())
                    .map(|c| tole_cli::plugin::warn::Configured {
                        name: c.name,
                        stdio: match c.transport {
                            tole_core::mcp::McpTransport::Stdio { command, args } => {
                                Some((command, args))
                            }
                            tole_core::mcp::McpTransport::Http { .. } => None,
                        },
                    })
                    .collect();
                let first_party = tole_cli::plugin_gate::first_party_names();
                for l in tole_cli::plugin::warn::warnings(
                    gate.entries(),
                    &configured,
                    &first_party,
                    tole_core::sandbox::session_policy().as_ref(),
                    &tole_core::sandbox::detect(),
                ) {
                    println!("{l}");
                }
            }
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Route {
            prompt,
            kind,
            job,
            risk,
            budget_left,
        } => {
            use tole_core::router;
            let kind = router::TaskKind::parse(&kind)
                .ok_or_else(|| anyhow!("--kind must be chat, run, mission, cron or delegate"))?;
            let max_risk = router::parse_risk(&risk)
                .ok_or_else(|| anyhow!("--risk must be read_only, write or destructive"))?;
            let brief = router::Brief {
                kind,
                prompt: &prompt,
                max_risk,
                profile: profile.as_ref().map(|p| p.name.as_str()),
                job: job.as_deref(),
                budget_left,
            };
            // The model a run would use without routing, and its source: a
            // matching rule replaces it (#586).
            let configured = settings.provider.model.as_ref().map(|m| {
                let from = match (m.source, m.env) {
                    (config_apply::Source::Env, Some(name)) => format!("env {name}"),
                    (source, _) => source.to_string(),
                };
                (m.value.clone(), from)
            });
            let secrets =
                tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
            let out = tole_cli::routing::explain(
                cfg.as_ref().unwrap_or(&Default::default()),
                &brief,
                configured.as_ref().map(|(m, f)| (m.as_str(), f.as_str())),
                &secrets,
            )
            .map_err(|e| anyhow!(secrets.redact(&e).into_owned()))?;
            print!("{out}");
            Ok(())
        }
        Command::Audit {
            discussions,
            discussion,
            json,
            ..
        } if discussions || discussion.is_some() => {
            let dir = tole_core::discussion_log::DiscussionLog::default_dir().ok_or_else(|| {
                anyhow!("cannot resolve the tole data dir (set CODECORA_HOME or HOME)")
            })?;
            let out = tole_cli::audit_discussion::discussions(
                &dir,
                discussion.as_deref(),
                json,
                &tole_core::secrets::SecretSet::from_env(),
            )
            .map_err(anyhow::Error::msg)?;
            print!("{out}");
            Ok(())
        }
        Command::Discuss { action } => {
            // The servers a helper child can resolve (#1039): the child is a
            // separate `tole mission` that reads the config and the cora
            // preset, and gets no `--mcp-server` flag of this process.
            #[cfg(feature = "mcp")]
            let servers = helper_mcp_servers(cfg.as_ref(), auto_mcp_specs());
            #[cfg(not(feature = "mcp"))]
            let servers: Vec<String> = Vec::new();
            discuss_command(action, cfg.as_ref(), &servers, &host.delegation)
        }
        Command::Delegation { action } => {
            let root = resolve_workspace_root(host.workspace.as_ref())?;
            delegation_command(action, &root)
        }
        Command::Audit { goal, json, .. } => {
            let root = resolve_workspace_root(host.workspace.as_ref())?;
            let data = tole_core::paths::tole_data_dir().ok_or_else(|| {
                anyhow!("cannot resolve the tole data dir (set CODECORA_HOME or HOME)")
            })?;
            let log = tole_cli::delegate::log_path(&data, &root)
                .ok_or_else(|| anyhow!("cannot canonicalize the workspace {}", root.display()))?;
            let out = tole_cli::audit::audit(
                &log,
                goal.as_deref(),
                json,
                &tole_core::secrets::SecretSet::from_env(),
            )
            .map_err(anyhow::Error::msg)?;
            print!("{out}");
            Ok(())
        }
        Command::Status { id } => {
            let id = match id {
                Some(id) => id,
                None => latest_session_id(&sessions_dir)
                    .ok_or_else(|| anyhow!("no sessions found in {}", sessions_dir.display()))?,
            };
            let window = settings.context_window.as_ref().map(|w| w.value);
            status_command(&sessions_dir, &id, window)
        }
        #[cfg(feature = "tui")]
        Command::Tui {
            resume,
            allow_patterns: _,
            yes,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            tui_command(
                &sessions_dir,
                settings.system_prompt.as_ref().map(|e| e.value.as_str()),
                resume,
                &allow_patterns,
                yes,
                &host,
                &settings,
            )
        }
        Command::Chat {
            system: _,
            resume,
            last,
            allow_patterns: _,
            yes,
            images,
        } => {
            let allow_patterns = with_trust(config_apply::values(&settings.allow), &trust_extra);
            let images = tole_cli::turn_runner::resolve_images(&images)?;
            chat_command(
                &sessions_dir,
                settings.system_prompt.as_ref().map(|e| e.value.as_str()),
                resume,
                last,
                &allow_patterns,
                yes,
                &host,
                images,
            )
        }
    }
}

#[cfg(test)]
#[path = "main_gate_tests.rs"]
mod gate_policy_tests;
