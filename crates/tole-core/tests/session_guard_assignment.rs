//! #1302: a leading assignment whose quoted (or escaped) value holds a blank
//! hid the program from the guard's word cut (`X='a b' tole x` read `b'` as the
//! program). Every input here runs tole in bash. The guard refuses when a
//! leading assignment ends inside a quote, so each is refused, as `sh -c` text
//! and wrapped.

use std::path::Path;
use tole_core::session_guard::check_session_argv;

/// Shell text handed to `sh -c`, as one argument.
fn shell(text: &str) -> bool {
    check_session_argv(&["sh".into(), "-c".into(), text.into()], Path::new("/tmp")).is_err()
}

#[test]
fn a_quoted_blank_in_a_leading_assignment_is_refused() {
    for text in [
        "X='a b' tole cron resume j",
        "X=\"a b\" tole cron resume j",
        "X='a grep b' tole cron resume j",
        "A=\"1 2\" tole x",
        "X='a;tole' tole x",
        "X='a;c' tole x",
        "X=\"a;c\" tole x",
        "X='a b;c' tole x",
        "X=$'a b' tole x",
        "X=$'a\\'b c' tole x",
        "X=a\\ b tole x",
        "X=a\\\tb tole x",
        "FOO=1 BAR='a b' tole x",
        "FOO='a b' BAR=2 tole x",
        "X='a'\"b c\" tole x",
        "X='\"'\"a b\" tole x",
        "X=\"a\\\" b\" tole x",
        "echo ok; X='a b' tole x",
        "grep a f | X='a b' tole x",
        "if true; then X='a b' tole x; fi",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
}

#[test]
fn a_blank_the_shell_does_not_cut_at_is_part_of_the_word() {
    // NBSP, CR and VT are not blanks to a shell: `X=a<NBSP>b` is one
    // assignment and `tole` is the program. Rust's `split_whitespace` cut
    // there and read `b` as the program.
    for text in [
        "X=a\u{a0}b tole x",
        "X=a\rb tole x",
        "X=a\u{b}b tole x",
        "X=1\u{a0}Y=2 tole x",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
}

#[test]
fn an_open_assignment_after_a_wrapper_is_refused() {
    // The separator cut leaves `c' tole x` as its own command, which no word
    // check refuses: the open assignment behind the wrapper is the signal.
    for text in [
        "env X='a b;c' tole x",
        "nohup X='a b;c' tole x",
        "exec X=\"a;c\" tole x",
        "env -u Y X=\"a;c\" tole x",
        "sudo X=$'a b;c' tole x",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
}

/// Already refused before #1302 (a bare `tole` word reaches a payload of its
/// own); pinned so a later change to the nesting keeps them refused.
#[test]
fn the_same_behind_wrappers_and_nested_shells_stays_refused() {
    for text in [
        "env X='a b' tole x",
        "nohup X='a b' tole x",
        "exec X=\"a b\" tole x",
        "env FOO=1 BAR='a b' tole x",
        "sh -c \"X='a b' tole x\"",
        "sh -c 'X=\"a b\" tole x'",
        "sh -c 'sh -c \"X=\\\"a b\\\" tole x\"'",
        "bash -c 'env X=\"a b\" tole x'",
        "sh -c 'sh -c \"X=a\\ b tole x\"'",
        "env sh -c \"X='a b' tole x\"",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
}

#[test]
fn an_assignment_that_runs_nothing_is_still_allowed() {
    for text in [
        "X=1 grep a f",
        "X=a Y=b ls",
        "FOO=1 grep \"a|b\" f",
        "X='a' ls",
        "X=\"ab\" cat f",
        "grep -E \"a|tole\" f",
        "echo \"a|b\"",
        "if grep -q \"a|b\" f; then echo ok; fi",
        "FOO=1 echo a\u{a0}b",
        "X=$(date) cmd",
        "X=1 Y=2 grep a f",
    ] {
        assert!(!shell(text), "must allow: {text:?}");
    }
}

/// Refused on purpose, though nothing here runs tole: the guard does not tell
/// a quoted blank that is harmless from one that is not.
#[test]
fn a_leading_assignment_with_a_quoted_blank_is_refused_even_when_harmless() {
    for text in [
        "X='a b' ls",
        "X=\"a b\" grep c f",
        "X=a\\ b",
        "X='a b'",
        "X=\"$(date)\" cmd",
    ] {
        assert!(shell(text), "refused by design: {text:?}");
    }
}
