//! The credential checks of `tole serve` that need nothing private (#742): constant-time
//! comparison, bearer matching, PBKDF2 password hashes, Basic and the shared check. Moved from
//! the unit tests of `serve_support/auth.rs` (#1320) with the test names unchanged.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use tole_cli::auth::*;

fn basic_header(user: &str, pass: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{pass}")))
}

fn basic_auth(token: Option<&str>) -> Authenticator {
    let hash = PasswordHash::create(b"correct horse", MIN_ITERATIONS).unwrap();
    let creds = BasicCredentials::new("owner", PasswordHash::parse(&hash).unwrap()).unwrap();
    Authenticator::new(token.map(str::to_string), Some(creds))
}

#[test]
fn ct_eq_matches_only_equal_slices() {
    assert!(ct_eq(b"secret", b"secret"));
    assert!(!ct_eq(b"secret", b"secreT"));
    assert!(!ct_eq(b"secret", b"secre"));
    assert!(ct_eq(b"", b""));
}

#[test]
fn bearer_matching_is_case_insensitive_and_exact() {
    assert!(bearer_matches(Some("Bearer t0ken"), "t0ken"));
    assert!(bearer_matches(Some("bearer  t0ken "), "t0ken"));
    assert!(bearer_matches(Some("BEARER t0ken"), "t0ken"));
    assert!(!bearer_matches(Some("Bearer t0kex"), "t0ken"));
    assert!(!bearer_matches(Some("Bearer t0ken2"), "t0ken"));
    assert!(!bearer_matches(Some("Basic t0ken"), "t0ken"));
    assert!(!bearer_matches(Some("t0ken"), "t0ken"));
    assert!(!bearer_matches(None, "t0ken"));
}

#[test]
fn a_hash_round_trips_and_a_wrong_password_fails() {
    let h = PasswordHash::create(b"correct horse", MIN_ITERATIONS).unwrap();
    assert!(h.starts_with("$pbkdf2-sha256$i=10000$"), "{h}");
    let p = PasswordHash::parse(&h).unwrap();
    assert!(p.verify(b"correct horse"));
    assert!(!p.verify(b"correct horsE"));
    assert!(!p.verify(b""));
    // Two hashes of one password differ (random salt).
    assert_ne!(
        h,
        PasswordHash::create(b"correct horse", MIN_ITERATIONS).unwrap()
    );
}

#[test]
fn weak_or_malformed_hashes_and_passwords_are_refused_without_echo() {
    assert!(PasswordHash::create(b"short", MIN_ITERATIONS).is_err());
    // Seven characters in fourteen bytes is still too short; eight is fine.
    assert!(PasswordHash::create("ééééééé".as_bytes(), MIN_ITERATIONS).is_err());
    assert!(PasswordHash::create("éééééééé".as_bytes(), MIN_ITERATIONS).is_ok());
    assert!(PasswordHash::create(b"long enough pw", MIN_ITERATIONS - 1).is_err());
    let h = PasswordHash::create(b"correct horse", MIN_ITERATIONS).unwrap();
    let weak = h.replacen("i=10000", "i=9999", 1);
    let e = PasswordHash::parse(&weak).err().unwrap();
    assert!(e.contains("too few") && !e.contains(&weak), "{e}");
    let huge = h.replacen("i=10000", "i=99999999", 1);
    assert!(PasswordHash::parse(&huge)
        .err()
        .unwrap()
        .contains("too many"));
    for bad in [
        "",
        "plaintext-password",
        "$pbkdf2-sha1$i=10000$AAAAAAAAAAAAAAAAAAAAAA$AAAA",
        "$pbkdf2-sha256$i=10000$AAAA$AAAA",
        &format!("{h}$extra"),
    ] {
        let e = PasswordHash::parse(bad).err().unwrap();
        assert!(e.contains("malformed"), "{bad}: {e}");
        assert!(
            bad.is_empty() || !e.contains(bad),
            "the error echoed the input"
        );
    }
}

#[test]
fn basic_accepts_only_the_right_user_and_password() {
    let a = basic_auth(None);
    assert!(a.check(Some(&basic_header("owner", "correct horse"))));
    assert!(!a.check(Some(&basic_header("owner", "wrong password"))));
    assert!(!a.check(Some(&basic_header("intruder", "correct horse"))));
    assert!(!a.check(Some(&basic_header("Owner", "correct horse"))));
    assert!(!a.check(Some(&basic_header("owner", ""))));
    assert!(!a.check(Some(&basic_header("", ""))));
    assert!(!a.check(Some("Basic !!!not-base64")));
    assert!(!a.check(Some(&format!("Basic {}", STANDARD.encode("owner")))));
    assert!(!a.check(Some("Basic")));
    assert!(!a.check(Some("Bearer correct horse")));
    assert!(!a.check(None));
    // The scheme is case-insensitive; a password may hold ':'.
    let header = basic_header("owner", "correct horse").replacen("Basic", "bAsIc", 1);
    assert!(a.check(Some(&header)));
    // An oversized credential is refused before any hashing.
    assert!(!a.check(Some(&basic_header("owner", &"x".repeat(2000)))));
}

#[test]
fn the_token_and_basic_go_through_the_same_check() {
    let a = basic_auth(Some("t0ken-0742"));
    assert!(a.check(Some("Bearer t0ken-0742")));
    assert!(a.check(Some(&basic_header("owner", "correct horse"))));
    assert!(!a.check(Some("Bearer t0ken-0743")));
    // Token only: Basic never passes, and there is no challenge.
    let t = Authenticator::token("t0ken-0742".into());
    assert!(t.check(Some("Bearer t0ken-0742")));
    assert!(!t.check(Some(&basic_header("owner", "correct horse"))));
    assert!(t.challenge().is_none());
    assert!(a.challenge().unwrap().starts_with("Basic realm="));
    // Basic without a token: a bearer never passes.
    assert!(!basic_auth(None).check(Some("Bearer t0ken-0742")));
}

#[test]
fn user_names_and_loopback_binds_are_checked() {
    let h = || {
        PasswordHash::parse(&PasswordHash::create(b"correct horse", MIN_ITERATIONS).unwrap())
            .unwrap()
    };
    assert!(BasicCredentials::new("", h()).is_err());
    assert!(BasicCredentials::new("a:b", h()).is_err());
    assert!(BasicCredentials::new("a\nb", h()).is_err());
    assert!(BasicCredentials::new(&"u".repeat(129), h()).is_err());
    assert!(BasicCredentials::new(" owner ", h()).is_ok());
    for b in ["127.0.0.1", "localhost", "::1", "[::1]", "127.1.2.3"] {
        assert!(is_loopback_bind(b), "{b}");
    }
    for b in ["0.0.0.0", "192.168.1.5", "::", "example.com", ""] {
        assert!(!is_loopback_bind(b), "{b}");
    }
}
