//! Session routes that write a log register the TUI writes too (#947): fork, tree,
//! compact and name, and the idle-session guard they and the model routes share.

use super::live_sessions::live;
use super::State;
use serde_json::json;
use tole_core::storage::Storage;

/// `POST /sessions/{id}/fork {"at"?: <seq>}` (#947): fork the session at a
/// settled entry into a new session file, through the same
/// `JsonlStorage::fork` the TUI `/fork` and `tole branch` call. `at` is the
/// `seq` of an entry as `GET .../entries` shows it (default: the last
/// settled entry). Writes only the new file and, as the TUI does, one
/// index line; the source is never touched. The new session is not made
/// live: `POST /sessions {"resume": "<new id>"}` opens it.
///
/// 404 unknown session, 409 while a turn runs (the turn holds the storage
/// lock; a fork waits for none), 400 for a bad or unsettled `at`.
pub(super) fn fork_route(state: &State, id: &str, body: &str) -> (u16, serde_json::Value) {
    let req = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(v) if v.is_object() => v,
            _ => return (400, json!({"error": "invalid JSON body"})),
        }
    };
    let at_seq = match req.get("at") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => match v.as_u64() {
            Some(n) => Some(n),
            None => return (400, json!({"error": "at must be an entry seq (integer)"})),
        },
    };
    let st = match live(state, id) {
        Ok(st) => st,
        Err(unknown) => return unknown,
    };
    if *st.busy.lock().expect("busy lock") {
        return (409, json!({"error": "session is busy: a turn is running"}));
    }
    // The turn lock, held across the whole fork as the TUI holds it, so no
    // turn appends to the source while it is read. A turn that took it
    // between the check above and here is the same 409.
    let Ok(guard) = st.storage.try_lock() else {
        return (409, json!({"error": "session is busy: a turn is running"}));
    };
    let at_id = match at_seq {
        None => None,
        Some(n) => match guard.entries().iter().find(|e| e.seq == n) {
            Some(e) => Some(e.id.clone()),
            None => return (400, json!({"error": format!("no entry with seq {n}")})),
        },
    };
    let Some(dir) = st.path.parent().map(|d| d.to_path_buf()) else {
        return (500, json!({"error": "session file has no directory"}));
    };
    let new_id = tole_cli::session_host::new_session_id("s");
    let forked =
        match tole_core::storage::JsonlStorage::fork(&st.path, &dir, &new_id, at_id.as_deref()) {
            Ok(f) => f,
            Err(tole_core::storage::StorageError::Invalid(m)) => {
                return (400, json!({"error": m}));
            }
            Err(e) => return (500, json!({"error": format!("forking {id}: {e}")})),
        };
    let cut = forked
        .forked_from()
        .map(|o| o.entry.clone())
        .unwrap_or_default();
    let cut_seq = guard.entries().iter().find(|e| e.id == cut).map(|e| e.seq);
    drop(forked);
    drop(guard);
    tole_cli::session_index::record(&dir, &new_id, "branch");
    eprintln!("tole: session/{id} forked into {new_id} (serve)");
    (
        200,
        json!({"id": new_id, "forkedFrom": id, "at": cut, "seq": cut_seq}),
    )
}

/// `GET /sessions/{id}/tree` (#947): the session's family, from the same
/// reader as `tole sessions --tree` and the TUI `/tree` (its parent chain
/// up to the root and every fork below it), read from the session files
/// so it works while a turn runs. Rows are depth-first, `depth` relative
/// to the root. 404 for an unknown session.
pub(super) fn tree_route(state: &State, id: &str) -> (u16, serde_json::Value) {
    let path = match live(state, id) {
        Ok(st) => st.path,
        Err(unknown) => return unknown,
    };
    let Some(dir) = path.parent() else {
        return (500, json!({"error": "session file has no directory"}));
    };
    let rows: Vec<_> =
        tole_cli::session_index::family(tole_cli::session_index::tree_nodes(dir), id)
            .into_iter()
            .map(|(depth, n)| {
                json!({
                    "id": n.id,
                    "parent": n.parent,
                    "at": n.at,
                    "state": n.pc,
                    "turns": n.turns,
                    "depth": depth,
                    "current": n.id == id,
                })
            })
            .collect();
    (200, json!({"id": id, "tree": rows}))
}

/// Run `write` on session `id`'s storage under the turn lock, for the routes
/// that write a `fact/*` register the TUI writes too (#947). 404 for an
/// unknown session, 409 while a turn runs (the busy flag, then the turn
/// lock itself: a turn that took it in between is the same 409).
pub(super) fn with_idle_session(
    state: &State,
    id: &str,
    write: impl FnOnce(&mut tole_core::storage::JsonlStorage) -> Result<(), (u16, String)>,
) -> Result<(), (u16, serde_json::Value)> {
    let st = live(state, id)?;
    let busy = || (409, json!({"error": "session is busy: a turn is running"}));
    if *st.busy.lock().expect("busy lock") {
        return Err(busy());
    }
    let Ok(mut guard) = st.storage.try_lock() else {
        return Err(busy());
    };
    write(&mut guard).map_err(|(status, e)| (status, json!({"error": e})))?;
    // A register changed: keep the last-known status snapshot current (#573).
    st.refresh_status(&*guard);
    Ok(())
}

/// `POST /sessions/{id}/compact` (#947): the TUI `/compact`, i.e.
/// `session_settings::request_compaction`. It records the forced pass of
/// #481 and the session's NEXT prompt compacts first (when compaction is
/// enabled); it is not a mid-turn compaction and runs no model call here.
pub(super) fn compact_route(state: &State, id: &str) -> (u16, serde_json::Value) {
    match with_idle_session(state, id, |s| {
        tole_cli::session_settings::request_compaction(s).map_err(|e| (500, e))
    }) {
        Ok(()) => {
            eprintln!("tole: session/{id} compaction requested (serve)");
            (200, json!({"id": id, "compaction": "requested"}))
        }
        Err(e) => e,
    }
}

/// `POST /sessions/{id}/name {"name": "<label>"}` (#947): the TUI `/name`,
/// i.e. `session_settings::set_name` (the `fact/session_name` register).
pub(super) fn name_route(state: &State, id: &str, body: &str) -> (u16, serde_json::Value) {
    let Ok(req) = serde_json::from_str::<serde_json::Value>(body) else {
        return (400, json!({"error": "invalid JSON body"}));
    };
    let Some(name) = req.get("name").and_then(|v| v.as_str()) else {
        return (400, json!({"error": "missing name"}));
    };
    // The same label rule as the TUI, checked before the session is looked
    // at so a bad name is a 400 whatever the session's state.
    let name = match tole_cli::session_settings::clean_label("name", name) {
        Ok(n) => n,
        Err(e) => return (400, json!({"error": e})),
    };
    match with_idle_session(state, id, |s| {
        tole_cli::session_settings::set_name(s, &name).map_err(|e| (500, e))
    }) {
        Ok(()) => (200, json!({"id": id, "name": name})),
        Err(e) => e,
    }
}
