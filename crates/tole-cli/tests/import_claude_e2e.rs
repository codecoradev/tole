//! #381: `tole import-claude` with the REAL binary.

use std::process::Command;

#[test]
fn import_claude_creates_a_resumable_session_and_refuses_a_repeat() {
    let dir = std::env::temp_dir().join(format!("tole-cc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tole-core/tests/fixtures/cc_session.jsonl"
    );
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_tole"))
            .arg("--sessions-dir")
            .arg(&dir)
            .args(["import-claude", fixture])
            .env("TOLE_NO_SESSION_INDEX", "1")
            .output()
            .unwrap()
    };
    let out = run();
    let (so, se) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{so}{se}");
    assert!(
        so.contains("imported: cc-11111111-2222-3333-4444-555555555555"),
        "{so}"
    );
    assert!(so.contains("tole resume cc-11111111"), "{so}");
    assert!(se.contains("line 9"), "malformed line warned: {se}");
    let again = run();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already imported"));
}
