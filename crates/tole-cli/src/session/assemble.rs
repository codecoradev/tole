//! How a new session's system prompt is composed (the first step of #941).
//!
//! The pieces used to be private functions of the `tole` binary, so only the
//! CLI commands could reach them: the explicit `TOLE_SYSTEM_PROMPT`, the
//! built-in default prompt (plan-mode variant, one context line), and the
//! cora review guidance that is named only when both review tools are
//! registered, and the skills wiring (`apply_skills`) with the workspace
//! resolution it needs. Moved here unchanged so every face can call the same
//! code.
//!
//! `evals/replay` reads [`default_system_prompt`] out of this file's source
//! (grep-based on purpose, so a copy cannot drift); keep the function name
//! and the single string literal.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tole_core::tool::ToolRegistry;

/// Resolution order: --system flag > TOLE_SYSTEM_PROMPT env > none.
/// The flag is handled by clap (not parsed here); env is the fallback.
pub fn resolve_system_prompt() -> Option<String> {
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
pub fn default_system_prompt() -> &'static str {
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
pub fn default_system_prompt() -> &'static str {
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
pub fn build_default_prompt(plan_mode: bool) -> String {
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    default_prompt_with_context(plan_mode, &cwd)
}

/// [`build_default_prompt`] for a session whose working directory is not the
/// process cwd: a server session (`serve`, ACP, MCP over HTTP) jails its tools
/// to its own workspace, so the context line names that (#945). The text is
/// otherwise identical to the CLI's.
pub fn build_default_prompt_in(plan_mode: bool, workspace: &std::path::Path) -> String {
    default_prompt_with_context(plan_mode, &workspace.display().to_string())
}

fn default_prompt_with_context(plan_mode: bool, cwd: &str) -> String {
    let mut p = default_prompt_for(plan_mode);
    p.push_str(&format!(
        "\nContext: working directory {cwd}; today is {} (UTC).",
        today_utc()
    ));
    p
}

/// The system prompt a NEW server session pins in its header (#945): an
/// explicit prompt (profile persona, `TOLE_SYSTEM_PROMPT`, trusted config)
/// wins, exactly as `--system` does on the CLI; otherwise the shared default
/// for the session's mode and workspace. The cora review guidance follows
/// either one, only when both review tools are in `registry`.
pub fn server_session_prompt(
    explicit: Option<String>,
    plan_mode: bool,
    workspace: &std::path::Path,
    registry: &ToolRegistry,
) -> String {
    let base = explicit.unwrap_or_else(|| build_default_prompt_in(plan_mode, workspace));
    format!("{base}{}", review_section(registry))
}

/// The cora agent-as-reviewer flow (#422, cora >= 0.19): the calling
/// model judges a diff itself, so cora needs no LLM key. Named in the
/// system prompt only when both tools are registered (absent in plan
/// mode, where MCP tools are filtered out, and without `cora mcp`).
pub const CORA_REVIEW_SECTION: &str = "\n\n# Code review\nTo review a change: get its \
unified diff (e.g. `git diff --cached` or `git diff <base>`), call \
mcp_cora_cora_review_prepare with it, judge the diff yourself following \
the returned instructions and response_schema, then call \
mcp_cora_cora_review_finalize with the SAME diff and your findings (a JSON \
array, [] when none) and report its verdict, including should_block.";

pub fn review_section(registry: &ToolRegistry) -> &'static str {
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

/// Civil date `YYYY-mm-dd` from days since the Unix epoch (Howard
/// Hinnant's algorithm — no chrono dep).
pub fn civil_from_days(days: i64) -> String {
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
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86400) as i64)
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

/// Skills wiring (issue #161), used by the CLI commands (run, chat, the TUI).
/// Server faces do not call it yet (#945). Loads `--skill`
/// files (loud error on broken ones), and when the workspace exposes a
/// non-empty skills dir, registers the ReadOnly load_skill tool and
/// returns the index/section block to append to the system prompt.
/// `--no-skills` short-circuits everything.
///
/// Discovery root: an explicit `--workspace` wins; absent flags fall
/// back to the CURRENT DIRECTORY — the same default the file-tools
/// jail uses (found by the 2026-10-05 sweep: a project with
/// `<cwd>/skills/` and no flag silently got no discovery).
///
/// `brief` is the owner-written task (the `run` prompt): with
/// `TOLE_SKILL_INDEX=uteke` it is the recall query for the skills shown
/// (#590); interactive sessions pass `None` and get the pinned core.
pub fn apply_skills(
    skills: &[PathBuf],
    workspace: Option<&String>,
    no_skills: bool,
    registry: &mut ToolRegistry,
    brief: Option<&str>,
) -> Result<String> {
    if no_skills {
        return Ok(String::new());
    }
    let ws_root = resolve_workspace_root(workspace)?;
    let (gate, notes) = crate::skill_approve::gate_from_env().map_err(anyhow::Error::msg)?;
    for note in notes {
        eprintln!("tole: {note}");
    }
    let recall = crate::skill_recall::SkillRecall {
        gate,
        ..crate::skill_recall::SkillRecall::from_env(brief).map_err(anyhow::Error::msg)?
    };
    apply_skills_in(skills, Some(ws_root), false, registry, &recall)
}

