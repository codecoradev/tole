//! Read-only views for the desktop's Helpers and Skills and tools pages
//! (#1042): `GET /profiles`, `GET /skills` and `GET /tools`.
//!
//! Each answers from the owner's own files through a library function and
//! changes nothing. They are GET-only on purpose: writing a profile, approving
//! or revoking a skill or a connected tool, and changing the allow list stay
//! with the owner at a terminal (`docs/faces.md`, "Owner actions that are not
//! serve routes"), so no verb on these paths does anything else than read.

use serde_json::{json, Value};

use super::State;

fn secrets() -> tole_core::secrets::SecretSet {
    tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env)
}

/// `GET /profiles`: the profiles of the owner's user config as display rows.
pub(super) fn profiles(state: &State) -> (u16, Value) {
    match state.profiles.list(&secrets()) {
        Ok(rows) => (200, json!({"profiles": rows})),
        Err(e) => (e.status(), json!({"error": e.message()})),
    }
}

/// `GET /skills`: every skill of the serve workspace with its approval state,
/// the drafts waiting for review and the archived skills.
pub(super) fn skills(state: &State) -> (u16, Value) {
    let ws = state.workspace_root.as_path();
    (
        200,
        tole_cli::skill::state::state_json(Some(ws), &secrets()),
    )
}

/// `GET /tools`: the owner's connected (MCP) servers with their trust state
/// from the plugin lock. Starts no server.
#[cfg(feature = "mcp")]
pub(super) fn tools(state: &State) -> (u16, Value) {
    let specs = tole_cli::session::owner_mcp::specs();
    match tole_cli::plugin::view::state_json(specs, state.workspace_root.clone()) {
        Ok(v) => (200, v),
        Err(e) => (500, json!({"error": e})),
    }
}

#[cfg(not(feature = "mcp"))]
pub(super) fn tools(_state: &State) -> (u16, Value) {
    (200, json!({"servers": [], "notes": [], "probed": false}))
}
