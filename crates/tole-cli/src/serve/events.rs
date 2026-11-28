//! `GET /sessions/{id}/events` (#524): the Server-Sent Events stream of one session.

use super::response::respond;
use super::State;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tole_cli::serve_support::shutdown;
use tole_cli::serve_support::transport::Conn;
use tole_cli::session_host::lock_sessions;

pub(super) fn write_all(stream: &mut Conn, text: &str) -> bool {
    stream
        .write_all(text.as_bytes())
        .and_then(|()| stream.flush())
        .is_ok()
}

/// The `seq` a stream resumes after: an explicit `after=<seq>` of `query`
/// wins over the `Last-Event-ID` header, and a stream with neither starts at
/// 0. Shared by the session and the discussion stream.
pub(super) fn after_of(
    query: &str,
    last_event_id: Option<&str>,
) -> Result<u64, (u16, serde_json::Value)> {
    match super::pct::raw_param(query, "after") {
        Some(a) => a.parse().map_err(|_| {
            (
                400,
                json!({"error": "after must be a non-negative integer"}),
            )
        }),
        None => Ok(last_event_id
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)),
    }
}

/// `GET /sessions/{id}/events?after=<seq>` (#524): Server-Sent Events.
///
/// Durable `entry` frames replay from `after` (or `Last-Event-ID`) and then
/// follow the log; ephemeral frames (`text_delta`, `reasoning`,
/// `tool_started`, `tool_finished`, `approval_*`) are best-effort and not
/// replayable. See `tole_cli::events` for the model. The stream ends when
/// the client goes away (a write fails, or the heartbeat finds it dead) or
/// the session leaves the live map (`closed`).
pub(super) fn serve_events(
    mut stream: Conn,
    state: &Arc<State>,
    id: &str,
    query: &str,
    last_event_id: Option<&str>,
) {
    use tole_cli::events::{entry_frame, frame, ApprovalWatch, EntryTail, Heartbeat, HEARTBEAT};
    let after = match after_of(query, last_event_id) {
        Ok(n) => n,
        Err((status, payload)) => {
            respond(&mut stream, status, &payload);
            return;
        }
    };
    let path = {
        let sessions = lock_sessions(&state.sessions);
        sessions.get(id).map(|st| st.path.clone())
    };
    let Some(path) = path else {
        respond(&mut stream, 404, &json!({"error": "unknown session"}));
        return;
    };
    let Some(_slot) = state.streams.try_acquire() else {
        respond(
            &mut stream,
            429,
            &json!({"error": "too many open event streams"}),
        );
        return;
    };
    // Subscribe BEFORE replaying so no ephemeral frame falls in the gap.
    let sub = state.buses.subscribe(id);
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n";
    if !write_all(&mut stream, head) {
        return;
    }

    let live_busy = |state: &State| -> Option<bool> {
        let sessions = lock_sessions(&state.sessions);
        sessions
            .get(id)
            .map(|st| *st.busy.lock().expect("busy lock"))
    };
    let mut tail = EntryTail::new(path, after);
    let mut approvals = ApprovalWatch::default();
    let mut ready_sent = false;
    let mut replay_err_logged = false;
    let mut hb = Heartbeat::new(
        tole_cli::events::heartbeat_interval(
            std::env::var(tole_cli::events::HEARTBEAT_ENV)
                .ok()
                .as_deref(),
        ),
        Instant::now(),
    );
    // Approval state changes go out BEFORE the durable entries of the same
    // pass, so a decision is announced ahead of the effects it caused.
    // `Some(n)`: n frames written (0 is normal); `None`: the peer is gone.
    let send_approvals = |stream: &mut Conn, approvals: &mut ApprovalWatch| -> Option<usize> {
        let mut n = 0;
        for (name, data) in approvals.poll(&state.approvals.list(Some(id))) {
            if !write_all(stream, &frame(name, None, &data)) {
                return None;
            }
            n += 1;
        }
        Some(n)
    };
    loop {
        if ready_sent {
            match send_approvals(&mut stream, &mut approvals) {
                None => return,
                Some(0) => {}
                Some(_) => hb.note_write(Instant::now()),
            }
        }
        // Durable frames (a read error just retries on the next pass).
        let polled = match tail.poll() {
            Ok(entries) => {
                for e in entries {
                    if !write_all(&mut stream, &entry_frame(&e)) {
                        return;
                    }
                    hb.note_write(Instant::now());
                }
                true
            }
            Err(kind) => {
                // Once per stream, kind only (no path), so a recurrence is
                // diagnosable without a line every 150 ms pass.
                if !replay_err_logged {
                    replay_err_logged = true;
                    eprintln!("tole serve: event stream replay could not read the log ({kind})");
                }
                false
            }
        };
        // `ready` says the replay is complete, so it waits for a pass that
        // actually read the log: after a failed read it would announce an
        // empty replay and a client resuming from `after` would lose every
        // entry it missed (lossless replay is the point of the log).
        if !ready_sent && polled {
            ready_sent = true;
            let ready = frame(
                "ready",
                None,
                &json!({"id": id, "after": after, "lastSeq": tail.last_seq(),
                        "busy": live_busy(state).unwrap_or(false)}),
            );
            // The replay is complete; then what is pending right now.
            if !write_all(&mut stream, &ready) {
                return;
            }
            hb.note_write(Instant::now());
            if send_approvals(&mut stream, &mut approvals).is_none() {
                return;
            }
        }
        let dropped = sub.take_dropped();
        if dropped > 0 {
            if !write_all(
                &mut stream,
                &frame("resync", None, &json!({"dropped": dropped})),
            ) {
                return;
            }
            hb.note_write(Instant::now());
        }
        // Ephemeral frames: wait briefly for one (this is also the poll
        // interval for the durable tail), then drain what queued up.
        match sub.rx.recv_timeout(Duration::from_millis(150)) {
            Ok(first) => {
                let mut next = Some(first);
                while let Some(e) = next {
                    if !write_all(&mut stream, &e.to_frame()) {
                        return;
                    }
                    hb.note_write(Instant::now());
                    next = sub.rx.try_recv().ok();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if live_busy(state).is_none() {
            // Evicted from the live map: say so and end cleanly. Entries
            // written before the end have been sent (one last poll above).
            let _ = tail
                .poll()
                .map(|rest| rest.iter().all(|e| write_all(&mut stream, &entry_frame(e))));
            let _ = write_all(&mut stream, &frame("closed", None, &json!({"id": id})));
            return;
        }
        // Shutting down (#1014): end the stream once the session's turn has
        // settled, entries included, so the drain never waits on an idle viewer.
        // The client reconnects with `after` (to the next serve).
        if shutdown::requested() && live_busy(state) != Some(true) {
            let _ = tail
                .poll()
                .map(|rest| rest.iter().all(|e| write_all(&mut stream, &entry_frame(e))));
            return;
        }
        // Silence for HEARTBEAT_SECS: write a comment. This is also how a
        // dead peer is found (the write fails) and its slot freed.
        if hb.due(Instant::now()) {
            if !write_all(&mut stream, HEARTBEAT) {
                return;
            }
            hb.note_write(Instant::now());
        }
    }
}
