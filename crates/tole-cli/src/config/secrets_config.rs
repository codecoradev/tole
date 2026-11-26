//! `[secrets]` bindings, `sensitive_files` and their layering rules (#598, #683).

use super::Config;
use serde::Deserialize;
use std::collections::BTreeMap;

/// One `[secrets]` entry (#598): the tools (and tole launchers) whose
/// child processes may receive this secret. The KEY is the environment
/// variable's NAME; a value never belongs in a config file.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretBinding {
    pub tools: Vec<String>,
}

/// A `[secrets]` key: a secret-shaped environment variable NAME, or a
/// `*_URL` / `*_URI` name (a connection string with a password is scrubbed
/// for its value, #682, so it must be grantable).
pub(super) fn check_secret(name: &str, b: &SecretBinding) -> Result<(), String> {
    // Never echo the key or an entry: either may be a value pasted by mistake.
    let url_name = name.ends_with("_URL") || name.ends_with("_URI");
    if !valid_grant_name(name) || !(tole_core::secrets::is_secret_name(name) || url_name) {
        return Err(
            "secrets: every key must be the NAME of a secret environment variable \
                    ([A-Z0-9_], containing TOKEN, SECRET, API_KEY, PASSWORD, ... or ending \
                    in _KEY, _URL, ...), e.g. GITHUB_TOKEN or REDIS_URL; other variables \
                    already reach every child"
                .into(),
        );
    }
    let tool_ok = |t: &str| {
        !t.is_empty()
            && t.len() <= 64
            && t.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    };
    if !b.tools.iter().all(|t| tool_ok(t)) {
        return Err(format!(
            "secrets.{name}.tools: every entry must be a tool name of a-z, 0-9 or `_` \
             (e.g. \"gh\", \"git\", \"run_command\")"
        ));
    }
    Ok(())
}

/// A `sensitive_files` entry (#683): a bare file NAME glob (`*` wildcards),
/// no directory part, at most 128 chars, no control characters.
pub(super) fn check_sensitive_glob(g: &str) -> Result<(), String> {
    if g.trim().is_empty()
        || g.len() > 128
        || g.contains(['/', '\\'])
        || g.chars().any(char::is_control)
        || g.chars().all(|c| c == '*')
    {
        return Err(
            "sensitive_files: every entry must be a file name or a name glob with `*` \
             (no directory, not only `*`), e.g. \"*.secret\" or \"vault-*.json\""
                .into(),
        );
    }
    Ok(())
}

/// The sensitive-name globs of the layered config (#683): the user layer's
/// list followed by the project's. A project can only ADD names (more
/// files are redacted); nothing a config says can make a built-in name
/// readable.
pub(super) fn layered_sensitive(
    project: &Option<Vec<String>>,
    user: &Option<Vec<String>>,
) -> Option<Vec<String>> {
    if project.is_none() && user.is_none() {
        return None;
    }
    let mut out: Vec<String> = user.clone().unwrap_or_default();
    for g in project.iter().flatten() {
        if !out.contains(g) {
            out.push(g.clone());
        }
    }
    Some(out)
}

/// The consumers a layer leaves `name` bound to: its own entry, else the
/// built-in default (`GITHUB_TOKEN` for gh, git, hook and mcp).
fn bound_tools(layer: &Option<BTreeMap<String, SecretBinding>>, name: &str) -> Vec<String> {
    match layer.as_ref().and_then(|m| m.get(name)) {
        Some(b) => b.tools.clone(),
        None => tole_core::secret_release::Release::default()
            .tools(name)
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default(),
    }
}

/// The `[secrets]` of the layered config (#598): the user layer's entries,
/// where a project entry may only NARROW a binding (its tools ∩ the user's
/// or the default's). A project file comes from a cloned repository; it
/// must never hand a secret to a tool the user did not.
pub(super) fn layered_secrets(
    project: &Option<BTreeMap<String, SecretBinding>>,
    user: &Option<BTreeMap<String, SecretBinding>>,
) -> Option<BTreeMap<String, SecretBinding>> {
    let mut out = user.clone().unwrap_or_default();
    for (name, b) in project.iter().flatten() {
        let allowed = bound_tools(user, name);
        let tools = b
            .tools
            .iter()
            .filter(|t| allowed.contains(t))
            .cloned()
            .collect();
        out.insert(name.clone(), SecretBinding { tools });
    }
    (project.is_some() || user.is_some()).then_some(out)
}

/// One warning per project `[secrets]` entry that tries to WIDEN a binding
/// (those tools are refused), naming the remedy.
pub fn secrets_widening_warnings(project: &Config, user: &Config, shown: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (name, b) in project.secrets.iter().flatten() {
        let allowed = bound_tools(&user.secrets, name);
        let refused: Vec<&str> = b
            .tools
            .iter()
            .filter(|t| !allowed.contains(t))
            .map(String::as_str)
            .collect();
        if !refused.is_empty() {
            out.push(format!(
                "ignoring the grant of {name} to {} in project config {shown}: a project config \
                 can only narrow a secret's tools. To grant it, add the tools to `{name}` under \
                 [secrets] in your user config",
                refused.join(", ")
            ));
        }
    }
    out
}

impl Config {
    /// The secret release this config sets (#598): the defaults, with each
    /// `[secrets]` entry replacing its name's binding.
    pub fn release(&self) -> tole_core::secret_release::Release {
        let mut r = tole_core::secret_release::Release::default();
        for (name, b) in self.secrets.iter().flatten() {
            r = r.with(name, b.tools.iter().cloned());
        }
        r
    }
}

/// An environment variable NAME as a grant: `[A-Z0-9_]`, not starting with
/// a digit. Anything else (a value pasted by mistake) is refused.
pub(super) fn valid_grant_name(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}
