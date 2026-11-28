//! The global session index and the session tree (#439, #463): the unit tests
//! that moved out of `src/session/index.rs`, unchanged (they use only its
//! public API).

mod common;
use common::TestDir;
use std::io::Write;
use std::path::{Path, PathBuf};
use tole_cli::session::index::*;
use tole_core::storage::{JsonlStorage, Storage};

fn tmp() -> TestDir {
    TestDir::new("tole-session-index")
}

fn entry(id: &str, file: &Path, at: u64) -> Entry {
    Entry {
        id: id.into(),
        file: file.to_path_buf(),
        cwd: PathBuf::from("/work/proj"),
        face: "run".into(),
        created_at: at,
    }
}

#[test]
fn last_line_wins_newest_first_and_bad_lines_are_skipped() {
    let d = tmp();
    let idx = d.join("idx.jsonl");
    append(&idx, &entry("s-a", &d.join("a.jsonl"), 10)).unwrap();
    append(&idx, &entry("s-b", &d.join("b.jsonl"), 20)).unwrap();
    // A torn / foreign line must not break the index.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&idx)
        .unwrap()
        .write_all(b"{\"id\": \"s-x\", trunc\nnot json\n")
        .unwrap();
    // Non-UTF-8 garbage mid-file must not hide the lines after it.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&idx)
        .unwrap()
        .write_all(b"\xff\xfe\x00 torn\n")
        .unwrap();
    append(&idx, &entry("s-a", &d.join("a2.jsonl"), 30)).unwrap();
    let all = load(&idx);
    let ids: Vec<&str> = all.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["s-a", "s-b"]);
    assert_eq!(all[0].file, d.join("a2.jsonl"));
    assert!(load(&d.join("missing.jsonl")).is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&idx).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "owner-only");
    }
}

#[test]
fn rows_read_the_session_file_and_skip_gone_sessions() {
    let d = tmp();
    let idx = d.join("idx.jsonl");
    let mut s = JsonlStorage::create(&d, "s-live", None).unwrap();
    use tole_core::entry::{EntryType, NewEntry};
    s.commit(tole_core::storage::Commit::new().entry(NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        serde_json::json!({"role": "user", "text": "fix the\nflaky parser test please, it fails on CI every other run"}),
    )))
    .unwrap();
    append(&idx, &entry("s-live", &d.join("s-live.jsonl"), 5)).unwrap();
    append(&idx, &entry("s-gone", &d.join("s-gone.jsonl"), 9)).unwrap();
    let rows = rows(&idx, None);
    assert_eq!(rows.len(), 1, "a deleted session is not listed");
    assert_eq!(rows[0].turns, 1);
    assert!(
        rows[0].preview.starts_with("fix the flaky parser"),
        "{:?}",
        rows[0]
    );
    assert!(rows[0].preview.ends_with('…'));
    assert!(!rows[0].preview.contains('\n'));
    assert_eq!(
        tole_cli::session::index::rows(&idx, Some("PARSER")).len(),
        1
    );
    assert_eq!(
        tole_cli::session::index::rows(&idx, Some("proj")).len(),
        1,
        "cwd matches"
    );
    assert!(tole_cli::session::index::rows(&idx, Some("nothing-like-this")).is_empty());
    assert_eq!(find(&idx, "s-live").unwrap().id, "s-live");
    assert!(find(&idx, "s-gone").is_none());
    // The resume hint names the project dir (and a non-default
    // sessions dir).
    let e = find(&idx, "s-live").unwrap();
    assert_eq!(
        resume_hint(&e),
        format!(
            "cd /work/proj && tole --sessions-dir {} resume s-live",
            d.display()
        )
    );
    let spaced = Entry {
        cwd: PathBuf::from("/work/my proj"),
        ..e.clone()
    };
    assert!(resume_hint(&spaced).starts_with("cd '/work/my proj' && "));
    // An entry whose file is not `<id>.jsonl` never resolves the id.
    append(&idx, &entry("s-other", &d.join("s-live.jsonl"), 7)).unwrap();
    assert!(find(&idx, "s-other").is_none());
}

fn node(id: &str, parent: Option<&str>) -> TreeNode {
    TreeNode {
        id: id.into(),
        parent: parent.map(str::to_string),
        at: parent.map(|_| "e1".to_string()),
        pc: "Final".into(),
        turns: 1,
    }
}

fn ids(v: &[(usize, TreeNode)]) -> Vec<(usize, &str)> {
    v.iter().map(|(d, n)| (*d, n.id.as_str())).collect()
}

/// #463: the family is the whole tree around the session, depths are
/// relative to its root, other trees and the rest of the forest are out.
#[test]
fn family_is_the_tree_around_a_session() {
    let nodes = || {
        vec![
            node("a", None),
            node("b", Some("a")),
            node("c", Some("a")),
            node("d", Some("b")),
            node("x", None),
            node("y", Some("x")),
            // a fork whose source file is gone is its own root
            node("o", Some("gone")),
            node("p", Some("o")),
        ]
    };
    let want = [(0, "a"), (1, "b"), (2, "d"), (1, "c")];
    for cur in ["a", "b", "c", "d"] {
        assert_eq!(ids(&family(nodes(), cur)), want, "from {cur}");
    }
    assert_eq!(ids(&family(nodes(), "y")), [(0, "x"), (1, "y")]);
    assert_eq!(ids(&family(nodes(), "p")), [(0, "o"), (1, "p")]);
    assert!(family(nodes(), "missing").is_empty());
    // a corrupt parent cycle terminates
    let cyc = vec![node("m", Some("n")), node("n", Some("m"))];
    let _ = family(cyc, "m");
}
