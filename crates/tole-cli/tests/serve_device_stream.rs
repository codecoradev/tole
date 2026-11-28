//! Device tokens (#1320) and the event streams of `tole serve`: revoking a token ends the streams
//! it opened (sessions and discussions), a shared-token stream is not touched by it, and a
//! read-only token cannot take the slots the others need.

#![cfg(feature = "shell-tools")]

use std::time::{Duration, Instant};
use tole_cli::serve_support::device_tokens::Scope;
use tole_core::discussion::Contract;
use tole_core::discussion_log::DiscussionLog;
use tole_core::secrets::SecretSet;

mod common;
mod serve_harness;
use serve_harness::raw::{device_token, store_of};
use serve_harness::{home_of, new_session, prompt, spawn_mock, Frame, Serve, Sse};

const WITHIN: Duration = Duration::from_secs(30);
/// What the server may take to notice a revoke: a pass of the stream loop is 150 ms.
const ENDS_WITHIN: Duration = Duration::from_secs(3);

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn open(serve: &Serve, path: &str, token: Option<&str>) -> Result<Sse, (u16, serde_json::Value)> {
    let header = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    Sse::open(serve, path, &header, token.is_none())
}

/// Every frame up to the end of the stream; fails when it is still open after `bound`.
fn until_it_ends(sse: &Sse, bound: Duration) -> Vec<Frame> {
    let start = Instant::now();
    let mut got = Vec::new();
    loop {
        match sse.next(bound.saturating_sub(start.elapsed())) {
            Some(f) => got.push(f),
            None => {
                assert!(
                    start.elapsed() < bound,
                    "still open after a revoke: {got:?}"
                );
                return got;
            }
        }
    }
}

fn is_entry(f: &Frame, after: u64) -> bool {
    f.event == "entry" && f.id.is_some_and(|s| s > after)
}

#[test]
fn revoking_a_token_ends_its_session_stream_and_not_the_shared_tokens() {
    let serve = Serve::spawn(&spawn_mock(0));
    let ro = device_token(&serve, "watch", Scope::ReadOnly);
    let id = new_session(&serve);
    let path = format!("/sessions/{id}/events");
    let watching = open(&serve, &path, Some(&ro)).unwrap();
    let owner = open(&serve, &path, None).unwrap();
    let ready = watching.until(WITHIN, |f| f.event == "ready");
    let after = ready.last().unwrap().data["lastSeq"].as_u64().unwrap();
    owner.until(WITHIN, |f| f.event == "ready");

    store_of(&serve).revoke("watch", now()).unwrap();
    prompt(&serve, &id, "hello after the revoke");

    // The revoked stream ends, with no frame of what happened after the revoke.
    let rest = until_it_ends(&watching, ENDS_WITHIN);
    assert!(!rest.iter().any(|f| is_entry(f, after)), "{rest:?}");
    // The shared token's stream carries on and sees the new entries.
    let seen = owner.until(WITHIN, |f| is_entry(f, after));
    assert!(seen.iter().any(|f| is_entry(f, after)));
    // The reconnect of the revoked token is a 401.
    assert_eq!(open(&serve, &path, Some(&ro)).err().unwrap().0, 401);
}

/// A stream is tied to ITS token: revoking one of two read-only tokens ends only that one's stream,
/// and a token made again under the same name is another token, so the old one's stream still ends.
#[test]
fn a_stream_follows_its_own_token_and_not_its_name_or_the_others() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let path = format!("/sessions/{id}/events");
    let (a, b) = (
        device_token(&serve, "a", Scope::ReadOnly),
        device_token(&serve, "b", Scope::ReadOnly),
    );
    let (sa, sb) = (
        open(&serve, &path, Some(&a)).unwrap(),
        open(&serve, &path, Some(&b)).unwrap(),
    );
    sa.until(WITHIN, |f| f.event == "ready");
    sb.until(WITHIN, |f| f.event == "ready");

    store_of(&serve).revoke("a", now()).unwrap();
    until_it_ends(&sa, ENDS_WITHIN);
    // B's stream is still open a few passes later, and still gets what happens next.
    assert!(sb
        .next(Duration::from_secs(1))
        .is_none_or(|f| f.event != "closed"));
    prompt(&serve, &id, "after a was revoked");
    sb.until(WITHIN, |f| f.event == "entry");

    // `b` revoked and made again: the new secret works, the old one's stream ends.
    store_of(&serve).revoke("b", now()).unwrap();
    let again = device_token(&serve, "b", Scope::ReadOnly);
    until_it_ends(&sb, ENDS_WITHIN);
    assert!(open(&serve, &path, Some(&again)).is_ok());
    assert_eq!(open(&serve, &path, Some(&b)).err().unwrap().0, 401);
}

