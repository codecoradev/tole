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
///
/// `workspace` (#437) is `read_only` plus `write_file` / `edit_file`
/// through the path guard ([`tole_core::approval::WORKSPACE_GUARD`]). The
/// file tools are jailed to the workspace root (`--workspace`, else the
/// cwd; canonicalized, so `../` and symlinks cannot escape), and the guard
/// keeps dot-paths (`.git/hooks`, `.envrc`, `.vscode/`, `.tole/`, ...),
/// build / task files (`build.rs`, `Makefile`, `package.json`, ...) and
/// scripts prompting, since a later tool may execute those. Ordinary
/// source files stay writable and the user may later run them: the preset
/// is for a workspace whose results the user reviews, not a sandbox.
/// `delete_file` (Destructive) and `run_command` keep prompting;
/// [`check_workspace_root`] requires an explicit workspace and refuses a
/// home or filesystem root.
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
    (
        "workspace",
        &[
            "read_file",
            "verify_package",
            "uteke_recall",
            "cora_search",
            "tole_session_status",
            "tole_session_list",
            "job_poll",
            "write_file@workspace",
            "edit_file@workspace",
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

/// The `workspace` preset (#437) needs an EXPLICIT workspace (`--workspace`
/// or the config `workspace` key), never the implicit cwd, which is usually
/// a whole repository; and it is refused when that root is the user's home
/// or a filesystem root. `presets` are the effective preset names, `root`
/// the explicit workspace (`None` = not given), `home` the user's home.
pub fn check_workspace_root(
    presets: &[String],
    root: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> Result<()> {
    if !presets.iter().any(|p| p.eq_ignore_ascii_case("workspace")) {
        return Ok(());
    }
    let Some(root) = root else {
        anyhow::bail!(
            "--trust workspace needs an explicit workspace: pass --workspace <dir> \
             (or set `workspace` in the config) for the directory it may write to"
        );
    };
    let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let root = canon(root);
    let is_home = home.is_some_and(|h| canon(h) == root);
    if root.parent().is_none() || is_home {
        anyhow::bail!(
            "--trust workspace refuses the workspace {}: it is your home or a filesystem \
             root; pass --workspace <project dir>",
            root.display()
        );
    }
    Ok(())
}
