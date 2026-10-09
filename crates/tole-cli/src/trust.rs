//! Trust presets (issue #159): the single source of truth for what a
//! preset name expands to, shared by the `--trust` flag and the
//! `.tole/config.toml` loader so both reject exactly the same names.

use anyhow::Result;

/// Trust presets (issue #159): one word for "auto-allow the fleet's own
/// ecosystem tools". Pure sugar — the expanded patterns feed the SAME
/// AllowlistApprover machinery as `--allow`, so enforcement (and the
/// Destructive-never-allowed invariant) is unchanged. `internal` covers
/// the probe-gated native integrations (uteke_*, cora_search) plus the
/// cora MCP auto-preset surface (mcp_cora_*) and the always-safe
/// verify_package/job tools plus `agent_poll` (Write since #300; exact name,
/// so `agent_start` still prompts); it deliberately excludes the write-capable
/// native tools (write_file/edit_file/run_command/git/gh), which keep
/// prompting.
pub const TRUST_PRESETS: &[(&str, &[&str])] = &[
    (
        "internal",
        &[
            "uteke_*",
            "cora_search",
            "mcp_cora_*",
            "verify_package",
            "job_*",
            "agent_poll",
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

/// Expand `--trust` preset names into extra allow patterns. Unknown
/// preset names are a hard error — a typo silently narrowing trust would
/// be worse than failing. `none`/empty → no extra patterns.
pub fn expand_trust(presets: &[String]) -> Result<Vec<String>> {
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
