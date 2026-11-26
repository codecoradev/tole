//! What `tole config check` prints: the IGNORED notes, the startup warnings and the report.

use super::{
    discover, layered, load, render_with, sandbox, Annotations, Config, ConfigError,
    CONFIG_REL_PATH, PROJECT_TAG, USER_TAG,
};
use std::path::Path;

/// Suffix of a project config's `tui` lines in `config check` (#463).
pub const TUI_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the theme, colors or keys (user config only; the theme also TOLE_THEME)";

/// Suffix of a project config's `cron` lines in `config check` (#579).
pub const CRON_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set scheduler limits (user config only)";
/// Suffix of a project config's `discussions` lines in `config check` (#786).
pub const DISCUSSIONS_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the discussion request timeout (user config only; also TOLE_DISCUSSION_TIMEOUT_SECS)";
/// Suffix of a project config's `provider` lines in `config check` (#1236).
pub const PROVIDER_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the provider request timeout (user config only; also TOLE_REQUEST_TIMEOUT_SECS)";
/// Suffix of a project config's `sandbox` lines in `config check` (#601).
pub const SANDBOX_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set the sandbox (user config only)";
/// Suffix of a project config's `curator` lines in `config check` (#593).
pub const CURATOR_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot change the skill curator (user config only)";
/// Suffix of a project config's `delegation` lines in `config check` (#585).
pub const DELEGATION_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot set delegation limits (user config only)";
/// Suffix of a project config's `profiles` line in `config check` (#584).
pub const PROFILES_IGNORED_NOTE: &str =
    "  IGNORED: a project config cannot define profiles (user config only)";

/// Suffix of a project config's `secrets` lines in `config check` (#598).
pub const SECRETS_PROJECT_NOTE: &str =
    "  a project config can only narrow a secret's tools (widening is ignored)";

/// The warning printed at startup when a project config carries `[profiles]`.
pub fn profiles_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `profiles` in project config {shown}: profiles can only be defined in the \
         user config"
    )
}

/// The warning printed at startup when a project config carries `[hooks]`.
pub fn hooks_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `hooks` in project config {shown}: webhook routes can only be defined in \
         the user config"
    )
}

/// The warning printed at startup when a project config carries
/// `[webhook_out]` (#911).
pub fn webhook_out_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `webhook_out` in project config {shown}: outbound webhook destinations can \
         only be defined in the user config"
    )
}

/// The warning printed at startup when a project config carries
/// `redaction_allowlist` (#597).
pub fn redaction_allowlist_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `redaction_allowlist` in project config {shown}: the secret-shape allowlist \
         can only be set in the user config"
    )
}

/// The warning printed at startup when a project config carries `[tui]`.
pub fn tui_ignored_warning(shown: &str) -> String {
    format!(
        "ignoring `tui` in project config {shown}: the theme, colors and keys can only be set in \
         the user config (the theme also with TOLE_THEME)"
    )
}

/// `tole config check`: returns the text to print, or the error. `cwd`,
/// `explicit` and the user config path are parameters (no global state).
/// `annotate` supplies the per-key suffixes from the shared resolution
/// over the LAYERED config (see `config_apply::check`), so a user key a
/// project key overrides shows the project as its effective source.
pub fn check(
    cwd: &Path,
    explicit: Option<&Path>,
    user: Option<&Path>,
    annotate: &dyn Fn(&Config) -> Annotations,
) -> Result<String, ConfigError> {
    let user_cfg = match user {
        Some(p) if p.exists() => Some((p.to_path_buf(), load(p)?)),
        _ => None,
    };
    let project = match discover(cwd, explicit) {
        Some(path) => Some((path.clone(), load(&path)?)),
        None => None,
    };
    let none = Config::default();
    let merged = layered(
        project.as_ref().map_or(&none, |(_, c)| c),
        user_cfg.as_ref().map_or(&none, |(_, c)| c),
    );
    let ann = annotate(&merged);
    let mut lines = Vec::new();
    let section = |lines: &mut Vec<String>, cfg: &Config, tag: &str| {
        let rendered = render_with(cfg, tag, &ann);
        if rendered.is_empty() {
            lines.push("(valid; no keys set)".to_string());
        } else {
            lines.extend(rendered);
        }
    };
    match (&user_cfg, user) {
        (Some((path, cfg)), _) => {
            let shown = path.canonicalize().unwrap_or(path.clone());
            lines.push(format!("user config: {}", shown.display()));
            section(&mut lines, cfg, USER_TAG);
        }
        (None, Some(p)) => lines.push(format!("no user config at {}", p.display())),
        (None, None) => lines.push("no user config (set CODECORA_HOME or HOME)".into()),
    }
    match &project {
        Some((path, cfg)) => {
            let shown = path.canonicalize().unwrap_or(path.clone());
            lines.push(format!("config: {}", shown.display()));
            section(&mut lines, cfg, PROJECT_TAG);
        }
        None => lines.push(format!("no {CONFIG_REL_PATH} in {}", cwd.display())),
    }
    // The sandbox a session would use: only the user layer feeds it.
    if let Some(s) = &merged.sandbox {
        lines.extend(sandbox::effective_lines(s));
    }
    lines.push(
        "note: precedence is flag > env > config > user config > default; a higher layer \
         replaces a whole list key, flags can only turn boolean keys on, and --no-config \
         ignores both files."
            .into(),
    );
    Ok(lines.join("\n"))
}
