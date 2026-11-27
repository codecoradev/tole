//! Image routes (#558): upload one image into a live session, read one back.

use super::State;
use serde_json::json;
use std::io::Write;
use tole_cli::serve_support::transport::Conn;
use tole_cli::session_host::lock_sessions;

pub(super) fn image_error(e: tole_cli::serve_images::ImageApiError) -> (u16, serde_json::Value) {
    (e.status(), json!({"error": e.message()}))
}

/// The JSONL file of a live session, or the 404 answer.
fn live_session_path(
    state: &State,
    id: &str,
) -> Result<std::path::PathBuf, (u16, serde_json::Value)> {
    let sessions = lock_sessions(&state.sessions);
    sessions
        .get(id)
        .map(|st| st.path.clone())
        .ok_or_else(|| (404, json!({"error": "unknown session"})))
}

/// `POST /sessions/{id}/images` (#558): store the raw body as an image of
/// a live session; answers the reference (`id`, `mime`, `bytes`, `fnv1a64`).
pub(super) fn upload_image(state: &State, id: &str, body: &[u8]) -> (u16, serde_json::Value) {
    if let Err(resp) = live_session_path(state, id) {
        return resp;
    }
    let Some(store) = state.uploads.as_ref() else {
        return (
            503,
            json!({"error": "image uploads need a data directory (set CODECORA_HOME or HOME)"}),
        );
    };
    match store.store(id, body) {
        Ok(r) => (200, tole_cli::serve_images::upload_json(&r)),
        Err(e) => image_error(e),
    }
}

/// `GET /sessions/{id}/images/{fnv1a64}` (#558): only an image the session's
/// log records, re-checked against the jail, its size and its hash.
pub(super) fn read_image(
    state: &State,
    id: &str,
    hash: &str,
) -> Result<(String, Vec<u8>), (u16, serde_json::Value)> {
    let path = live_session_path(state, id)?;
    tole_cli::serve_images::read_recorded_in_jail(&path, hash).map_err(image_error)
}

/// An image body with a fixed type: the recorded mime, never sniffed by
/// the client (`nosniff`), never cached by a shared cache.
pub(super) fn respond_image(stream: &mut Conn, mime: &str, data: &[u8]) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: private, no-store\r\nConnection: close\r\n\r\n",
        data.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(data);
    let _ = stream.flush();
}