/// Testable core of [`apply_skills`] taking the RESOLVED discovery
/// root (None = caller already resolved to cwd; kept Option so tests
/// can pass a tmpdir without changing the process cwd). `recall.gate`
/// decides which discovered skills may load (#591); `--skill` files are
/// the owner's own choice and are not gated.
pub fn apply_skills_in(
    skills: &[PathBuf],
    ws_root: Option<PathBuf>,
    no_skills: bool,
    registry: &mut ToolRegistry,
    recall: &crate::skill_recall::SkillRecall,
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
        let (block, notes) =
            crate::skill_recall::skills_section(recall, ws_path.as_deref(), registry)
                .map_err(anyhow::Error::msg)?;
        for note in notes {
            eprintln!("tole: {note}");
        }
        sections.push_str(&block);
    }
    Ok(sections)
}

/// Routes a NEW session once (#586), before its provider is built: off
/// unless `[router] enabled = true`; a session that already has a model is
/// left alone. A broken router fails the start with its remedy, with secrets
/// redacted. The one-line decision goes to stderr unless `quiet` (the TUI
/// owns the screen). Moved here from the `tole` binary so any face can call
/// it (#943); which faces do is the face policy's business (#945).
#[allow(clippy::too_many_arguments)]
pub fn route_session(
    storage: &mut dyn tole_core::storage::Storage,
    setup: &crate::delegate::Setup,
    profile: Option<&str>,
    kind: tole_core::router::TaskKind,
    prompt: &str,
    registry: &ToolRegistry,
    quiet: bool,
    budget_left: Option<u64>,
) -> Result<()> {
    let brief = tole_core::router::Brief {
        kind,
        prompt,
        max_risk: registry.max_risk(),
        profile,
        job: None,
        budget_left,
    };
    let secrets = crate::provider_chain::process_secrets(&crate::config_apply::real_env);
    let line = crate::routing::route_new_session(storage, &setup.cfg, &brief, &secrets)
        .map_err(|e| anyhow::anyhow!(secrets.redact(&e).into_owned()))?;
    if let (Some(l), false) = (line, quiet) {
        eprintln!("{l}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Face policy (#943)
// ---------------------------------------------------------------------------

/// Where a face's shell tools (`run_command`, `git`, `gh`) run. Files are
/// jailed to the workspace on every face; only the shell directory differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellDir {
    /// The process working directory (the CLI faces and `tole mcp`).
    ProcessCwd,
    /// The session workspace (server faces and cron).
    Workspace,
}

/// Which child-agent tools a face gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentsPolicy {
    /// None.
    None,
    /// The classic depth-1 agent tools, without delegation (serve, ACP).
    Classic,
    /// `delegate` / `delegate_poll` over the delegation tree (the CLI faces).
    Delegating,
}

/// What differs between faces when a session's tool roster is built, as plain
/// data: the capabilities a face may have and where its shell tools run.
/// Identity, auth, input/output format and the approval channel are NOT here
/// (they are the face's own, see the epic #941). Each constant is the value
/// the face had before this table existed; nothing changes by moving them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FacePolicy {
    /// A short name, for tests and diagnostics.
    pub name: &'static str,
    pub shell_dir: ShellDir,
    pub agents: AgentsPolicy,
    /// May the session use MCP client servers (`--mcp-server`, the cora
    /// auto-preset)? A security boundary: off wherever the face cannot
    /// vouch for the operator's choice.
    pub mcp_clients: bool,
}

