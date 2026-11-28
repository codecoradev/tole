//! Per-device tokens, part 1 (#1320), through the public API of
//! `tole_cli::serve_support::device_tokens`.

mod common;
use common::TestDir;
use std::path::PathBuf;
use tole_cli::serve_support::device_tokens::*;

const TOKEN_LEN: usize = 51;
const MAX_LINE: usize = 1024;

fn sha_hex(token: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const T0: u64 = 1_795_000_000;

fn store(d: &TestDir) -> Store {
    // The store makes this directory itself (0700).
    Store::new(&d.join("tole"))
}

fn file(d: &TestDir) -> PathBuf {
    d.join("tole/serve-tokens.jsonl")
}

fn add(s: &Store, name: &str, scope: Scope) -> Added {
    s.add(name, scope, T0).unwrap()
}

#[test]
fn round_trip_names_the_owner_and_the_scope() {
    let d = TestDir::new("tole-dt-roundtrip");
    let s = store(&d);
    let a = add(&s, "phone", Scope::ReadOnly);
    let b = add(&s, "laptop", Scope::Full);
    let snap = s.load().unwrap();
    assert_eq!(
        snap.lookup(a.secret.expose()),
        Some(Principal {
            name: "phone".into(),
            scope: Scope::ReadOnly
        })
    );
    assert_eq!(
        snap.lookup(b.secret.expose()),
        Some(Principal {
            name: "laptop".into(),
            scope: Scope::Full
        })
    );
    assert_eq!(snap.entries().len(), 2);
    assert_eq!(snap.entries()[0].created_at, T0);
    assert_eq!(snap.entries()[0].last_used_at, None);
}

#[test]
fn a_token_has_256_bits_from_the_random_source_and_is_never_the_same() {
    let d = TestDir::new("tole-dt-entropy");
    let s = store(&d);
    let t: Vec<String> = (0..8)
        .map(|i| {
            add(&s, &format!("d{i}"), Scope::Full)
                .secret
                .expose()
                .to_string()
        })
        .collect();
    for x in &t {
        assert!(x.starts_with(PREFIX));
        assert_eq!(x.len(), TOKEN_LEN);
        // 43 base64url characters carry 32 bytes.
        let raw = x.strip_prefix(PREFIX).unwrap();
        assert_eq!(raw.len(), 43);
        assert!(raw
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    }
    let mut u = t.clone();
    u.sort();
    u.dedup();
    assert_eq!(u.len(), t.len());
}

#[test]
fn lookup_refuses_everything_that_is_not_an_active_token() {
    let d = TestDir::new("tole-dt-refuse");
    let s = store(&d);
    let a = add(&s, "phone", Scope::Full);
    let snap = s.load().unwrap();
    let good = a.secret.expose();
    let mut wrong = good.to_string();
    wrong.replace_range(
        wrong.len() - 1..,
        if good.ends_with('A') { "B" } else { "A" },
    );
    for bad in [
        "",
        PREFIX,
        &good[..good.len() - 1],
        &format!("{good}x"),
        &good.to_uppercase(),
        &wrong,
        "a-static-bearer-token-of-the-shared-kind-0123456789",
        &good.replacen(PREFIX, "tole_xx_", 1),
    ] {
        assert_eq!(snap.lookup(bad), None, "{bad}");
    }
    assert!(snap.lookup(good).is_some());
}

#[test]
fn revoke_is_a_new_line_and_the_old_token_stays_dead() {
    let d = TestDir::new("tole-dt-revoke");
    let s = store(&d);
    let a = add(&s, "phone", Scope::Full);
    let before = std::fs::read(file(&d)).unwrap();
    s.revoke("phone", T0 + 5).unwrap();
    let after = std::fs::read(file(&d)).unwrap();
    // Append-only: the first bytes are untouched, one line was added.
    assert!(after.starts_with(&before));
    assert_eq!(after.iter().filter(|b| **b == b'\n').count(), 2);
    let snap = s.load().unwrap();
    assert_eq!(snap.lookup(a.secret.expose()), None);
    assert_eq!(snap.entries()[0].revoked_at, Some(T0 + 5));
    // The name is free again; the new token works and the old one does not.
    let b = add(&s, "phone", Scope::ReadOnly);
    let snap = s.load().unwrap();
    assert_eq!(snap.lookup(a.secret.expose()), None);
    assert_eq!(
        snap.lookup(b.secret.expose()).unwrap().scope,
        Scope::ReadOnly
    );
    assert_eq!(snap.entries().len(), 2);
    // A second revoke of a revoked name finds nothing active.
    s.revoke("phone", T0 + 9).unwrap();
    assert!(matches!(
        s.revoke("phone", T0 + 10),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        s.revoke("nobody", T0),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn a_name_is_unique_among_active_tokens_and_validated() {
    let d = TestDir::new("tole-dt-names");
    let s = store(&d);
    add(&s, "phone", Scope::Full);
    assert!(matches!(
        s.add("phone", Scope::ReadOnly, T0),
        Err(StoreError::Duplicate(_))
    ));
    let long_ok = "a".repeat(MAX_NAME);
    for ok in ["a", "0", "phone-2", "my.laptop_1", long_ok.as_str()] {
        s.add(ok, Scope::Full, T0)
            .unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    let long_bad = "a".repeat(MAX_NAME + 1);
    for bad in [
        "",
        "Phone",
        "-x",
        ".x",
        "_x",
        "a b",
        "a/b",
        "a\nb",
        "a\u{0}b",
        "é",
        "a:b",
        "ａ",
        long_bad.as_str(),
    ] {
        assert!(
            matches!(s.add(bad, Scope::Full, T0), Err(StoreError::BadName(_))),
            "{bad:?}"
        );
    }
    // A refused add left nothing behind.
    assert_eq!(s.load().unwrap().entries().len(), 6);
}

#[test]
fn at_most_64_active_tokens() {
    let d = TestDir::new("tole-dt-many");
    let s = store(&d);
    for i in 0..MAX_ACTIVE {
        s.add(&format!("d{i}"), Scope::Full, T0).unwrap();
    }
    assert!(matches!(
        s.add("one-more", Scope::Full, T0),
        Err(StoreError::TooMany)
    ));
    s.revoke("d0", T0).unwrap();
    s.add("one-more", Scope::Full, T0).unwrap();
}

fn corrupt_line(d: &TestDir) -> Option<usize> {
    match store(d).load() {
        Err(StoreError::Corrupt { line, .. }) => Some(line),
        _ => None,
    }
}

#[test]
fn a_line_that_does_not_read_back_fails_the_whole_store() {
    let d = TestDir::new("tole-dt-corrupt");
    let s = store(&d);
    let a = add(&s, "phone", Scope::Full);
    s.revoke("phone", T0).unwrap();
    let good = std::fs::read_to_string(file(&d)).unwrap();
    let h = format!("sha256:{}", "0".repeat(64));
    let cases: Vec<(&str, String)> = vec![
        ("garbage", "this is not json\n".into()),
        ("torn tail", "{\"op\":\"revoke\",\"v\":1,\"na".into()),
        ("unknown op", "{\"op\":\"grant\",\"v\":1,\"name\":\"x\",\"at\":1}\n".into()),
        ("unknown field", "{\"op\":\"revoke\",\"v\":1,\"name\":\"x\",\"at\":1,\"y\":2}\n".into()),
        ("version", "{\"op\":\"revoke\",\"v\":2,\"name\":\"x\",\"at\":1}\n".into()),
        ("bad name", format!("{{\"op\":\"add\",\"v\":1,\"name\":\"X\",\"scope\":\"full\",\"hash\":\"{h}\",\"at\":1}}\n")),
        ("bad scope", format!("{{\"op\":\"add\",\"v\":1,\"name\":\"x\",\"scope\":\"root\",\"hash\":\"{h}\",\"at\":1}}\n")),
        ("bad hash", "{\"op\":\"add\",\"v\":1,\"name\":\"x\",\"scope\":\"full\",\"hash\":\"sha256:zz\",\"at\":1}\n".into()),
        ("too long", format!("{}\n", " ".repeat(MAX_LINE) + "{}")),
        ("not utf-8", "\u{0}\n".into()),
    ];
    for (what, bad) in cases {
        std::fs::write(file(&d), format!("{good}{bad}")).unwrap();
        assert_eq!(corrupt_line(&d), Some(3), "{what}");
        // Nothing is written on top of a store that cannot be read.
        assert!(s.add("other", Scope::Full, T0).is_err(), "{what}");
        assert!(s.revoke("phone", T0).is_err(), "{what}");
        assert_eq!(
            std::fs::read_to_string(file(&d)).unwrap(),
            format!("{good}{bad}"),
            "{what}"
        );
    }
    // A hand-written second add of one active name.
    let line = good.lines().next().unwrap();
    std::fs::write(file(&d), format!("{line}\n{line}\n")).unwrap();
    assert_eq!(corrupt_line(&d), Some(2));
    // The good file is still fine and the token is still dead.
    std::fs::write(file(&d), &good).unwrap();
    assert_eq!(s.load().unwrap().lookup(a.secret.expose()), None);
}

#[test]
fn a_corrupt_revoke_does_not_bring_the_token_back() {
    let d = TestDir::new("tole-dt-resurrect");
    let s = store(&d);
    let a = add(&s, "phone", Scope::Full);
    // The revoke line is cut in half, as a crash or an editor might.
    let mut f = std::fs::read_to_string(file(&d)).unwrap();
    f.push_str("{\"op\":\"revoke\",\"v\":1,\"name\":\"phone\"");
    std::fs::write(file(&d), f).unwrap();
    assert!(s.load().is_err());
    let live = Live::new(s);
    assert_eq!(live.lookup(a.secret.expose()), None);
    assert!(live.error().unwrap().contains("corrupt at line 2"));
}

#[test]
fn a_file_over_its_cap_is_refused_unread() {
    let d = TestDir::new("tole-dt-cap");
    let s = store(&d);
    add(&s, "phone", Scope::Full);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(file(&d))
        .unwrap();
    use std::io::Write;
    f.write_all(&vec![b'\n'; MAX_FILE_BYTES as usize]).unwrap();
    assert!(matches!(s.load(), Err(StoreError::TooLarge)));
    assert!(matches!(
        s.add("x", Scope::Full, T0),
        Err(StoreError::TooLarge)
    ));
}

#[cfg(unix)]
#[test]
fn the_file_and_its_directory_are_owner_only() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let d = TestDir::new("tole-dt-modes");
    let s = store(&d);
    add(&s, "phone", Scope::Full);
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(file(&d)), 0o600);
    assert_eq!(mode(d.join("tole/serve-tokens.lock")), 0o600);
    assert_eq!(mode(d.join("tole")), 0o700);

    // Wider than owner-only: refused, not repaired.
    for bad in [0o644, 0o640, 0o620, 0o666] {
        std::fs::set_permissions(file(&d), std::fs::Permissions::from_mode(bad)).unwrap();
        assert!(matches!(s.load(), Err(StoreError::Unsafe(_))), "{bad:o}");
        assert!(s.add("x", Scope::Full, T0).is_err());
        assert!(!Live::new(s.clone()).touch("phone", T0));
    }
    assert_eq!(mode(file(&d)), 0o666);
    std::fs::set_permissions(file(&d), std::fs::Permissions::from_mode(0o600)).unwrap();
    s.load().unwrap();

    // A directory others can write could have the file swapped.
    std::fs::set_permissions(d.join("tole"), std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(s.load(), Err(StoreError::Unsafe(_))));
    assert!(matches!(
        s.add("x", Scope::Full, T0),
        Err(StoreError::Unsafe(_))
    ));
    std::fs::set_permissions(d.join("tole"), std::fs::Permissions::from_mode(0o700)).unwrap();

    // A link in the file's place is not followed, for reading or appending.
    let target = d.join("elsewhere");
    std::fs::write(&target, "").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(file(&d)).unwrap();
    symlink(&target, file(&d)).unwrap();
    assert!(s.load().is_err());
    assert!(s.add("x", Scope::Full, T0).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"");
}

#[test]
fn no_secret_reaches_a_file_a_debug_or_an_error() {
    let d = TestDir::new("tole-dt-leak");
    let s = store(&d);
    let a = add(&s, "phone", Scope::Full);
    let token = a.secret.expose().to_string();
    let raw = token.strip_prefix(PREFIX).unwrap();
    let hash_hex = sha_hex(&token);

    // Not in the file (only its hash is), nor in the lock.
    let mut files = String::new();
    for f in std::fs::read_dir(d.join("tole")).unwrap() {
        files.push_str(&String::from_utf8_lossy(
            &std::fs::read(f.unwrap().path()).unwrap(),
        ));
    }
    assert!(files.contains(&hash_hex));
    assert!(!files.contains(&token) && !files.contains(raw));

    // Not in any Debug.
    let snap = s.load().unwrap();
    let live = Live::new(s.clone());
    let debug = format!("{a:?} {:?} {snap:?} {:?} {:?}", a.secret, snap.entries(), s);
    assert!(!debug.contains(&token) && !debug.contains(raw));
    assert!(
        !debug.contains(&hash_hex),
        "the hash stays out of Debug too"
    );
    assert!(debug.contains("<redacted>"));
    let _ = live;

    // Not in an error, whichever way the store fails.
    let errors = [
        s.add("phone", Scope::Full, T0).unwrap_err().to_string(),
        s.add("Bad", Scope::Full, T0).unwrap_err().to_string(),
        s.revoke("nobody", T0).unwrap_err().to_string(),
    ];
    std::fs::write(file(&d), format!("{token} {hash_hex}\n")).unwrap();
    let corrupt = s.load().unwrap_err().to_string();
    for e in errors.iter().chain([&corrupt]) {
        assert!(
            !e.contains(&token) && !e.contains(raw) && !e.contains(&hash_hex),
            "{e}"
        );
    }
}

#[test]
fn two_adds_of_one_name_at_once_make_exactly_one_token() {
    let d = TestDir::new("tole-dt-race");
    let s = store(&d);
    let ok: usize = (0..8)
        .map(|i| {
            let s = s.clone();
            std::thread::spawn(move || {
                s.add(
                    "shared",
                    if i % 2 == 0 {
                        Scope::Full
                    } else {
                        Scope::ReadOnly
                    },
                    T0,
                )
                .is_ok()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| usize::from(h.join().unwrap()))
        .sum();
    assert_eq!(ok, 1);
    assert_eq!(s.load().unwrap().entries().len(), 1);
}

#[test]
fn concurrent_adds_of_different_names_all_land_whole() {
    let d = TestDir::new("tole-dt-parallel");
    let s = store(&d);
    let hs: Vec<_> = (0..16)
        .map(|i| {
            let s = s.clone();
            std::thread::spawn(move || s.add(&format!("d{i}"), Scope::Full, T0).unwrap())
        })
        .collect();
    let added: Vec<Added> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let snap = s.load().unwrap();
    assert_eq!(snap.entries().len(), 16);
    for a in &added {
        assert_eq!(snap.lookup(a.secret.expose()).unwrap().name, a.name);
    }
}

#[test]
fn an_edited_file_without_a_final_newline_gets_no_glued_line() {
    let d = TestDir::new("tole-dt-newline");
    let s = store(&d);
    add(&s, "a", Scope::Full);
    let f = std::fs::read_to_string(file(&d)).unwrap();
    std::fs::write(file(&d), f.trim_end()).unwrap();
    add(&s, "b", Scope::Full);
    assert_eq!(s.load().unwrap().entries().len(), 2);
}
