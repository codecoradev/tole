use super::*;
use tole_core::entry::{EntryType, NewEntry};
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::Commit;

fn msg(role: &str, text: &str) -> NewEntry {
    NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        serde_json::json!({"role": role, "text": text}),
    )
}

/// One settled turn (user message, final answer) on `s`; the answer's id.
fn settle(s: &mut JsonlStorage, user: &str, answer: &str) -> String {
    s.commit(Commit::new().entry(msg("user", user))).unwrap();
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
        .unwrap();
    let seq = s.state().seq;
    let added = s
        .commit(
            Commit::new()
                .entry(msg("assistant", answer))
                .transition(StateTransition::from(seq, Pc::Final)),
        )
        .unwrap();
    added[0].id.clone()
}

fn texts(s: &JsonlStorage) -> Vec<String> {
    s.entries()
        .iter()
        .map(|e| e.payload["text"].as_str().unwrap_or("").to_string())
        .collect()
}

fn jsonl_files(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    v.sort();
    v
}

/// A TUI session `id` holding two settled turns; the two answer ids.
fn two_turns(dir: &std::path::Path, id: &str) -> (TuiSession, String, String) {
    let s = session(dir, id);
    let (a, b) = {
        let mut g = s.lock();
        let a = settle(&mut g.storage, "q1", "a1");
        let b = settle(&mut g.storage, "q2", "a2");
        (a, b)
    };
    (s, a, b)
}

#[test]
fn fork_defaults_to_the_last_settled_entry_while_this_process_holds_the_lock() {
    let dir = tmp("fork-default");
    let (s, _a, b) = two_turns(&dir, "s-1");
    let src_bytes = std::fs::read(dir.join("s-1.jsonl")).unwrap();
    // s-1 is open (writer lock held by THIS process) while it is forked.
    let reply = s.apply(HostCmd::Fork(None));
    let Ok(HostReply::Session(new_id)) = reply else {
        panic!("fork refused: {reply:?}");
    };
    assert_ne!(new_id, "s-1");
    assert_eq!(s.session_id(), new_id, "the TUI switched to the fork");
    assert_eq!(s.footer().session, new_id);
    {
        let g = s.lock();
        let o = g.storage.forked_from().expect("a fork");
        assert_eq!((o.session.as_str(), o.entry.as_str()), ("s-1", b.as_str()));
        assert_eq!(texts(&g.storage), ["q1", "a1", "q2", "a2"]);
        assert_eq!(g.storage.state().pc, Pc::Final);
    }
    // the original is byte-identical, released, and resumable
    assert_eq!(std::fs::read(dir.join("s-1.jsonl")).unwrap(), src_bytes);
    let re = JsonlStorage::open(dir.join("s-1.jsonl")).expect("source released, not locked");
    assert!(re.forked_from().is_none());
    drop(re);
    assert_eq!(s.switch_session("s-1").unwrap(), "s-1");
    assert_eq!(
        s.apply(HostCmd::Switch(new_id.clone())),
        Ok(HostReply::Session(new_id))
    );
}

#[test]
fn fork_at_an_earlier_settled_entry_cuts_there_and_new_work_stays_on_the_fork() {
    let dir = tmp("fork-at");
    let (s, a, _b) = two_turns(&dir, "s-1");
    let src_bytes = std::fs::read(dir.join("s-1.jsonl")).unwrap();
    let Ok(HostReply::Session(new_id)) = s.apply(HostCmd::Fork(Some(a.clone()))) else {
        panic!("fork refused");
    };
    {
        let mut g = s.lock();
        let o = g.storage.forked_from().unwrap().clone();
        assert_eq!((o.session.as_str(), o.entry.as_str()), ("s-1", a.as_str()));
        assert_eq!(texts(&g.storage), ["q1", "a1"]);
        // the branch diverges: new work lands in the fork, never the source
        settle(&mut g.storage, "q3", "a3");
    }
    assert_eq!(std::fs::read(dir.join("s-1.jsonl")).unwrap(), src_bytes);
    assert_eq!(
        texts(&JsonlStorage::open_read_only(dir.join(format!("{new_id}.jsonl"))).unwrap()),
        ["q1", "a1", "q3", "a3"]
    );
}