impl FacePolicy {
    /// `run`, `chat`, `resume`, the TUI and `mission`.
    pub const CLI: FacePolicy = FacePolicy {
        name: "cli",
        shell_dir: ShellDir::ProcessCwd,
        agents: AgentsPolicy::Delegating,
        mcp_clients: true,
    };
    /// `tole mcp` and the MCP-over-HTTP host (their file tools).
    pub const MCP_SERVER: FacePolicy = FacePolicy {
        name: "mcp-server",
        shell_dir: ShellDir::ProcessCwd,
        agents: AgentsPolicy::None,
        mcp_clients: false,
    };
    /// A session hosted by `tole serve` or ACP.
    pub const SERVER_SESSION: FacePolicy = FacePolicy {
        name: "server-session",
        shell_dir: ShellDir::Workspace,
        agents: AgentsPolicy::Classic,
        mcp_clients: false,
    };
    /// A cron run.
    pub const CRON: FacePolicy = FacePolicy {
        name: "cron",
        shell_dir: ShellDir::Workspace,
        agents: AgentsPolicy::None,
        mcp_clients: false,
    };
}

/// The per-session inputs of [`roster_for`] that are not the face's policy.
pub struct RosterInputs<'a> {
    /// The file-tool jail root.
    pub workspace: PathBuf,
    pub plan_mode: bool,
    /// The parent's `--allow` patterns, inherited by child agents.
    pub allow_patterns: &'a [String],
    /// Start each child agent in its own git worktree (CLI only).
    pub agents_worktree: bool,
    /// The delegation setup, required by [`AgentsPolicy::Delegating`].
    pub delegation: Option<&'a crate::delegate::Setup>,
    /// Raw `--mcp-server` specs, read only when the policy allows MCP
    /// clients.
    pub mcp_specs: &'a [String],
}

/// Build the [`Roster`](crate::tool_roster::Roster) a face asks of the shared
/// tool roster, reading every face difference from `policy`.
pub fn roster_for(policy: &FacePolicy, i: RosterInputs<'_>) -> Result<crate::tool_roster::Roster> {
    use crate::tool_roster::{AgentTools, Roster};
    let mut roster = Roster::new(i.workspace);
    if policy.shell_dir == ShellDir::ProcessCwd {
        roster.shell_dir = std::env::current_dir().context("resolving cwd")?;
    }
    roster.plan_mode = i.plan_mode;
    roster.agents = match policy.agents {
        AgentsPolicy::None => None,
        AgentsPolicy::Classic => Some(AgentTools {
            worktrees: false,
            parent_allows: i.allow_patterns.to_vec(),
            delegation: None,
        }),
        AgentsPolicy::Delegating => Some(AgentTools {
            worktrees: i.agents_worktree,
            parent_allows: i.allow_patterns.to_vec(),
            delegation: Some(
                i.delegation
                    .context("a delegating face needs its delegation setup")?
                    .clone(),
            ),
        }),
    };
    #[cfg(feature = "mcp")]
    if policy.mcp_clients {
        roster.mcp_servers = i
            .mcp_specs
            .iter()
            .map(|s| tole_core::mcp::McpServerConfig::parse(s))
            .collect::<Result<Vec<_>, String>>()
            .map_err(anyhow::Error::msg)?;
    }
    #[cfg(not(feature = "mcp"))]
    let _ = (policy.mcp_clients, i.mcp_specs);
    Ok(roster)
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
    fn server_session_prompt_composes_default_explicit_and_review() {
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
        let ws = std::path::Path::new("/some/ws");
        let mut reg = tole_core::tool::ToolRegistry::with_approver(
            tole_core::approval::AllowlistApprover::allow_only(vec![]),
        );
        let d = super::server_session_prompt(None, false, ws, &reg);
        assert!(d.starts_with(super::default_system_prompt()));
        assert!(d.contains("Context: working directory /some/ws; today is "));
        assert!(!d.contains("# Code review"));
        let e = super::server_session_prompt(Some("mine".into()), false, ws, &reg);
        assert_eq!(e, "mine");
        // The plan-mode sentence, as on the CLI.
        assert!(super::server_session_prompt(None, true, ws, &reg).contains("PLAN MODE"));
        reg.register(Box::new(Named("mcp_cora_cora_review_prepare")))
            .unwrap();
        assert!(!super::server_session_prompt(None, false, ws, &reg).contains("# Code review"));
        reg.register(Box::new(Named("mcp_cora_cora_review_finalize")))
            .unwrap();
        assert!(super::server_session_prompt(None, false, ws, &reg)
            .ends_with(super::CORA_REVIEW_SECTION));
        assert_eq!(
            super::server_session_prompt(Some("mine".into()), false, ws, &reg),
            format!("mine{}", super::CORA_REVIEW_SECTION)
        );
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
        let dir = crate::test_dir::TestDir::new("tole-ws");
        let ws = dir.to_path_buf().into_os_string().into_string().unwrap();
        let resolved = resolve_workspace_root(Some(&ws)).unwrap();
        assert_eq!(resolved, dir.canonicalize().unwrap());
    }

    #[test]
    fn workspace_missing_dir_rejected() {
        let err = resolve_workspace_root(Some(&"/nonexistent/tole/ws".to_string()));
        assert!(err.is_err());
    }

    #[test]
    fn workspace_file_rejected() {
        let d = crate::test_dir::TestDir::new("tole-ws-file");
        let f = d.join("file");
        std::fs::write(&f, "x").unwrap();
        let ws = f.into_os_string().into_string().unwrap();
        let err = resolve_workspace_root(Some(&ws));
        assert!(err.is_err());
    }

    #[test]
    fn workspace_none_falls_back_to_cwd() {
        let resolved = resolve_workspace_root(None).unwrap();
        assert_eq!(resolved, std::env::current_dir().unwrap());
    }
}

