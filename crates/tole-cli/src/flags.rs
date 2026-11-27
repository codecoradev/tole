//! The flag helpers of the command fns: the trust list, the serve URL, the
//! refusals of config-sourced flags and the server-face flag checks.

use crate::HostConfig;
use anyhow::Result;
use std::path::PathBuf;

#[cfg(test)]
mod tests;

/// Flag-over-env trust list (the pre-config rule, #159), kept as a thin
/// test oracle over the shared [`config_apply::resolve_list`]: startup
/// resolves `trust` through `Settings::resolve` (flag > TOLE_TRUST env >
/// config), whose first two layers must stay exactly this behavior.
#[cfg(test)]
pub(crate) fn resolve_trust_flags(flag: &[String], env_val: Option<String>) -> Vec<String> {
    let env = env_val
        .map(|v| tole_cli::config_apply::split_env_list(&v))
        .unwrap_or_default();
    tole_cli::config_apply::resolve_list(flag, &env, None)
        .map(|(v, _)| v)
        .unwrap_or_default()
}

/// Default base URL of a `tole serve` instance for `tole approvals`.
pub(crate) const DEFAULT_SERVE_URL: &str = "http://127.0.0.1:7801";

/// Effective serve URL for `tole approvals` (#339): an explicit `--url`
/// wins over the `TOLE_SERVE_URL` env, which wins over the default. An
/// empty env value counts as unset.
pub(crate) fn resolve_serve_url(flag: Option<String>, env_val: Option<String>) -> String {
    flag.or_else(|| env_val.filter(|v| !v.is_empty()))
        .unwrap_or_else(|| DEFAULT_SERVE_URL.to_string())
}

/// Append the trust-preset patterns to the user's `--allow` list.
pub(crate) fn with_trust(mut allow_patterns: Vec<String>, trust_extra: &[String]) -> Vec<String> {
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
pub(crate) fn check_client_session_flags(
    face: &str,
    skills: &[PathBuf],
    no_skills: bool,
    explicit_mcp_servers: bool,
) -> Result<()> {
    if !skills.is_empty() {
        anyhow::bail!(
            "--skill is not supported by `tole {face}` (skills load into \
             run/chat/resume and serve/acp session system prompts)"
        );
    }
    if no_skills {
        anyhow::bail!(
            "--no-skills is not supported by `tole {face}` (it serves no sessions, so \
             it loads no skills — nothing to disable)"
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
pub(crate) fn refuse(msg: &str, layer: Option<tole_cli::config_apply::Source>) -> anyhow::Error {
    refuse_for(msg, layer, false)
}

/// [`refuse`] for `tole serve`, `acp` and `mcp`: `--no-config` is refused
/// there (#855), so the way out is to edit the config file.
#[cfg(feature = "shell-tools")]
pub(crate) fn refuse_unattended(
    msg: &str,
    layer: Option<tole_cli::config_apply::Source>,
) -> anyhow::Error {
    refuse_for(msg, layer, true)
}

pub(crate) fn refuse_for(
    msg: &str,
    layer: Option<tole_cli::config_apply::Source>,
    unattended: bool,
) -> anyhow::Error {
    let Some(layer) = layer else {
        return anyhow::anyhow!("{msg}");
    };
    let layer = match layer {
        tole_cli::config_apply::Source::UserConfig => "user config",
        _ => "project config",
    };
    if unattended {
        anyhow::anyhow!(
            "{msg} (the value comes from the {layer}; remove it from that config file, \
             --no-config is refused for this face)"
        )
    } else {
        anyhow::anyhow!(
            "{msg} (the value comes from the {layer}; \
             use --no-config to ignore the config files)"
        )
    }
}

/// The config layer a value came from, if it came from a config file
/// (project or user, #433).
pub(crate) fn config_layer(
    s: tole_cli::config_apply::Source,
) -> Option<tole_cli::config_apply::Source> {
    use tole_cli::config_apply::Source;
    matches!(s, Source::Config | Source::UserConfig).then_some(s)
}

pub(crate) fn from_config<T>(
    e: &tole_cli::config_apply::ListEff<T>,
) -> Option<tole_cli::config_apply::Source> {
    e.as_ref().and_then(|e| config_layer(e.source))
}

pub(crate) fn pretool_from_config(
    s: &tole_cli::config_apply::Settings,
) -> Option<tole_cli::config_apply::Source> {
    from_config(&s.on_pretool).or_else(|| from_config(&s.on_posttool))
}

/// [`check_client_session_flags`] over the EFFECTIVE (post-config) values;
/// a config-sourced offender gets the `--no-config` hint.
pub(crate) fn check_server_face_flags(
    face: &str,
    host: &HostConfig,
    settings: &tole_cli::config_apply::Settings,
    explicit_mcp: bool,
) -> Result<()> {
    // `serve` and `acp` host sessions that load skills (#945), so the owner's
    // `--skill` / `--no-skills` apply there; `mcp` and `mission` still refuse.
    let hosts_skills = matches!(face, "serve" | "acp");
    // A skill file named by the PROJECT config sits inside the served
    // repository and its bytes are not bound by the config trust: a later
    // `git pull` would change what loads into every new session. Refused;
    // the flag, TOLE_SKILL, a profile and the user config are the owner's own.
    if hosts_skills
        && settings
            .skill
            .as_ref()
            .is_some_and(|s| s.source == tole_cli::config_apply::Source::Config)
    {
        return Err(refuse_for(
            &format!(
                "--skill from the project config is not supported by `tole {face}` (a skill \
                 file the repository names is not content-bound; pass --skill or TOLE_SKILL, \
                 or set it in the user config)"
            ),
            Some(tole_cli::config_apply::Source::Config),
            true,
        ));
    }
    let (skills, no_skills) = if hosts_skills {
        (&[][..], false)
    } else {
        (&host.skills[..], host.no_skills)
    };
    check_client_session_flags(face, skills, no_skills, explicit_mcp).map_err(|e| {
        // Same order as the checks inside: skill, no_skills, mcp_server.
        let cfg = if !skills.is_empty() {
            from_config(&settings.skill)
        } else if no_skills {
            config_layer(settings.no_skills.source)
        } else {
            from_config(&settings.mcp_server)
        };
        // An exported TOLE_SKILL reaches every face: name it, or the
        // refusal would point at a --skill flag nobody typed.
        let from_env = !skills.is_empty()
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
        // `mission` is attended: it keeps the `--no-config` way out.
        refuse_for(&e.to_string(), cfg, face != "mission")
    })
}
