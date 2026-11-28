//! `GET /info` (#1024): version, protocol and feature names, and the scope of the caller (#1320);
//! no path, user or setting.

use super::State;
use serde_json::{json, Value};
use tole_cli::serve_support::device_tokens::Scope;

/// One stable name per route group of `docs/mobile-control.md`; never renamed.
const FEATURES: &str =
    "approvals compact cron delegation.approvals detach discussions discussions.events effort \
    entries events.sse fork hooks images interrupt missions models profiles resume session.name \
    sessions.index skills tools tree";

pub(super) fn route(state: &State, scope: Scope) -> (u16, Value) {
    let mut features: Vec<&str> = FEATURES.split_whitespace().collect();
    features.extend(state.auth.basic_enabled().then_some("auth.basic"));
    features.extend(cfg!(feature = "mcp").then_some("mcp"));
    features.sort_unstable();
    let version = env!("CARGO_PKG_VERSION");
    let protocol = tole_cli::protocol::SERVE_PROTOCOL_VERSION;
    (
        200,
        json!({
            "version": version,
            "protocolVersion": protocol,
            "features": features,
            "scope": scope.as_str(),
        }),
    )
}
