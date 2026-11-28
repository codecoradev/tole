//! #1314: `SecretSet::union_bounded` and `merge_process_secrets`: a union that
//! keeps the newest set whole and drops the oldest entries past the cap.

use tole_core::secrets::{merge_process_secrets, process_secrets, SecretSet};

fn one(name: &str, value: &str) -> SecretSet {
    SecretSet::default().with(name, value)
}

#[test]
fn a_union_redacts_both_sides_and_names_a_shared_value_by_the_newer_set() {
    let old = one("OLD", "old-secret-0123456789").with("SHARED", "shared-secret-0123456789");
    let new = one("NEW", "new-secret-0123456789").with("RENAMED", "shared-secret-0123456789");
    let u = old.union_bounded(&new, 100);
    assert_eq!(u.names(), vec!["OLD", "NEW", "RENAMED"]);
    let text = u.redact("old-secret-0123456789 new-secret-0123456789 shared-secret-0123456789");
    assert_eq!(text, "«OLD» «NEW» «RENAMED»");
}

#[test]
fn past_the_cap_the_oldest_entries_go_and_the_newest_set_stays_whole() {
    let mut set = SecretSet::default();
    for i in 0..6 {
        set = set.with(&format!("K{i}"), &format!("value-number-{i}-0123456789"));
    }
    let newer = one("N0", "newer-value-0-0123456789").with("N1", "newer-value-1-0123456789");
    let u = set.union_bounded(&newer, 5);
    assert_eq!(u.names(), vec!["K3", "K4", "K5", "N0", "N1"]);
    // A newest set larger than the cap is kept whole, nothing of the old one.
    let u = set.union_bounded(&newer, 1);
    assert_eq!(u.names(), vec!["N0", "N1"]);
}

/// One test: the process set is global to the test binary.
#[test]
fn merging_into_the_process_set_never_drops_what_an_earlier_merge_added() {
    merge_process_secrets(&one("SESSION_A", "session-a-secret-0123456789"));
    merge_process_secrets(&one("SESSION_B", "session-b-secret-0123456789"));
    merge_process_secrets(&one("SESSION_A", "session-a-secret-0123456789"));
    let out = process_secrets().redact("session-a-secret-0123456789 session-b-secret-0123456789");
    assert_eq!(out, "«SESSION_A» «SESSION_B»");
    // Repeating a turn does not grow the set.
    let before = process_secrets().names().len();
    merge_process_secrets(&one("SESSION_B", "session-b-secret-0123456789"));
    assert_eq!(process_secrets().names().len(), before);
}
