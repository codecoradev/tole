//! Shared fixtures of the serve unit tests: a `State` over scratch directories and
//! helpers that open a session in-process.

use crate::serve::*;
use tole_cli::session_host::Sessions;

pub(super) fn test_state(root: std::path::PathBuf, sessions_dir: std::path::PathBuf) -> State {
    // The entrypoint fixes the process's skills choice before any
    // session opens (#945); these tests load none.
    tole_cli::session_host::set_server_skills(
        tole_cli::session::assemble::ServerSkills::new(vec![], true).unwrap(),
    );
    tole_cli::session::owner_mcp::set(vec![]).unwrap();
    State {
        sessions: Arc::new(Mutex::new(Sessions::default())),
        approvals: tole_cli::approvals::ApprovalQueue::shared(),
        allow_patterns: vec![],
        profiles: Default::default(),
        plan_mode: false,
        memory: None,
        auth: tole_cli::auth::Authenticator::token("t".into()),
        workspace_root: root,
        sessions_dir: Some(sessions_dir),
        turnend: vec![],
        live_connections: std::sync::atomic::AtomicUsize::new(0),
        buses: tole_cli::events::BusRegistry::default(),
        streams: tole_cli::events::StreamLimiter::new(tole_cli::events::MAX_STREAMS),
        auth_failures: tole_cli::auth::AuthLimiter::new(),
        uploads: None,
        hooks: Default::default(),
        hook_dir: None,
        audit_pending: Mutex::default(),
        mission: Default::default(),
        missions: Arc::default(),
        discussions: Arc::default(),
        models: Default::default(),
        turn_events: std::sync::OnceLock::new(),
    }
}

/// Open a serve session in-process and settle one turn in its log, so
/// there is something to fork. Returns (session id, seq of the answer).
pub(super) fn session_with_a_turn(state: &State, cwd: &std::path::Path) -> (String, u64) {
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::state::{Pc, StateTransition};
    use tole_core::storage::Commit;
    let (status, v) = route(
        state,
        "POST",
        "/sessions",
        &json!({"cwd": cwd.to_str().unwrap()}).to_string(),
    );
    assert_eq!(status, 200, "{v}");
    let id = v["sessionId"].as_str().unwrap().to_string();
    let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
    let mut g = st.storage.lock().unwrap();
    let msg = |role: &str, text: &str| {
        NewEntry::root(
            EntryType::new(EntryType::MESSAGE),
            json!({"role": role, "text": text}),
        )
    };
    g.commit(Commit::new().entry(msg("user", "hi"))).unwrap();
    let seq = g.state().seq;
    g.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))
        .unwrap();
    let seq = g.state().seq;
    g.commit(
        Commit::new()
            .entry(msg("assistant", "hello"))
            .transition(StateTransition::from(seq, Pc::Final)),
    )
    .unwrap();
    let answer = g
        .entries()
        .iter()
        .rev()
        .find(|e| e.payload["role"] == "assistant")
        .unwrap()
        .seq;
    (id, answer)
}

/// Open a serve session in-process; returns its id.
pub(super) fn open_session(state: &State, cwd: &std::path::Path) -> String {
    let (status, v) = route(
        state,
        "POST",
        "/sessions",
        &json!({"cwd": cwd.to_str().unwrap()}).to_string(),
    );
    assert_eq!(status, 200, "{v}");
    v["sessionId"].as_str().unwrap().to_string()
}
