//! Discussions over serve (#1042): `GET /discussions`, `GET /discussions/{id}`
//! and `POST /discussions/estimate`.
//!
//! A thin face over the discussion library, the one `tole discuss` uses
//! (`tole_cli::discuss`): the list and the show are the audit view
//! (`audit_discussion`, never a raw contribution, every string redacted), and
//! the estimate is the dry run of `tole discuss start --dry-run`. This module
//! only turns a request into the library's `StartArgs` and a library error
//! into a status.
//!
//! The request carries what the owner types at the terminal and nothing else:
//! `question`, `criteria`, `participants`, `budget_tokens`, `max_rounds`,
//! `files` and `id`. Participants are the profiles of the OWNER's user config
//! (re-read per request); background files are named inside the serve
//! workspace root, confined and read by the library like `--file`.

use serde_json::{json, Value};

use super::State;
use tole_cli::discuss::{self, Env, StartArgs};

/// The only keys a discussion request may carry.
const KEYS: [&str; 7] = [
    "question",
    "criteria",
    "participants",
    "budget_tokens",
    "max_rounds",
    "files",
    "id",
];

/// A library error as an answer: an unknown discussion is a 404, anything else
/// the library refuses (a contract rule, a participant, a file) is the
/// client's to fix.
fn refused(e: &str) -> (u16, Value) {
    // The library names the ledger directory; the client needs not.
    if e.starts_with("no discussion") {
        return (404, json!({"error": "no such discussion"}));
    }
    (400, json!({"error": e}))
}

fn strings(req: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(v) = req.get(key) else {
        return Ok(Vec::new());
    };
    v.as_array()
        .and_then(|a| a.iter().map(|s| s.as_str().map(str::to_string)).collect())
        .ok_or_else(|| format!("{key} must be an array of strings"))
}

/// The request body as the owner's `StartArgs`. A key outside [`KEYS`] is
/// refused by name, never ignored, so a field that would widen something (a
/// tool list, a profile, an allow pattern) cannot be sent by mistake.
pub(super) fn parse(body: &str) -> Result<StartArgs, (u16, Value)> {
    let bad = |m: String| (400, json!({"error": m}));
    let req: Value = serde_json::from_str(body).map_err(|_| bad("invalid JSON body".into()))?;
    let obj = req
        .as_object()
        .ok_or_else(|| bad("the body must be a JSON object".into()))?;
    if let Some(k) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(bad(format!(
            "unknown field {k:?}: a discussion request takes only {}",
            KEYS.join(", ")
        )));
    }
    let question = req
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("question must be a string".into()))?
        .to_string();
    let budget_tokens = req
        .get("budget_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("budget_tokens must be a positive integer".into()))?;
    let max_rounds = match req.get("max_rounds") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| bad("max_rounds must be a positive integer".into()))?,
        ),
    };
    let id = match req.get("id") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| bad("id must be a string".into()))?
                .to_string(),
        ),
    };
    Ok(StartArgs {
        id,
        question,
        criteria: strings(&req, "criteria").map_err(bad)?,
        participants: strings(&req, "participants").map_err(bad)?,
        budget_tokens,
        max_rounds,
        files: strings(&req, "files").map_err(bad)?,
    })
}

/// Runs `f` with the library's environment: the discussion ledger directory,
/// the owner's user config (the same file that defines the profiles), the
/// process secrets and the serve workspace root as the folder background files
/// are named against. The provider is never called here (`dry`).
pub(super) fn with_env<T>(
    state: &State,
    f: impl FnOnce(&Env<'_>) -> Result<T, String>,
) -> Result<T, (u16, Value)> {
    let dir = discuss::dir().map_err(|e| (503, json!({"error": e})))?;
    let (cfg, path) = match state.profiles.load_config() {
        Ok(Some((cfg, path))) => (cfg, Some(path)),
        Ok(None) => (tole_cli::config::Config::default(), None),
        Err(e) => return Err((e.status(), json!({"error": e.message()}))),
    };
    let base = path.and_then(std::path::Path::parent);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let secrets = tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
    let make = tole_cli::discussion_host::factory(
        tole_core::openai::OpenAiConfig::new("http://127.0.0.1:1/v1", "dry", "dry"),
        secrets.clone(),
        tole_cli::config_apply::discussion_timeout(),
    );
    let env = Env {
        dir: &dir,
        cfg: &cfg,
        paths: (base, home.as_deref()),
        secrets,
        make,
        workdir: &state.workspace_root,
    };
    f(&env).map_err(|e| refused(&env.secrets.redact(&e)))
}

/// `GET /discussions`: the audit list, without the server's directory path.
pub(super) fn list(state: &State) -> (u16, Value) {
    match with_env(state, |env| {
        discuss::audit::discussions_value(env.dir, None, &env.secrets)
    }) {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                o.remove("dir");
            }
            (200, v)
        }
        Err(e) => e,
    }
}

/// `GET /discussions/{id}`: rounds, the gate's report, disagreements, the
/// claim-checker's checklist and the tokens used against the budget.
pub(super) fn show(state: &State, id: &str) -> (u16, Value) {
    if !tole_core::discussion::id_ok(id) {
        return (400, json!({"error": "invalid discussion id"}));
    }
    match with_env(state, |env| {
        discuss::audit::discussions_value(env.dir, Some(id), &env.secrets)
    }) {
        Ok(v) => (200, v),
        Err(e) => e,
    }
}

/// `POST /discussions/estimate`: `tole discuss start --dry-run`. Validates the
/// contract, the participants and the background files and says what it may
/// cost; writes nothing and calls no model.
pub(super) fn estimate(state: &State, body: &str) -> (u16, Value) {
    let args = match parse(body) {
        Ok(a) => a,
        Err(e) => return e,
    };
    match with_env(state, |env| discuss::estimate(&args, env)) {
        Ok(v) => (200, v),
        Err(e) => e,
    }
}
