//! #525: `create_session` is the one place a session file is created AND
//! registered in the global index, shared by run, chat, tui, mission and the
//! server faces. Index registration is process-global state (it reads
//! `CODECORA_HOME`), so this file holds a single test that owns the
//! environment for the whole process.

use tole_cli::session_host::create_session;
use tole_cli::session_index;
use tole_core::storage::Storage;

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-create-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_created_session_is_pinned_and_registered_under_its_face() {
    let home = tmp("home");
    let dir = tmp("sessions");
    std::env::set_var("CODECORA_HOME", &home);
    std::env::remove_var("TOLE_NO_SESSION_INDEX");
    let index = session_index::index_path().expect("index path under CODECORA_HOME");
    assert!(index.starts_with(&home));

    // Registered: a file, a pinned prompt and alias, one index row.
    let s = create_session(
        &dir,
        "s-one",
        Some("be brief"),
        Some("alias-one"),
        Some("serve"),
    )
    .unwrap();
    assert_eq!(s.session_id(), "s-one");
    assert_eq!(s.system_prompt(), Some("be brief"));
    assert!(dir.join("s-one.jsonl").is_file());
    let rows = session_index::load(&index);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].id.as_str(), rows[0].face.as_str()),
        ("s-one", "serve")
    );
    assert_eq!(
        rows[0].file,
        dir.join("s-one.jsonl").canonicalize().unwrap()
    );

    // `face: None` = do not index (a caller that must stay off the user's index).
    let _ = create_session(&dir, "s-two", None, None, None).unwrap();
    assert!(dir.join("s-two.jsonl").is_file());
    assert_eq!(
        session_index::load(&index).len(),
        1,
        "no row for an unindexed session"
    );

    // The documented off switch wins over a face.
    std::env::set_var("TOLE_NO_SESSION_INDEX", "1");
    let _ = create_session(&dir, "s-three", None, None, Some("tui")).unwrap();
    std::env::remove_var("TOLE_NO_SESSION_INDEX");
    assert_eq!(
        session_index::load(&index).len(),
        1,
        "TOLE_NO_SESSION_INDEX=1 is honored"
    );

    // Another face gets its own row (the index lists newest first at one
    // second resolution, so compare as a set, not as an order).
    let _ = create_session(&dir, "s-four", None, None, Some("mission")).unwrap();
    let mut faces: Vec<_> = session_index::load(&index)
        .into_iter()
        .map(|e| e.face)
        .collect();
    faces.sort();
    assert_eq!(faces, ["mission", "serve"]);

    // An unsafe id is refused by the storage layer, and nothing is indexed.
    assert!(create_session(&dir, "../evil", None, None, Some("run")).is_err());
    assert_eq!(session_index::load(&index).len(), 2);

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&dir);
}
