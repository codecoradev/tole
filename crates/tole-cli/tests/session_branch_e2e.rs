//! #210: `tole branch <id>` forks a session at a settled entry and
//! `tole sessions --tree` shows the shape, with the REAL binary.

use serde_json::json;
use std::path::PathBuf;
use std::process::{Command, Output};
use tole_core::entry::{EntryType, NewEntry};
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::{Commit, JsonlStorage, Storage};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-branch-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tole(dir: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .arg("--sessions-dir")
        .arg(dir)
        .args(args)
        .env("TOLE_NO_SESSION_INDEX", "1")
        .output()
        .unwrap()
}

fn seed(dir: &PathBuf) -> String {
    let mut s = JsonlStorage::create(dir, "s-main", None).unwrap();
    s.commit(Commit::new().entry(NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        json!({"role": "user", "text": "approach A"}),
    )))
    .unwrap();
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
        .unwrap();
    let seq = s.state().seq;
    let done = s
        .commit(
            Commit::new()
                .entry(NewEntry::root(
                    EntryType::new(EntryType::MESSAGE),
                    json!({"role": "assistant", "text": "did A"}),
                ))
                .transition(StateTransition::from(seq, Pc::Final)),
        )
        .unwrap();
    done[0].id.clone()
}

#[test]
fn branch_forks_a_session_and_the_tree_shows_it() {
    let dir = tmpdir("flow");
    let settled = seed(&dir);

    let out = tole(&dir, &["branch", "s-main"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let new_id = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("branched: ").map(str::to_string))
        .expect("prints `branched: <id>`");
    let f = JsonlStorage::open_read_only(dir.join(format!("{new_id}.jsonl"))).unwrap();
    assert_eq!(f.forked_from().unwrap().entry, settled);
    assert_eq!(f.entries().len(), 2);

    let tree = String::from_utf8(tole(&dir, &["sessions", "--tree"]).stdout).unwrap();
    let main_at = tree.find("s-main").expect(&tree);
    let fork_at = tree.find(new_id.as_str()).expect(&tree);
    assert!(main_at < fork_at, "root first, fork under it:\n{tree}");
    assert!(tree.contains("└─"), "{tree}");

    // Bad targets fail loudly.
    assert!(!tole(&dir, &["branch", "s-main", "--at", "nope"])
        .status
        .success());
    assert!(!tole(&dir, &["branch", "s-missing"]).status.success());
}
