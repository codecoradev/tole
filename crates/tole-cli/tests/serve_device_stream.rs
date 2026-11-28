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