#[test]
fn fork_of_an_unsettled_unknown_or_empty_source_is_refused_and_leaves_nothing() {
    let dir = tmp("fork-refuse");
    let (s, _a, _b) = two_turns(&dir, "s-1");
    let user_msg = s.lock().storage.entries()[0].id.clone();
    let before = jsonl_files(&dir);
    let e = s.apply(HostCmd::Fork(Some(user_msg))).unwrap_err();
    assert!(e.contains("not a settled point"), "{e}");
    let e = s.apply(HostCmd::Fork(Some("nope".into()))).unwrap_err();
    assert!(e.contains("unknown entry"), "{e}");
    assert_eq!(s.session_id(), "s-1", "a refused fork keeps the session");
    assert_eq!(
        jsonl_files(&dir),
        before,
        "no half-written fork left behind"
    );
    // a fresh session has nothing settled to fork from
    let e = s
        .new_session()
        .and_then(|_| s.fork_session(None))
        .unwrap_err();
    assert!(e.contains("no settled entry"), "{e}");
    assert_eq!(
        jsonl_files(&dir).len(),
        before.len() + 1,
        "only /new added a file"
    );
}

#[test]
fn tree_lists_parent_siblings_and_grandchildren_indented_with_the_current_marked() {
    let dir = tmp("tree");
    let mut root = JsonlStorage::create_with(&dir, "r", None, Some("sys")).unwrap();
    let cut = settle(&mut root, "q1", "a1");
    drop(root);
    // r -> {b, c}; b -> d
    for (id, from) in [("b", "r"), ("c", "r"), ("d", "b")] {
        drop(JsonlStorage::fork(dir.join(format!("{from}.jsonl")), &dir, id, None).unwrap());
    }
    // an unrelated session never shows
    drop(JsonlStorage::create_with(&dir, "z", None, Some("sys")).unwrap());

    let s = session_reopen(&dir, "b");
    let Ok(HostReply::Sessions(rows)) = s.apply(HostCmd::PickTree) else {
        panic!("no tree");
    };
    let at = format!("(@{cut})");
    let want = [
        ("r", "r  1 turns".to_string()),
        ("b", format!("└─ b {at}  1 turns  (current)")),
        ("d", format!("   └─ d {at}  1 turns")),
        ("c", format!("└─ c {at}  1 turns")),
    ];
    let got: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.id.as_str(), r.label.as_str()))
        .collect();
    let want: Vec<(&str, &str)> = want.iter().map(|(i, l)| (*i, l.as_str())).collect();
    assert_eq!(got, want);
    // the same family from a leaf and from the root
    let leaf = session_reopen(&dir, "d");
    drop(s);
    let ids = |v: Vec<SessionItem>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(leaf.tree().unwrap()), ["r", "b", "d", "c"]);
    // a session with no parent and no forks is just itself
    let z = session_reopen(&dir, "z");
    let rows = z.tree().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "z");
    assert!(rows[0].label.ends_with("(current)"));
}

#[test]
fn tree_after_fork_shows_the_new_session_under_the_source_and_switches_back() {
    let dir = tmp("tree-fork");
    let (s, _a, b) = two_turns(&dir, "s-1");
    let Ok(HostReply::Session(f1)) = s.apply(HostCmd::Fork(None)) else {
        panic!("fork refused");
    };
    let Ok(HostReply::Sessions(rows)) = s.apply(HostCmd::PickTree) else {
        panic!("no tree");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "s-1");
    assert!(rows[0].label.starts_with("s-1  "), "{}", rows[0].label);
    assert_eq!(rows[1].id, f1);
    assert_eq!(rows[1].label, format!("└─ {f1} (@{b})  2 turns  (current)"));
    // picking the parent switches there (Switch), and the tree still has both
    assert_eq!(
        s.apply(HostCmd::Switch("s-1".into())),
        Ok(HostReply::Session("s-1".into()))
    );
    let ids: Vec<_> = s.tree().unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["s-1".to_string(), f1]);
}