#[cfg(test)]
mod skills_wiring_tests {
    use super::*;

    #[test]
    fn apply_skills_loads_explicit_skill_files_and_registers_tool() {
        let dir = crate::test_dir::TestDir::new("tole-skills-wire");
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
            None,
        )
        .unwrap();
        assert!(sections.contains("# Skill: demo"));
        assert!(sections.contains("body"));
        assert!(reg.get("load_skill").is_some(), "load_skill registered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_skills_no_skills_short_circuits() {
        let dir = crate::test_dir::TestDir::new("tole-skills-off");
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
            None,
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
        let dir = crate::test_dir::TestDir::new("tole-skills-cwd");
        std::fs::create_dir_all(dir.join("skills").join("demo")).unwrap();
        std::fs::write(
            dir.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: \"d\"\n---\nbody",
        )
        .unwrap();
        let mut reg = ToolRegistry::new();
        let sections = apply_skills_in(
            &[],
            Some(dir.to_path_buf()),
            false,
            &mut reg,
            &Default::default(),
        )
        .unwrap();
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

#[cfg(test)]
mod face_policy_tests {
    use super::*;

    fn inputs<'a>(allow: &'a [String], d: Option<&'a crate::delegate::Setup>) -> RosterInputs<'a> {
        RosterInputs {
            workspace: std::env::temp_dir(),
            plan_mode: false,
            allow_patterns: allow,
            agents_worktree: true,
            delegation: d,
            mcp_specs: &[],
        }
    }

    /// The roster of each face, pinned (#943): what the face had before
    /// `FacePolicy` existed.
    #[test]
    fn each_face_keeps_the_roster_it_had() {
        let allow = vec!["read_file".to_string()];
        let d = crate::delegate::Setup::default();
        let cwd = std::env::current_dir().unwrap();

        let r = roster_for(&FacePolicy::CLI, inputs(&allow, Some(&d))).unwrap();
        assert_eq!(r.shell_dir, cwd);
        let a = r.agents.expect("cli agents");
        assert!(a.worktrees && a.delegation.is_some());
        assert_eq!(a.parent_allows, allow);

        let r = roster_for(&FacePolicy::MCP_SERVER, inputs(&allow, None)).unwrap();
        assert_eq!(r.shell_dir, cwd);
        assert!(r.agents.is_none());

        let r = roster_for(&FacePolicy::SERVER_SESSION, inputs(&allow, None)).unwrap();
        assert_eq!(r.shell_dir, r.workspace);
        let a = r.agents.expect("server agents");
        assert!(!a.worktrees && a.delegation.is_none());
        assert_eq!(a.parent_allows, allow);

        let r = roster_for(&FacePolicy::CRON, inputs(&allow, None)).unwrap();
        assert_eq!(r.shell_dir, r.workspace);
        assert!(r.agents.is_none());
    }

    #[test]
    fn delegating_without_a_setup_is_refused() {
        assert!(roster_for(&FacePolicy::CLI, inputs(&[], None)).is_err());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_specs_reach_only_faces_that_may_use_clients() {
        let specs = vec!["fs=npx x".to_string()];
        let d = crate::delegate::Setup::default();
        let mk = |p: &FacePolicy| {
            let mut i = inputs(&[], Some(&d));
            i.mcp_specs = &specs;
            roster_for(p, i).map(|r| r.mcp_servers.len())
        };
        assert_eq!(mk(&FacePolicy::CLI).unwrap(), 1);
        for p in [
            FacePolicy::MCP_SERVER,
            FacePolicy::SERVER_SESSION,
            FacePolicy::CRON,
        ] {
            assert_eq!(mk(&p).unwrap(), 0, "{}", p.name);
        }
    }
}
