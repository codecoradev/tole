//! `GET /models`, `POST /sessions/{id}/model` and `POST /sessions/{id}/effort`
//! (#574): the model picker and the thinking level of a remote client.
//!
//! Choosing a model chooses what the token holder spends, and the choice is a
//! durable session register (it survives `resume`), so the OWNER decides which
//! models may be picked, never the request:
//!
//! - the allowed list is the owner's `TOLE_MODELS` (the same variable that
//!   advertises the ACP model picker, comma separated) plus the configured
//!   default model, which a client may always switch back to. It is read once
//!   at start-up. With no `TOLE_MODELS` the default is the only model, so a
//!   client can pick nothing else;
//! - the request carries a model id only. It cannot name a provider URL or a
//!   key: the session keeps using the server's own provider configuration, and
//!   an id outside the list is a `400`;
//! - the writers are the TUI's own (`session_settings::write_model` and
//!   `set_effort`), so the registers are exactly the ones `/model` and
//!   `/thinking` write, and the same refusals apply while a turn runs.

use super::{with_idle_session, State};
use serde_json::{json, Value};

/// What the owner lets a client pick.
#[derive(Clone, Debug, Default)]
pub struct ModelCatalog {
    /// `TOLE_MODELS`, deduplicated, in order.
    allow: Vec<String>,
    /// The configured default model (the provider configuration's).
    default: Option<String>,
}

impl ModelCatalog {
    /// Build from the owner's environment and provider configuration.
    pub fn from_env() -> Self {
        let default = tole_cli::turn_runner::provider_config()
            .ok()
            .map(|c| c.model)
            .filter(|m| !m.trim().is_empty());
        Self::new(std::env::var("TOLE_MODELS").ok().as_deref(), default)
    }

    pub fn new(tole_models: Option<&str>, default: Option<String>) -> Self {
        let mut allow: Vec<String> = Vec::new();
        for id in tole_models.unwrap_or("").split(',') {
            let id = id.trim();
            // An id the session registers would refuse is never offered.
            if tole_cli::session_settings::clean_label("model id", id).is_ok()
                && !allow.iter().any(|m| m == id)
            {
                allow.push(id.to_string());
            }
        }
        Self { allow, default }
    }

    /// The models a client may pick: the default first, then the allow list.
    fn list(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in self.default.iter().chain(self.allow.iter()) {
            if !out.contains(m) {
                out.push(m.clone());
            }
        }
        out
    }
}

/// `GET /models`: `{"models": [...], "default": "<id>" | null}`.
pub fn list_route(state: &State) -> (u16, Value) {
    (
        200,
        json!({"models": state.models.list(), "default": state.models.default}),
    )
}

/// `POST /sessions/{id}/model {"model": "<id>"}`.
pub fn model_route(state: &State, id: &str, body: &str) -> (u16, Value) {
    let Ok(req) = serde_json::from_str::<Value>(body) else {
        return (400, json!({"error": "invalid JSON body"}));
    };
    let Some(model) = req.get("model").and_then(Value::as_str) else {
        return (400, json!({"error": "missing model"}));
    };
    // The same label rule as the TUI, then the owner's list.
    let model = match tole_cli::session_settings::clean_label("model id", model) {
        Ok(m) => m,
        Err(e) => return (400, json!({"error": e})),
    };
    if !state.models.list().contains(&model) {
        return (
            400,
            json!({"error": format!("model {model:?} is not allowed: pick one from GET /models")}),
        );
    }
    match with_idle_session(state, id, |s| {
        tole_cli::session_settings::write_model(s, &model)
            .map_err(|e| (500, format!("storage error: {e}")))
    }) {
        Ok(()) => (200, json!({"id": id, "model": model})),
        Err(e) => e,
    }
}

/// `POST /sessions/{id}/effort {"effort": "minimal"|"low"|"medium"|"high"|null}`.
/// `null` (or `"unset"`, the TUI's word) clears the override.
pub fn effort_route(state: &State, id: &str, body: &str) -> (u16, Value) {
    let Ok(req) = serde_json::from_str::<Value>(body) else {
        return (400, json!({"error": "invalid JSON body"}));
    };
    let level: Option<String> = match req.get("effort") {
        None => {
            return (
                400,
                json!({"error": "missing effort (a level, or null to clear)"}),
            )
        }
        Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("unset") => None,
        Some(Value::String(s)) => match tole_core::openai::parse_reasoning_effort(s) {
            Ok(l) => Some(l),
            Err(e) => return (400, json!({"error": e})),
        },
        Some(_) => return (400, json!({"error": "effort must be a string or null"})),
    };
    match with_idle_session(state, id, |s| {
        tole_cli::session_settings::set_effort(s, level.as_deref()).map_err(|e| (500, e))
    }) {
        Ok(()) => (200, json!({"id": id, "effort": level})),
        Err(e) => e,
    }
}
