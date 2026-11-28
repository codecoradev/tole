//! `GET /discussions/{id}/events` (#1094): follow one discussion to its close
//! without polling `GET /discussions/{id}`.
//!
//! A discussion is not a session, so this is its own Server-Sent Events
//! stream, one per discussion, fed by the discussion's ledger and nothing
//! else. It carries a small signal and never the ledger's lines: a frame says
//! that the ledger moved, to which `seq`, and where the discussion stands, and
//! the client answers it with `GET /discussions/{id}` (the audit view, redacted
//! as it is there). So no contribution, summary or closing text can reach the
//! stream, not even redacted, and every frame still goes through the process
//! secret set, the one of that route, before it is written.
//!
//! - `changed` (durable, `id: <seq>`): `{seq, round, step, tokens_used,
//!   closed}`. `seq` is the position of the last ledger line, `step` what the
//!   ledger says must happen next (`contribute`, `summarize`, `check`,
//!   `decide`, `finish`, `done`). Several ledger steps between two looks of the
//!   server coalesce into one frame: the signal is "the ledger is now at
//!   `seq`", so nothing is lost. A client that connects with `after=<seq>` (or
//!   `Last-Event-ID`) gets a frame at once when the ledger is past it, and
//!   none when it is not, so a dropped connection resumes without missing or
//!   repeating one.
//! - `ready`: the first look at the ledger has been sent, `busy` is whether
//!   this process drives the discussion.
//! - `state` (ephemeral): `running` or `interrupted` flipped (a failed call, a
//!   resume), which the ledger does not record.
//! - `closed`: the discussion is closed; the stream ends after it.

