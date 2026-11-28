//! `tole serve token add` when the token cannot be shown (#1320): it is revoked, not
//! left active and unknown.

mod common;
use common::TestDir;
use std::io::Write;
use tole_cli::serve_support::device_tokens::*;

/// A stdout that refuses every write, as a closed pipe does.
struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "pipe closed",
        ))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "pipe closed",
        ))
    }
}

/// A stdout that takes the write and fails the flush.
struct FlushFails(Vec<u8>);

impl Write for FlushFails {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("flush failed"))
    }
}

fn add(name: &str) -> TokenAction {
    TokenAction::Add {
        name: name.into(),
        scope: Scope::Full,
    }
}

#[test]
fn a_token_that_could_not_be_printed_is_revoked_and_the_command_fails() {
    let d = TestDir::new("tole-dt-broken-out");
    let s = Store::new(&d.join("tole"));
    let mut err = Vec::new();
    let e = run(&s, add("phone"), 7, &mut Broken, &mut err)
        .unwrap_err()
        .to_string();
    assert!(e.contains("revoked") && e.contains("\"phone\""), "{e}");
    assert!(err.is_empty(), "no sentence about a token nobody saw");
    let snap = s.load().unwrap();
    assert_eq!(snap.entries().len(), 1);
    assert!(!snap.entries()[0].active());
    // The name is free again.
    let mut out = Vec::new();
    run(&s, add("phone"), 8, &mut out, &mut Vec::new()).unwrap();
    assert!(s.load().unwrap().active("phone").is_some());
}

#[test]
fn a_flush_that_fails_counts_as_not_shown() {
    let d = TestDir::new("tole-dt-flush");
    let s = Store::new(&d.join("tole"));
    let mut out = FlushFails(Vec::new());
    let mut err = Vec::new();
    let e = run(&s, add("phone"), 7, &mut out, &mut err)
        .unwrap_err()
        .to_string();
    let token = String::from_utf8(out.0).unwrap();
    assert!(!e.contains(token.trim_end()) && err.is_empty());
    assert!(s.load().unwrap().active("phone").is_none());
}

#[test]
fn the_secret_comes_first_and_the_sentence_about_it_last() {
    let d = TestDir::new("tole-dt-order");
    let s = Store::new(&d.join("tole"));
    // One buffer for both streams shows the order they were written in.
    struct Both(std::rc::Rc<std::cell::RefCell<Vec<(char, String)>>>, char);
    impl Write for Both {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .borrow_mut()
                .push((self.1, String::from_utf8_lossy(b).into_owned()));
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Default::default();
    run(
        &s,
        add("phone"),
        7,
        &mut Both(std::rc::Rc::clone(&log), 'o'),
        &mut Both(std::rc::Rc::clone(&log), 'e'),
    )
    .unwrap();
    let log = log.borrow();
    let first_err = log.iter().position(|(k, _)| *k == 'e').unwrap();
    let last_out = log.iter().rposition(|(k, _)| *k == 'o').unwrap();
    assert!(
        last_out < first_err,
        "stdout is complete before stderr says anything"
    );
    let said: String = log
        .iter()
        .filter(|(k, _)| *k == 'e')
        .map(|(_, t)| t.as_str())
        .collect();
    assert!(said.ends_with("It is shown only now; keep it.\n"), "{said}");
    assert!(log[0].1.starts_with("tole_dt_"));
}
