//! #908: the memo inside `SecretSet::redact` must be invisible. Every result,
//! on a miss and on a hit, is byte-identical to `redact_uncached` (the
//! reference behaviour) over a large corpus of ordinary, adversarial and
//! secret-bearing text, and a changed allowlist or secret set can never
//! return a stale result. The allowlist is process-wide, so every test here
//! that installs one holds `ALLOWLIST` for its whole run.

use super::{shape, SecretSet};
use std::cell::RefCell;
use std::sync::Mutex;

fn fixture() -> String {
    j(&["g", "hp", "_", &"0".repeat(34), "FX"])
}

/// The allowlist regex that keeps [`fixture`].
fn allow() -> String {
    j(&["^g", "hp_", "0{20}"])
}

static ALLOWLIST: Mutex<()> = Mutex::new(());

thread_local! {
    static BEFORE_STORE: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Called by `redact` between computing a result and storing it, so a test
/// can interleave an allowlist install at exactly that point.
pub(super) fn before_store() {
    // One-shot: taken out before it runs, so it may itself call `redact`.
    let hook = BEFORE_STORE.with(|h| h.borrow_mut().take());
    if let Some(f) = hook {
        f();
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Fixtures that look like credentials are built at run time from short
/// pieces, so no secret-shaped literal exists in the source (a scanner reads
/// the source, not the bytes the test builds).
fn j(parts: &[&str]) -> String {
    parts.concat()
}

fn secret() -> String {
    j(&["s3cr3t-", "Value-", "01234567", "89abcdef"])
}

fn zx() -> String {
    j(&["Zx9Qw3Er", "5Ty7Ui1O", "p2As4Df6", "Gh8Jk0Lm"])
}

fn fragments() -> Vec<String> {
    let secret = secret();
    let gh = j(&[
        "g", "hp", "_", "aB3dE5gH", "7jK9mN1p", "Q3rS5tU7", "vW9xY1zA", "3bC5",
    ]);
    let fixture = fixture();
    let aws = j(&["AK", "IA", "IOSFODNN", "7EXAMPLQ"]);
    let jwt = j(&[
        "ey",
        "J",
        "hbGciOiJ",
        "IUzI1NiJ9",
        ".",
        "ey",
        "J",
        "zdWIiOiIx",
        "MjM0NTY3",
        "ODkwIn0",
        ".",
        "abc_",
        "def-",
        "123",
    ]);
    let pem = j(&[
        "-----BE",
        "GIN ",
        "RSA ",
        "PRIV",
        "ATE KEY-----\n",
        "MIIEow",
        "IBAAKC",
        "AQEA7bq9",
        "\nQ2xk3Zp0aB\n",
        "-----E",
        "ND ",
        "RSA ",
        "PRIV",
        "ATE KEY-----\n",
    ]);
    let b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&secret)
    };
    let mut v: Vec<String> = vec![
        gh,
        fixture,
        aws,
        jwt,
        pem,
        format!("API_KEY={}", zx()),
        j(&["pass", "word: ", "hunter2h", "unter2hu", "nter2"]),
        j(&["https://", "bob:", "p4ssw0rd", "XYZ@example.com/x"]),
        j(&[
            "Authorization: Be",
            "arer ",
            "abcDEF01",
            "23456789",
            "abcdef01",
            "23",
        ]),
        j(&["curl -u ", "admin:", "Sup3rS3c", "retPw https://h"]),
        j(&["--pass", "word=", "Sup3rS3c", "ret99 "]),
        format!("token {secret} end"),
        format!("b64 {b64}"),
        format!("split {}\u{200b}{}", &secret[..8], &secret[8..]),
        format!("API_KEY=«{}»{}", "NAME", zx()),
        "«NAME» and «OTHER_1» markers, «unterminated".into(),
        "qzk0x qzkk12x qzkkk".into(),
        "x:".repeat(300),
        "x=".repeat(200),
        "naïve café 日本語 🔐 émoji \u{202e}rtl\u{ad}".into(),
        "key: value, timeout: 30, retries = 5, fn run(cfg: &Config) -> Result<()> {}".into(),
        "\n".repeat(50),
        "A".repeat(400),
        j(&["BEGIN -----BE", "GIN PRIV", "ATE KEY----- no end"]),
        j(&["sk", "-ant-", "api03-"]) + &"aB3_-".repeat(12),
    ];
    v.push(v[0].clone().replace('_', ""));
    v
}

fn build(rng: &mut Rng, frags: &[String]) -> String {
    let mut s = String::new();
    let target = 1024 + rng.below(30_000);
    while s.len() < target {
        s.push_str(&frags[rng.below(frags.len())]);
        s.push_str(["\n", " ", "", "\t", "\"", ": "][rng.below(6)]);
    }
    // Cut at a random char boundary and glue the head to a rotated tail, so
    // secrets are split across arbitrary points.
    let mut cut = rng.below(s.len());
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    let (a, b) = s.split_at(cut);
    if rng.below(3) == 0 {
        format!("{b}{a}")
    } else {
        s
    }
}

fn set() -> SecretSet {
    SecretSet::collect(std::iter::empty(), [("MY_TOKEN".to_string(), secret())])
}

fn assert_same(s: &SecretSet, t: &str) {
    let want = s.redact_uncached(t).into_owned();
    let miss = s.redact(t).into_owned();
    let hit = s.redact(t).into_owned();
    assert_eq!(miss, want, "miss differs from reference");
    assert_eq!(hit, want, "hit differs from reference");
}

#[test]
fn the_cache_is_byte_identical_and_never_stale() {
    let secret = secret();
    let _guard = ALLOWLIST.lock().unwrap_or_else(|e| e.into_inner());
    let frags = fragments();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let with = set();
    let empty = SecretSet::default();
    let mut texts: Vec<String> = (0..400).map(|_| build(&mut rng, &frags)).collect();
    // Adversarial: pure spam and a long run, above the cache threshold.
    texts.push("x:".repeat(200_000));
    texts.push("a".repeat(500_000));
    texts.push("«".repeat(5_000));
    for t in &texts {
        assert_same(&with, t);
        assert_same(&empty, t);
    }
    // Interleaved repeats: the hits stay right after unrelated inserts.
    for t in texts.iter().rev() {
        assert_same(&with, t);
    }

    // The secret set changes: a new set has its own cache, so a text cached
    // under the empty set is redacted again under the set that knows it.
    let t = format!("{} {secret} {}", "pad ".repeat(400), "pad ".repeat(10));
    let cached_empty = empty.redact(&t).into_owned();
    assert!(cached_empty.contains(&secret));
    let known = with.redact(&t).into_owned();
    assert!(
        !known.contains(&secret),
        "known value must be masked: {known}"
    );
    assert_eq!(empty.redact(&t).into_owned(), cached_empty);

    // The allowlist changes mid-session: nothing cached before survives.
    let fixture = fixture();
    let t = format!("{} fixture {fixture} end", "pad ".repeat(400));
    shape::set_allowlist(&[]).unwrap();
    assert!(!empty.redact(&t).contains(&fixture));
    assert!(!empty.redact(&t).contains(&fixture)); // now a hit
    shape::set_allowlist(&[allow()]).unwrap();
    assert!(
        empty.redact(&t).contains(&fixture),
        "allowlist applies at once"
    );
    for t in texts.iter().take(50) {
        assert_same(&with, t);
    }
    shape::set_allowlist(&[]).unwrap();
    assert!(
        !empty.redact(&t).contains(&fixture),
        "clearing applies at once"
    );
    for t in texts.iter().take(50) {
        assert_same(&with, t);
    }
}

fn padded(tail: &str) -> String {
    format!("{} {tail}", "pad ".repeat(400))
}

#[test]
fn a_clone_shares_the_cache_and_with_never_does() {
    let secret = secret();
    let _guard = ALLOWLIST.lock().unwrap_or_else(|e| e.into_inner());
    shape::set_allowlist(&[]).unwrap();
    let a = set();
    let b = a.clone();
    let t = padded(&format!("token {secret} end"));
    let want = a.redact_uncached(&t).into_owned();
    assert_eq!(a.redact(&t).into_owned(), want);
    let len = |s: &SecretSet| s.inner.cache.lock().unwrap().map.len();
    assert_eq!(len(&a), 1);
    assert_eq!(len(&b), 1, "a clone shares the cache");
    assert_eq!(b.redact(&t).into_owned(), want);
    assert_eq!(len(&a), 1, "a hit through the clone adds nothing");

    // `with` builds a different set: its own cache, and it must mask what
    // the parent does not know, whatever the parent has cached.
    let extra = j(&["Another-", "Secret-", "Value-", "98765432", "10"]);
    let extra = extra.as_str();
    let t2 = padded(&format!("x {extra} y"));
    let parent_out = a.redact(&t2).into_owned();
    assert!(parent_out.contains(extra), "the parent does not know it");
    let child = a.with("EXTRA", extra);
    assert_eq!(len(&child), 0, "a derived set starts empty");
    let child_out = child.redact(&t2).into_owned();
    assert!(
        !child_out.contains(extra),
        "the child masks it: {child_out}"
    );
    assert_eq!(child_out, child.redact_uncached(&t2).into_owned());
    // The parent still answers as before.
    assert_eq!(a.redact(&t2).into_owned(), parent_out);
}

#[test]
fn concurrent_redaction_while_the_allowlist_toggles_ends_equal_to_the_reference() {
    let _guard = ALLOWLIST.lock().unwrap_or_else(|e| e.into_inner());
    shape::set_allowlist(&[]).unwrap();
    let s = set();
    let frags = fragments();
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    let mut texts: Vec<String> = (0..60).map(|_| build(&mut rng, &frags)).collect();
    texts.push(padded(&format!("fixture {} end", fixture())));
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|sc| {
        for k in 0..4 {
            let (s, texts, stop) = (&s, &texts, &stop);
            sc.spawn(move || {
                let mut i = k;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = s.redact(&texts[i % texts.len()]);
                    i += 7;
                }
            });
        }
        for round in 0..200 {
            let list: &[String] = if round % 2 == 0 { &[] } else { &[allow()] };
            shape::set_allowlist(list).unwrap();
            std::thread::yield_now();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    // Final policy: the last round (199) installed the allowlist.
    for t in &texts {
        let want = s.redact_uncached(t).into_owned();
        assert_eq!(s.redact(t).into_owned(), want, "stale after the toggling");
        assert_eq!(s.redact(t).into_owned(), want);
    }
    shape::set_allowlist(&[]).unwrap();
    for t in &texts {
        assert_eq!(s.redact(t).into_owned(), s.redact_uncached(t).into_owned());
    }
}

/// The interleaving the store-time generation check exists for: reader A
/// computes under the old allowlist, an allowlist is installed and reader B
/// moves the cache to the new generation, THEN A stores. Without the check
/// A's stale result would be filed under the new generation and served.
#[test]
fn a_result_computed_before_an_allowlist_install_is_not_stored_after_it() {
    let _guard = ALLOWLIST.lock().unwrap_or_else(|e| e.into_inner());
    shape::set_allowlist(&[]).unwrap();
    let s = SecretSet::default();
    let t = padded(&format!("fixture {} end", fixture()));
    let other = padded("unrelated text for reader B");
    BEFORE_STORE.with(|h| {
        let (s2, other2) = (s.clone(), other.clone());
        *h.borrow_mut() = Some(Box::new(move || {
            shape::set_allowlist(&[allow()]).unwrap();
            let _ = s2.redact(&other2); // reader B syncs the cache to the new generation
        }));
    });
    let a_out = s.redact(&t).into_owned(); // computed under the OLD allowlist
    assert!(!a_out.contains(&fixture()), "A saw the old allowlist");
    let now = s.redact(&t).into_owned();
    assert!(
        now.contains(&fixture()),
        "the new allowlist applies; A's stale result was not served"
    );
    shape::set_allowlist(&[]).unwrap();
}

#[test]
fn the_byte_count_does_not_drift_when_a_key_is_stored_again() {
    let _guard = ALLOWLIST.lock().unwrap_or_else(|e| e.into_inner());
    shape::set_allowlist(&[]).unwrap();
    let s = SecretSet::default();
    let t = padded("plain");
    // Two readers miss on the same key and both store it: the second insert
    // replaces the first and must not be counted again.
    BEFORE_STORE.with(|h| {
        let (s2, t2) = (s.clone(), t.clone());
        *h.borrow_mut() = Some(Box::new(move || {
            let _ = s2.redact(&t2);
        }));
    });
    s.redact(&t);
    assert_eq!(s.inner.cache.lock().unwrap().map.len(), 1);
    let c = s.inner.cache.lock().unwrap();
    let expect: usize = c
        .map
        .iter()
        .map(|(k, v)| k.len() + v.as_ref().map_or(0, |v| v.len()))
        .sum();
    assert_eq!(c.bytes, expect);
}
