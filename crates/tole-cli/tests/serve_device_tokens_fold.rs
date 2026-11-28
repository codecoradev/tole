//! Per-device tokens, part 1 (#1320): what the fold of the file refuses (a revoke of a
//! name that is not active, a hash added twice) and how a revoke treats a name.

mod common;
use common::TestDir;
use std::path::PathBuf;
use tole_cli::serve_support::device_tokens::*;

const T0: u64 = 1_795_000_000;

fn store(d: &TestDir) -> Store {
    Store::new(&d.join("tole"))
}

fn file(d: &TestDir) -> PathBuf {
    d.join("tole/serve-tokens.jsonl")
}

fn add(s: &Store, name: &str, scope: Scope) -> Added {
    s.add(name, scope, T0).unwrap()
}

fn first_line(d: &TestDir) -> String {
    std::fs::read_to_string(file(d))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn a_revoke_of_a_name_that_is_not_active_is_corrupt() {
    let d = TestDir::new("tole-dt-revoke-ghost");
    let s = store(&d);
    add(&s, "phone", Scope::Full);
    s.revoke("phone", T0).unwrap();
    let good = std::fs::read_to_string(file(&d)).unwrap();
    let revoke = good.lines().nth(1).unwrap();
    // A second revoke of a revoked name, and a revoke of a name never added.
    for bad in [revoke.to_string(), revoke.replace("phone", "ghost")] {
        std::fs::write(file(&d), format!("{good}{bad}\n")).unwrap();
        assert!(
            matches!(s.load(), Err(StoreError::Corrupt { line: 3, .. })),
            "{bad}"
        );
    }
    std::fs::write(file(&d), format!("{}\n", revoke.replace("phone", "ghost"))).unwrap();
    assert!(matches!(s.load(), Err(StoreError::Corrupt { line: 1, .. })));
}

#[test]
fn a_hash_that_was_added_before_is_never_added_again_revoked_or_not() {
    let d = TestDir::new("tole-dt-hash-twice");
    let s = store(&d);
    let a = add(&s, "phone", Scope::Full);
    let line = first_line(&d);
    s.revoke("phone", T0).unwrap();
    let good = std::fs::read_to_string(file(&d)).unwrap();
    // The same record again after its revoke: the name is free, the hash is not.
    std::fs::write(file(&d), format!("{good}{line}\n")).unwrap();
    assert!(matches!(
        s.load(),
        Err(StoreError::Corrupt {
            line: 3,
            why: "hash added twice"
        })
    ));
    // The same hash under another name, while the first is revoked.
    std::fs::write(
        file(&d),
        format!("{good}{}\n", line.replace("phone", "tablet")),
    )
    .unwrap();
    assert!(matches!(s.load(), Err(StoreError::Corrupt { line: 3, .. })));
    std::fs::write(file(&d), &good).unwrap();
    assert_eq!(s.load().unwrap().lookup(a.secret.expose()), None);
}

#[test]
fn a_use_of_an_unknown_name_is_still_tolerated() {
    let d = TestDir::new("tole-dt-used-ghost");
    let s = store(&d);
    add(&s, "phone", Scope::Full);
    let mut f = std::fs::read_to_string(file(&d)).unwrap();
    f.push_str("{\"op\":\"used\",\"v\":1,\"name\":\"ghost\",\"at\":5}\n");
    std::fs::write(file(&d), f).unwrap();
    assert_eq!(s.load().unwrap().entries().len(), 1);
}

#[test]
fn a_revoke_validates_the_name_and_no_error_echoes_it_raw() {
    let d = TestDir::new("tole-dt-revoke-name");
    let s = store(&d);
    add(&s, "phone", Scope::Full);
    for bad in ["\u{1b}[31mx", "a\nb", "", "Phone"] {
        let e = s.revoke(bad, T0).unwrap_err();
        assert!(matches!(e, StoreError::BadName(_)));
        assert!(!e.to_string().contains(bad) || bad.is_empty() || bad == "Phone");
        assert!(!e.to_string().contains('\u{1b}'));
    }
    // Names that are valid and shown in an error come quoted and escaped.
    let e = s.revoke("nobody", T0).unwrap_err().to_string();
    assert!(e.contains("\"nobody\""), "{e}");
    assert_eq!(s.load().unwrap().entries().len(), 1);
}
