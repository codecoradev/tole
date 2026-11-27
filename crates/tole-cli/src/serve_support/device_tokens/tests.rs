//! What only the module itself can see: the shape check that spares the file, and the time format.

use super::cmd::utc;
use super::*;
use crate::test_dir::TestDir;

#[test]
fn a_token_of_the_wrong_shape_costs_no_file_access() {
    let d = TestDir::new("tole-dt-shape");
    let s = Store::new(&d.join("tole"));
    let live = Live::new(s.clone());
    s.add("phone", Scope::Full, 1).unwrap();
    assert_eq!(live.lookup("the-shared-bearer-token"), None);
    assert_eq!(live.lookup(PREFIX), None);
    assert!(!live.loaded(), "the file was never looked at");
    // A token with the shape does read it.
    assert_eq!(live.lookup(&format!("{PREFIX}{}", "A".repeat(43))), None);
    assert!(live.loaded());
}

#[test]
fn times_are_utc_minutes() {
    assert_eq!(utc(0), "1970-01-01 00:00Z");
    assert_eq!(utc(1_795_000_000), "2026-11-18 11:06Z");
}
