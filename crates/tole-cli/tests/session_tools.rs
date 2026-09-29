//! #137: session_tools unit tests — multi-session routing semantics
//! without an LLM (CI-safe).

use serde_json::json;
use std::sync::Arc;
use tole_cli::session_tools::SessionToolState;
use tole_core::tool::Tool;

fn ws(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-sess-tools-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("a")).unwrap();
    std::fs::create_dir_all(d.join("b")).unwrap();
    std::fs::write(d.join("a/note.txt"), "ALPHA").unwrap();
    std::fs::write(d.join("b/note.txt"), "BETA").unwrap();
    d
}

fn tool_by_name<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> &'a dyn Tool {
    tools
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("tool {name} not found"))
        .as_ref()
}

#[test]
fn new_list_status_roundtrip() {
    let dir = ws("roundtrip");
    let state = SessionToolState::new(vec![], false, None, std::path::PathBuf::from(&dir));
    let tools = state.tools();

    let a = std::path::Path::new(&dir).join("a");
    let b = std::path::Path::new(&dir).join("b");

    let out = tool_by_name(&tools, "tole_session_new")
        .execute(json!({"cwd": a.to_string_lossy()}))
        .unwrap();
    let sid_a = out["session_id"].as_str().unwrap().to_string();
    let out = tool_by_name(&tools, "tole_session_new")
        .execute(json!({"cwd": b.to_string_lossy()}))
        .unwrap();
    let sid_b = out["session_id"].as_str().unwrap().to_string();
    assert_ne!(sid_a, sid_b, "session ids must be unique");

    let listed = tool_by_name(&tools, "tole_session_list")
        .execute(json!({}))
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);

    let st = tool_by_name(&tools, "tole_session_status")
        .execute(json!({"session_id": sid_a}))
        .unwrap();
    assert_eq!(st["busy"], json!(false));
}

#[test]
fn resolver_routes_to_session_jail() {
    let dir = ws("jail");
    let state = SessionToolState::new(vec![], false, None, std::path::PathBuf::from(&dir));
    let tools = state.tools();
    let a = std::path::Path::new(&dir).join("a");
    let b = std::path::Path::new(&dir).join("b");

    let sid_a = tool_by_name(&tools, "tole_session_new")
        .execute(json!({"cwd": a.to_string_lossy()}))
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let sid_b = tool_by_name(&tools, "tole_session_new")
        .execute(json!({"cwd": b.to_string_lossy()}))
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The resolver (as wired in mcp_http): session_id → that session's
    // registry; read_file there sees THAT session's jail.
    let sessions = Arc::clone(&state.sessions);
    let resolve = |sid: &str| -> Option<std::sync::Arc<tole_core::tool::ToolRegistry>> {
        tole_cli::session_host::lock_sessions(&sessions)
            .map
            .get(sid)
            .map(|st| Arc::clone(&st.registry))
    };

    let read = |sid: &str| {
        let reg = resolve(sid).unwrap();
        let out = reg
            .get("read_file")
            .unwrap()
            .execute(json!({"path": "note.txt"}))
            .unwrap();
        out["content"].as_str().unwrap().trim().to_string()
    };
    assert_eq!(read(&sid_a), "ALPHA");
    assert_eq!(read(&sid_b), "BETA");
}

#[test]
fn unknown_session_is_a_clear_error() {
    let root = ws("unknown-sid");
    let state = SessionToolState::new(vec![], false, None, root);
    let tools = state.tools();
    let err = tool_by_name(&tools, "tole_session_status")
        .execute(json!({"session_id": "nope"}))
        .unwrap_err();
    assert!(err.contains("unknown session"), "{err}");
}