use super::events::{after_of, write_all, Follower};
use super::request::Request;
use super::response::respond;
use super::State;
use serde_json::{json, Value};
use std::io::{ErrorKind, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tole_cli::events::{frame, heartbeat_interval, Heartbeat, HEARTBEAT, HEARTBEAT_ENV};
use tole_cli::serve_support::device_tokens::Scope;
use tole_cli::serve_support::shutdown;
use tole_cli::serve_support::transport::Conn;
use tole_core::discussion_log::{DiscussionLog, Next, State as Ledger};

/// How often the ledger file is looked at (one `stat` when nothing changed).
const POLL: Duration = Duration::from_millis(150);

/// Reads of the ledger that may fail one after the other (about 3 s) before
/// the stream ends.
const MAX_READ_FAILURES: u32 = 20;

/// The `changed` data for `st`: numbers, a flag and one word of a closed set.
fn changed(st: &Ledger) -> Value {
    let (step, round) = match st.next() {
        Next::Contribute { round, .. } => ("contribute", round),
        Next::Summarize { round, .. } => ("summarize", round),
        Next::Check { round } => ("check", round),
        Next::Decide { round } => ("decide", round),
        Next::Finish { .. } => ("finish", st.current_round()),
        Next::NeedContract | Next::Done => ("done", st.current_round()),
    };
    json!({"seq": st.last_seq, "round": round, "step": step,
           "tokens_used": st.tokens_used, "closed": st.closed.is_some()})
}

pub(super) fn serve(
    mut stream: Conn,
    state: &Arc<State>,
    id: &str,
    query: &str,
    req: &Request,
    scope: Scope,
) {
    let (who, last_event_id) = (
        Follower::new(state, scope, req),
        req.last_event_id.as_deref(),
    );
    let after = match after_of(query, last_event_id) {
        Ok(n) => n,
        Err((status, payload)) => return respond(&mut stream, status, &payload),
    };
    if !tole_core::discussion::id_ok(id) {
        return respond(&mut stream, 400, &json!({"error": "invalid discussion id"}));
    }
    let dir = match tole_cli::discuss::dir() {
        Ok(d) => d,
        Err(e) => return respond(&mut stream, 503, &json!({"error": e})),
    };
    let log = DiscussionLog::at(&dir, id).expect("the id was checked");
    let file = dir.join(format!("{id}.jsonl"));
    let secrets = tole_cli::provider_chain::process_secrets(&tole_cli::config_apply::real_env);
    // The slot first: a refused stream parses nothing, so the cap bounds the
    // number of ledger reads (up to 16 MB each) a client can cause.
    let Some(_slot) = who.slot(state) else {
        let busy = json!({"error": "too many open event streams"});
        return respond(&mut stream, 429, &busy);
    };
    // A first look before anything is written: an unknown discussion is a 404
    // like the show route's, an unreadable one says nothing of its path. Its
    // length is taken before the read, and the first pass of the loop uses
    // this state instead of reading again.
    let first_len = std::fs::symlink_metadata(&file).map_or(0, |m| m.len());
    let first = match log.state() {
        Ok(st) if st.contract.is_some() => st,
        Ok(_) => return respond(&mut stream, 404, &json!({"error": "no such discussion"})),
        Err(_) => return respond(&mut stream, 500, &json!({"error": "ledger unreadable"})),
    };
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n";
    if !write_all(&mut stream, head) {
        return;
    }
    // Every frame passes the secret set, as the show route's answer does.
    let send = |stream: &mut Conn, event: &str, seq: Option<u64>, data: Value| {
        write_all(stream, &frame(event, seq, &secrets.redact_value(&data)))
    };
    let mut hb = Heartbeat::new(
        heartbeat_interval(std::env::var(HEARTBEAT_ENV).ok().as_deref()),
        Instant::now(),
    );
    // `view`: what this process says of the discussion (`None` until `ready`).
    // `failed`: reads of the ledger that failed in a row.
    let (mut sent, mut seen_len, mut view, mut failed) = (after, Some(first_len), None, 0);
    let mut pending = Some(first);
    // The wait of each pass is a read of the peer: end of file means it is gone,
    // which a quiet discussion would otherwise find only at a heartbeat.
    let _ = stream.set_read_timeout(Some(POLL));
    loop {
        if !who.valid(state) {
            return;
        }
        // Asked BEFORE the ledger is read: a thread that ends after its last
        // line was written is seen as ended only with that line in the read.
        let driven = state.discussions.state(id, false);
        // One `stat` when the file did not change. The length is taken BEFORE
        // the read, so a write in between shows as a change on the next pass,
        // and it is kept only once a read succeeded.
        let len = std::fs::symlink_metadata(&file).map_or(0, |m| m.len());
        let look = match pending.take() {
            Some(st) => Some(st),
            None if seen_len != Some(len) => match log.state() {
                Ok(st) => {
                    seen_len = Some(len);
                    Some(st)
                }
                Err(_) => {
                    failed += 1;
                    None
                }
            },
            None => None,
        };
        // A ledger that stays unreadable (replaced, garbled, deleted) does not
        // leave a silent stream open: it ends, and the client's reconnect gets
        // the 404 or 500 of the first look.
        if failed >= MAX_READ_FAILURES {
            return;
        }
        if let Some(st) = look {
            failed = 0;
            let closed = st.closed.is_some();
            if st.last_seq > sent {
                sent = st.last_seq;
                if !send(&mut stream, "changed", Some(sent), changed(&st)) {
                    return;
                }
                hb.note_write(Instant::now());
            }
            if view.is_none() {
                let data = json!({"id": id, "after": after, "lastSeq": st.last_seq,
                                  "busy": driven == "running"});
                if !send(&mut stream, "ready", None, data) {
                    return;
                }
                view = Some(driven);
                hb.note_write(Instant::now());
            }
            if closed {
                send(&mut stream, "closed", None, json!({"id": id}));
                return;
            }
        }
        // `running` / `interrupted` is only this process's to know.
        if view.is_some_and(|v| v != driven) {
            if !send(&mut stream, "state", None, json!({"state": driven})) {
                return;
            }
            view = Some(driven);
            hb.note_write(Instant::now());
        }
        // Shutting down (#1014): end the stream; the client reconnects with
        // `after` to the next serve.
        if shutdown::requested() {
            return;
        }
        if hb.due(Instant::now()) {
            if !write_all(&mut stream, HEARTBEAT) {
                return;
            }
            hb.note_write(Instant::now());
        }
        match stream.read(&mut [0u8; 64]) {
            Ok(0) => return,
            Ok(_) => std::thread::sleep(POLL),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return,
        }
    }
}