fn started(ledger: &std::path::Path, id: &str) -> DiscussionLog {
    let l = DiscussionLog::at(ledger, id).unwrap();
    let names = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (criteria, who) = (names(&["a choice is named"]), names(&["coder", "ops"]));
    let c = Contract::new(
        id,
        "which one",
        &criteria,
        &who,
        100_000,
        None,
        &SecretSet::default(),
    );
    l.start(c.unwrap(), 1).unwrap();
    l
}

#[test]
fn revoking_a_token_ends_its_discussion_stream() {
    let dir = serve_harness::workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let ledger = home_of(&dir).join("tole/discussions");
    std::fs::create_dir_all(&ledger).unwrap();
    let log = started(&ledger, "open-one");
    let serve = Serve::spawn_in(&spawn_mock(0), dir, true);
    let ro = device_token(&serve, "watch", Scope::ReadOnly);
    let sse = open(&serve, "/discussions/open-one/events", Some(&ro)).unwrap();
    sse.until(WITHIN, |f| f.event == "ready");

    store_of(&serve).revoke("watch", now()).unwrap();
    log.reserve(1, "participant-coder", 500, 2).unwrap();

    let rest = until_it_ends(&sse, ENDS_WITHIN);
    assert!(!rest.iter().any(|f| f.event == "changed"), "{rest:?}");
}

#[test]
fn a_read_only_token_cannot_take_the_last_stream_slots() {
    let serve = Serve::spawn(&spawn_mock(0));
    let ro = device_token(&serve, "watch", Scope::ReadOnly);
    let full = device_token(&serve, "phone", Scope::Full);
    let id = new_session(&serve);
    let path = format!("/sessions/{id}/events");
    // Eight streams are open at most; a read-only token holds six.
    let held: Vec<Sse> = (0..6)
        .map(|_| open(&serve, &path, Some(&ro)).unwrap())
        .collect();
    let refused = open(&serve, &path, Some(&ro)).err().unwrap();
    assert_eq!(refused.0, 429, "{refused:?}");
    // The owner still gets in, with the shared token and with a full device token.
    let a = open(&serve, &path, None).unwrap();
    let b = open(&serve, &path, Some(&full)).unwrap();
    assert!(open(&serve, &path, Some(&full)).is_err(), "eight in all");
    drop((held, a, b));
}

/// Revoking while a long replay is under way ends it: the reader does not read until the token is
/// revoked, so the server is blocked on a full socket in the middle of the replay, and what comes
/// out afterwards is a fraction of the transcript.
#[test]
fn revoking_a_token_ends_a_replay_that_is_under_way() {
    use std::io::{Read, Write};
    let serve = Serve::spawn(&spawn_mock(0));
    let ro = device_token(&serve, "watch", Scope::ReadOnly);
    let id = new_session(&serve);
    prompt(&serve, &id, "one real turn");
    // A transcript of 4000 entries of 16 KiB (64 MiB) after the real turn: one entry per line,
    // numbered on from the highest `seq` of the file (a line is a record or an array of them).
    let file = serve.session_file(&id);
    let text = std::fs::read_to_string(&file).unwrap();
    let max = |v: &serde_json::Value| v["seq"].as_u64().unwrap_or(0);
    let mut seq = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .flat_map(|v| v.as_array().cloned().unwrap_or_else(|| vec![v]))
        .map(|v| max(&v))
        .max()
        .unwrap();
    let pad = "x".repeat(16 * 1024);
    let mut out = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap(),
    );
    const MADE: u64 = 4000;
    for _ in 0..MADE {
        seq += 1;
        let e = serde_json::json!({"kind": "entry", "seq": seq, "id": format!("e_{seq}"),
            "type": "message", "timestamp": 1_795_000_000_000u64,
            "payload": {"role": "user", "text": pad}});
        writeln!(out, "{e}").unwrap();
    }
    out.flush().unwrap();
    drop(out);

    let mut s = std::net::TcpStream::connect(("127.0.0.1", serve.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    write!(
        s,
        "GET /sessions/{id}/events HTTP/1.1\r\nAuthorization: Bearer {ro}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    // Not read yet: the server fills the socket and waits in the middle of the replay.
    std::thread::sleep(Duration::from_secs(3));
    store_of(&serve).revoke("watch", now()).unwrap();
    let mut got = Vec::new();
    let _ = s.read_to_end(&mut got);
    let entries = String::from_utf8_lossy(&got)
        .matches("event: entry")
        .count() as u64;
    assert!(
        entries < MADE / 2,
        "{entries} of {MADE} entries were delivered after the revoke"
    );
}
