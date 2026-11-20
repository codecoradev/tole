//! The connected tools' trust state as data (#1042, `tole serve`): `tole plugin
//! list --json` for a client, with the owner's server list and the plugin lock
//! it would read at a terminal.
//!
//! Read-only and it starts nothing. The context it builds has `probe: false`,
//! so no configured server is ever launched to fetch its tools (a launch is
//! how a changed manifest is found at a terminal); a server the lock already
//! refuses (unapproved, revoked, another command or program file, a changed
//! bundle manifest or sandbox profile) says so from the lock and the files,
//! and one the lock admits reads `locked (not probed: ...)`, the owner's
//! cue to run `tole plugin list` at a terminal for the live check. Approving,
//! revoking and adding a server are owner-only commands, never a route.

use std::path::PathBuf;

use serde_json::{json, Value};
use tole_core::mcp::McpServerConfig;

use super::cmd::{run, Ctx, PluginAction};

/// The one word a client switches on, from the state sentence of the list:
/// `trusted` (approved, or locked and admitted), `changed`, `not_trusted`
/// (never approved) or `removed` (revoked, or approved but no longer
/// configured).
fn trust(state: &str, revoked: bool) -> &'static str {
    if revoked || state.starts_with("revoked") || state.ends_with("(not configured)") {
        "removed"
    } else if state.starts_with("changed") {
        "changed"
    } else if state.starts_with("unapproved") || state.contains("unreadable") {
        "not_trusted"
    } else {
        "trusted"
    }
}

/// The fields of a server row a client shows; the rest of `tole plugin list
/// --json` (paths, hashes of files, the sandbox details) stays at the terminal.
const FIELDS: [&str; 10] = [
    "name",
    "state",
    "tier",
    "source",
    "version",
    "approved_at",
    "secrets",
    "sandbox",
    "manifest_hash",
    "lock",
];

/// `servers` (each with a `trust` word), `notes` from the lock and `probed`
/// (always false). `specs` are the owner's servers
/// ([`crate::session::owner_mcp`]) and `workspace` the folder whose project
/// lock applies.
pub fn state_json(specs: &[String], workspace: PathBuf) -> Result<Value, String> {
    let specs = specs
        .iter()
        .filter_map(|s| McpServerConfig::parse(s).ok())
        .collect();
    let never = |_: &McpServerConfig| Err("this view starts no server".to_string());
    let (mut ask, mut typed) = (|_: &str| false, |_: &str| String::new());
    let release = tole_core::secret_release::release();
    let mut ctx = Ctx {
        typed: &mut typed,
        probe_one: &|c, _| never(c),
        specs,
        user: tole_core::plugin_lock::Lockfile::user_path(),
        workspace,
        list: &never,
        interactive: false,
        ask: &mut ask,
        secrets: crate::provider_chain::process_secrets(&crate::config_apply::real_env),
        first_party: crate::plugin_gate::first_party_names(),
        sandbox: tole_core::sandbox::policy(),
        backend: tole_core::sandbox::detect(),
        mcp_grants: release
            .held_grants()
            .into_iter()
            .filter(|n| release.allows(n, "mcp"))
            .collect(),
        probe: false,
    };
    let out = run(PluginAction::List { json: true }, &mut ctx)?;
    let listed: Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
    let servers: Vec<Value> = listed["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let mut v = json!({
                "trust": trust(
                    row["state"].as_str().unwrap_or(""),
                    row["revoked"].as_bool().unwrap_or(false),
                ),
            });
            for k in FIELDS {
                v[k] = row[k].clone();
            }
            v
        })
        .collect();
    Ok(json!({"servers": servers, "notes": listed["notes"], "probed": false}))
}
