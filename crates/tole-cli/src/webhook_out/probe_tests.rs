use super::*;
use crate::webhook_out::send::Keys;
use crate::webhook_out::{Destination, KeySource};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use tole_core::web::AddrPolicy;

const KEY: &[u8] = b"tole-test-key-0123456789abcdef";
const NOW: i64 = 1_795_000_000;

struct Seen {
    url: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

struct Fake {
    result: RefCell<Option<Result<Reply, PostError>>>,
    seen: RefCell<Vec<Seen>>,
}

impl Fake {
    fn answering(r: Result<Reply, PostError>) -> Fake {
        Fake {
            result: RefCell::new(Some(r)),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Poster for Fake {
    fn post(&self, req: &Request<'_>) -> Result<Reply, PostError> {
        self.seen.borrow_mut().push(Seen {
            url: req.url.into(),
            body: req.body.to_vec(),
            headers: req.headers.to_vec(),
        });
        self.result.borrow_mut().take().expect("one request only")
    }
}

struct FixedKey(Option<&'static [u8]>);

impl Keys for FixedKey {
    fn key(&self, _: &Destination) -> Result<Vec<u8>, &'static str> {
        self.0.map(<[u8]>::to_vec).ok_or("key_unavailable")
    }
}

fn dests() -> Destinations {
    Destinations(BTreeMap::from([(
        "gateway".to_string(),
        Destination {
            key: KeySource::Env("K".into()),
            url: "https://hooks.example.com/tole?token=abc".into(),
            // Not listing any event: a test event does not depend on `events`.
            events: BTreeSet::new(),
            policy: AddrPolicy::default(),
            allow_agent_jobs: false,
            timeout: Duration::from_secs(10),
        },
    )]))
}

fn ok(status: u16) -> Result<Reply, PostError> {
    Ok(Reply {
        status,
        retry_after: None,
    })
}

fn header<'a>(h: &'a [(String, String)], k: &str) -> &'a str {
    h.iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
        .unwrap()
}

#[test]
fn a_test_event_is_signed_like_any_other_and_names_its_destination() {
    let fake = Fake::answering(ok(204));
    let p = probe(&dests(), "gateway", NOW, "n1", &fake, &FixedKey(Some(KEY))).unwrap();
    assert_eq!(p.outcome, Outcome::Status(204));
    assert!(p.outcome.ok());
    assert!(tole_core::webhook::valid_delivery(&p.id), "{}", p.id);
    assert!(p.id.starts_with("test:") && p.id.len() == "test:".len() + 16);

    let seen = fake.seen.borrow();
    let q = &seen[0];
    assert_eq!(q.url, "https://hooks.example.com/tole?token=abc");
    let body: serde_json::Value = serde_json::from_slice(&q.body).unwrap();
    assert_eq!(body["v"], 1);
    assert_eq!(body["type"], "webhook_test");
    assert_eq!(body["destination"], "gateway");
    assert_eq!(body["id"], p.id);
    assert_eq!(body["at"], NOW);
    // The same native v2 signature as every event, over exactly these bytes.
    assert_eq!(header(&q.headers, "X-Tole-Event"), "webhook_test");
    assert_eq!(header(&q.headers, "X-Tole-Delivery"), p.id);
    assert_eq!(
        header(&q.headers, "X-Tole-Signature"),
        tole_core::webhook::sign(KEY, &NOW.to_string(), &p.id, Some("webhook_test"), &q.body)
    );
}

#[test]
fn each_nonce_gives_its_own_id() {
    assert_ne!(test_event_id("a"), test_event_id("b"));
    assert_eq!(test_event_id("a"), test_event_id("a"));
}

#[test]
fn the_result_is_a_status_or_a_closed_code() {
    for (res, want) in [
        (ok(500), Outcome::Status(500)),
        (ok(403), Outcome::Status(403)),
        (Err(PostError::Timeout), Outcome::Failed("timeout")),
        (Err(PostError::Dns), Outcome::Failed("dns")),
        (
            Err(PostError::Transport("io:ConnectionRefused".into())),
            Outcome::Failed("transport"),
        ),
        (
            Err(PostError::RefusedAddress),
            Outcome::Failed("refused_address"),
        ),
        (
            Err(PostError::Redirect(302)),
            Outcome::Failed("redirect_refused"),
        ),
        (
            Err(PostError::SchemeRefused),
            Outcome::Failed("bad_destination"),
        ),
    ] {
        let fake = Fake::answering(res);
        let p = probe(&dests(), "gateway", NOW, "n", &fake, &FixedKey(Some(KEY))).unwrap();
        assert_eq!(p.outcome, want);
        assert!(!p.outcome.ok() || matches!(p.outcome, Outcome::Status(s) if s < 300));
    }
}

#[test]
fn an_unknown_destination_or_a_missing_key_sends_nothing() {
    let fake = Fake::answering(ok(200));
    assert_eq!(
        probe(&dests(), "nope", NOW, "n", &fake, &FixedKey(Some(KEY))).unwrap_err(),
        ProbeError::NoSuchDestination
    );
    assert_eq!(
        probe(&dests(), "gateway", NOW, "n", &fake, &FixedKey(None)).unwrap_err(),
        ProbeError::KeyUnavailable
    );
    assert!(fake.seen.borrow().is_empty(), "nothing went out");
}

#[test]
fn the_test_event_is_not_a_configurable_event() {
    // It cannot be listed in `events`, and an outbox line cannot name it.
    assert!(!Event::ALL.contains(&Event::WebhookTest));
    assert_eq!(Event::parse("webhook_test"), None);
}

#[test]
fn nothing_secret_is_in_the_body_or_the_outcome() {
    let fake = Fake::answering(ok(200));
    let p = probe(&dests(), "gateway", NOW, "n", &fake, &FixedKey(Some(KEY))).unwrap();
    let seen = fake.seen.borrow();
    let body = String::from_utf8_lossy(&seen[0].body).to_string();
    let key = std::str::from_utf8(KEY).unwrap();
    assert!(!body.contains(key) && !format!("{p:?}").contains(key));
    assert!(!body.contains("token=abc") && !format!("{p:?}").contains("token=abc"));
}
