//! How a new session's system prompt is composed (the first step of #941).
//!
//! The pieces used to be private functions of the `tole` binary, so only the
//! CLI commands could reach them: the explicit `TOLE_SYSTEM_PROMPT`, the
//! built-in default prompt (plan-mode variant, one context line), and the
//! cora review guidance that is named only when both review tools are
//! registered. Moved here unchanged so every face can call the same code.
//!
//! `evals/replay` reads [`default_system_prompt`] out of this file's source
//! (grep-based on purpose, so a copy cannot drift); keep the function name
//! and the single string literal.

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
