//! #1024: the `Entry` wire form, byte for byte.
//!
//! `GET /sessions/{id}/entries` and the SSE `entry` frames of `tole serve`
//! return the `Entry` the append-only JSONL log holds, so a change to the
//! struct (a rename, a dropped field, another field order, another kind
//! constant) is at once a ledger-format change and a break for every client.
//! These strings are written out by hand on purpose: a serde edit that moves
//! them fails here and has to be accepted in this file, with the
//! additive-only rule of `docs/mobile-control.md` ("Wire compatibility") in
//! mind. One entry per kind the machine writes, with the payload shapes the
//! turn loop produces (the end-to-end copies are the golden files of
//! `tole-cli`'s `serve_wire_entries` test).

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use test_dir::TestDir;

use tole_core::entry::{Entry, EntryType, NewEntry};
use tole_core::storage::{Commit, JsonlStorage, Storage};

/// One entry of each kind, as a client reads it, in the key order it is
/// written in.
const WIRE: &[&str] = &[
    r#"{"seq":1,"id":"e_1","parentId":null,"type":"message","timestamp":1000,"payload":{"role":"user","text":"hello"}}"#,
    r#"{"seq":9,"id":"intent_8","parentId":null,"type":"intent","timestamp":1001,"payload":{"input":{"content":"x","path":"a.txt"},"replay":"guarded","tool":"write_file"}}"#,
    r#"{"seq":13,"id":"e_13","parentId":"intent_8","type":"tool_result","timestamp":1002,"payload":{"ok":true,"output":{"bytes":1,"path":"a.txt"}}}"#,
    r#"{"seq":4,"id":"e_4","parentId":"e_1","type":"error","timestamp":1003,"payload":{"detail":"write_file","error":"approval required"}}"#,
];

#[test]
fn the_kind_names_are_the_wire_names() {
    assert_eq!(EntryType::MESSAGE, "message");
    assert_eq!(EntryType::INTENT, "intent");
    assert_eq!(EntryType::TOOL_RESULT, "tool_result");
    assert_eq!(EntryType::ERROR, "error");
}

#[test]
fn each_kind_serializes_to_its_pinned_json_and_back() {
    for text in WIRE {
        let entry: Entry = serde_json::from_str(text).unwrap();
        assert_eq!(&serde_json::to_string(&entry).unwrap(), text);
        // The keys a client reads, nothing else.
        let keys: Vec<String> = serde_json::from_str::<Value>(text)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut keys = keys;
        keys.sort();
        assert_eq!(
            keys,
            ["id", "parentId", "payload", "seq", "timestamp", "type"]
        );
    }
    let kinds: Vec<String> = WIRE
        .iter()
        .map(|t| {
            serde_json::from_str::<Entry>(t)
                .unwrap()
                .kind
                .as_str()
                .to_string()
        })
        .collect();
    assert_eq!(kinds, ["message", "intent", "tool_result", "error"]);
}

/// Forward and backward compatibility, the two halves of "additive only":
/// a kind this build does not know replays and serializes unchanged; a file
/// written before `parentId` was always present reads as a root.
#[test]
fn unknown_kinds_survive_and_a_missing_parent_is_a_root() {
    let newer = r#"{"seq":2,"id":"e_2","parentId":null,"type":"from_the_future","timestamp":5,"payload":{"x":[1,2]}}"#;
    let entry: Entry = serde_json::from_str(newer).unwrap();
    assert_eq!(entry.kind.as_str(), "from_the_future");
    assert_eq!(serde_json::to_string(&entry).unwrap(), newer);

    let older = r#"{"seq":2,"id":"e_2","type":"message","timestamp":5,"payload":{}}"#;
    let entry: Entry = serde_json::from_str(older).unwrap();
    assert_eq!(entry.parent_id, None);
}

/// The line the storage layer writes for an entry: the same object with
/// `"kind":"entry"` added in front, and no `parentId` key for a root (the wire
/// form of the entries route always has it, as `null`; a reader of the file
/// takes a missing one as a root, see above).
#[test]
fn the_storage_layer_writes_the_pinned_line() {
    let dir = TestDir::new("tole-entry-wire");
    let mut s = JsonlStorage::create(&dir, "wire", Some("/tmp".into())).unwrap();
    let root = s
        .commit(Commit::new().entry(NewEntry {
            id: None,
            parent_id: None,
            kind: EntryType::new(EntryType::MESSAGE),
            payload: json!({"role": "user", "text": "hello"}),
            timestamp: 1000,
        }))
        .unwrap();
    assert_eq!(root[0].id, "e_1");
    s.commit(Commit::new().entry(NewEntry {
        id: Some("intent_x".into()),
        parent_id: Some("e_1".into()),
        kind: EntryType::new(EntryType::INTENT),
        payload: json!({"tool": "read_file"}),
        timestamp: 1001,
    }))
    .unwrap();
    let text = std::fs::read_to_string(dir.join("wire.jsonl")).unwrap();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(r#""kind":"entry""#))
        .collect();
    assert_eq!(
        lines,
        [
            r#"{"kind":"entry","seq":1,"id":"e_1","type":"message","timestamp":1000,"payload":{"role":"user","text":"hello"}}"#,
            r#"{"kind":"entry","seq":2,"id":"intent_x","parentId":"e_1","type":"intent","timestamp":1001,"payload":{"tool":"read_file"}}"#,
        ]
    );
}
