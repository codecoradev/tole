//! Discussions over serve (#1042): `GET /discussions`, `GET /discussions/{id}`,
//! `POST /discussions/estimate`, `POST /discussions`,
//! `POST /discussions/{id}/resume` and `POST /discussions/{id}/stop`.
//!
//! A thin face over the discussion library, the one `tole discuss` uses
//! (`tole_cli::discuss`): the list and the show are the audit view
//! (`audit_discussion`, never a raw contribution, every string redacted), the
//! estimate is the dry run of `tole discuss start --dry-run`, and a start or a
//! resume is `discuss::begin` / `discuss::prepare_resume` driven on a thread by
//! `discuss::owner::Owner`. This module only turns a request into the library's
//! `StartArgs` and a library error into a status.
//!
//! Starting from here is the owner's action under the server's auth, at depth
//! 0, with the contract, participant, budget and file rules of `tole discuss
//! start`. A discussion a model starts with the `discuss` tool is not started
//! from here: it stays an approval. A `--plan-mode` server starts nothing
//! (a discussion spends the owner's budget and writes a ledger).
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
    // A state the request cannot change: an id that is taken, a discussion
    // that is closed or already running.
    if e.contains("already has a contract")
        || e.contains("already running")
        || e.contains("is closed")
    {
        return (409, json!({"error": e}));
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
        // Helper tools are the owner's `--tool` on the command line (#1039).
        // A `tools` field is refused as unknown by the KEYS check above; this
        // stays empty.
        tools: Vec::new(),
    })
}

/// Runs `f` with the library's environment: the discussion ledger directory,
/// the owner's user config (the same file that defines the profiles), the
/// process secrets and the serve workspace root as the folder background files
/// are named against. With `dry` the provider is never called and none is
/// needed (the estimate); otherwise it is the server's own provider
/// configuration, the one `tole discuss start` uses.
pub(super) fn with_env<T>(
    state: &State,
    dry: bool,
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
    let provider = if dry {
        tole_core::openai::OpenAiConfig::new("http://127.0.0.1:1/v1", "dry", "dry")
    } else {
        tole_cli::turn_runner::provider_config().map_err(|e| (503, json!({"error": e})))?
    };
    let make = tole_cli::discussion_host::factory(
        provider,
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
        // No host to start helper children here: a discussion that allows
        // tools is refused over serve, never run without them (#1039).
        delegator: None,
    };
    f(&env).map_err(|e| refused(&env.secrets.redact(&e)))
}

/// `GET /discussions`: the audit list, without the server's directory path.
pub(super) fn list(state: &State) -> (u16, Value) {
    match with_env(state, true, |env| {
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
    match with_env(state, true, |env| {
        discuss::audit::discussions_value(env.dir, Some(id), &env.secrets)
    }) {
        Ok(mut v) => {
            // `running` only this process knows; the ledger says the rest.
            let closed = v["outcome"].is_object();
            v["state"] = json!(state.discussions.state(id, closed));
            (200, v)
        }
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
    match with_env(state, true, |env| discuss::estimate(&args, env)) {
        Ok(v) => (200, v),
        Err(e) => e,
    }
}

/// A server that may not write (plan mode) starts and continues nothing.
fn writable(state: &State) -> Result<(), (u16, Value)> {
    if state.plan_mode {
        return Err((
            409,
            json!({"error": "this server runs in plan mode; a discussion spends budget and writes a ledger"}),
        ));
    }
    Ok(())
}

/// `POST /discussions`: `tole discuss start`, returning at once with the id;
/// read it with `GET /discussions/{id}`. `202`, or the refusals of the
/// estimate, plus `409` for an id that is taken. Any number may run at once.
pub(super) fn start(state: &State, body: &str) -> (u16, Value) {
    if let Err(e) = writable(state) {
        return e;
    }
    let args = match parse(body) {
        Ok(a) => a,
        Err(e) => return e,
    };
    match with_env(state, false, |env| state.discussions.start(&args, env)) {
        Ok(v) => (202, v),
        Err(e) => e,
    }
}

/// `POST /discussions/{id}/resume`: `tole discuss resume` for one that was
/// interrupted (a crash, a failed call). `202`; `409` when it is closed or
/// already running. Takes no body: there is nothing to change.
pub(super) fn resume(state: &State, id: &str) -> (u16, Value) {
    if !tole_core::discussion::id_ok(id) {
        return (400, json!({"error": "invalid discussion id"}));
    }
    if let Err(e) = writable(state) {
        return e;
    }
    match with_env(state, false, |env| state.discussions.resume(id, env)) {
        Ok(v) => (202, v),
        Err(e) => e,
    }
}

/// `POST /discussions/{id}/stop`: the ledger's own escalation; the
/// discussion closes with the usual report. Repeating it is harmless.
pub(super) fn stop(state: &State, id: &str) -> (u16, Value) {
    if !tole_core::discussion::id_ok(id) {
        return (400, json!({"error": "invalid discussion id"}));
    }
    // A stop only ends spending, so it is allowed on every server.
    match with_env(state, true, |env| state.discussions.stop(id, env)) {
        Ok(v) => (200, v),
        Err(e) => e,
    }
}
