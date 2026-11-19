//! #908: the memo inside `SecretSet::redact` must be invisible. Every result,
//! on a miss and on a hit, is byte-identical to `redact_uncached` (the
//! reference behaviour) over a large corpus of ordinary, adversarial and
//! secret-bearing text, and a changed allowlist or secret set can never
//! return a stale result. The allowlist is process-wide, so this test lives
//! in its own binary and runs as ONE test.

use tole_core::secrets::{shape, SecretSet};

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

const SECRET: &str = "s3cr3t-Value-0123456789abcdef";

fn fragments() -> Vec<String> {
    let gh = ["ghp", "_aB3dE5gH7jK9mN1pQ3rS5tU7vW9xY1zA3bC5"].concat();
    let fixture = ["ghp", "_0000000000000000000000000000000000FX"].concat();
    let aws = ["AKIA", "IOSFODNN7EXAMPLQ"].concat();
    let jwt = [
        "eyJ",
        "hbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc_def-123",
    ]
    .concat();
    let pem = [
        "-----BEGIN ",
        "RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA7bq9\nQ2xk3Zp0aB\n-----END ",
        "RSA PRIVATE KEY-----\n",
    ]
    .concat();
    let b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(SECRET)
    };
    let mut v: Vec<String> = vec![
        gh,
        fixture,
        aws,
        jwt,
        pem,
        format!("API_KEY={}", "Zx9Qw3Er5Ty7Ui1Op2As4Df6Gh8Jk0Lm"),
        "password: hunter2hunter2hunter2".into(),
        "https://bob:p4ssw0rdXYZ@example.com/x".into(),
        "Authorization: Bearer abcDEF0123456789abcdef0123".into(),
        "curl -u admin:Sup3rS3cretPw https://h".into(),
        "--password=Sup3rS3cret99 ".into(),
        format!("token {SECRET} end"),
        format!("b64 {b64}"),
        format!("split {}\u{200b}{}", &SECRET[..8], &SECRET[8..]),
        format!("API_KEY=«{}»{}", "NAME", "Zx9Qw3Er5Ty7Ui1Op2As4Df6Gh8Jk0Lm"),
        "«NAME» and «OTHER_1» markers, «unterminated".into(),
        "qzk0x qzkk12x qzkkk".into(),
        "x:".repeat(300),
        "x=".repeat(200),
        "naïve café 日本語 🔐 émoji \u{202e}rtl\u{ad}".into(),
        "key: value, timeout: 30, retries = 5, fn run(cfg: &Config) -> Result<()> {}".into(),
        "\n".repeat(50),
        "A".repeat(400),
        "BEGIN -----BEGIN PRIVATE KEY----- no end".into(),
        "sk-ant-api03-".to_string() + &"aB3_-".repeat(12),
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
    SecretSet::collect(
        std::iter::empty(),
        [("MY_TOKEN".to_string(), SECRET.to_string())],
    )
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
    let t = format!("{} {SECRET} {}", "pad ".repeat(400), "pad ".repeat(10));
    let cached_empty = empty.redact(&t).into_owned();
    assert!(cached_empty.contains(SECRET));
    let known = with.redact(&t).into_owned();
    assert!(
        !known.contains(SECRET),
        "known value must be masked: {known}"
    );
    assert_eq!(empty.redact(&t).into_owned(), cached_empty);

    // The allowlist changes mid-session: nothing cached before survives.
    let fixture = ["ghp", "_0000000000000000000000000000000000FX"].concat();
    let t = format!("{} fixture {fixture} end", "pad ".repeat(400));
    shape::set_allowlist(&[]).unwrap();
    assert!(!empty.redact(&t).contains(&fixture));
    assert!(!empty.redact(&t).contains(&fixture)); // now a hit
    shape::set_allowlist(&["^ghp_0{20}".to_string()]).unwrap();
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
