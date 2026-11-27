//! Tests of the session routes: the workspace jail, plan mode, fork, tree, compact and name.

use super::support::*;
use crate::serve::*;

/// cora 0.19 scan MAJOR: REST `POST /sessions` must enforce the same
/// jail-of-jails as the MCP transport — a cwd outside the server
/// workspace root is refused BEFORE any session file is created.
#[test]
fn rest_session_cwd_outside_workspace_root_is_refused() {
    let base = crate::test_dir::TestDir::new("tole-serve-jail");
    let root = base.join("ws");
    let inside = root.join("sub");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let state = test_state(root.clone(), sessions.clone());

    for escape in [
        "/",
        base.to_str().unwrap(),
        &format!("{}/..", root.display()),
    ] {
        let body = json!({ "cwd": escape }).to_string();
        let (status, payload) = route(&state, "POST", "/sessions", &body);
        assert_eq!(status, 403, "{escape}: {payload}");
        assert!(payload["error"].as_str().unwrap().contains("escapes"));
    }
    assert_eq!(
        std::fs::read_dir(&sessions).unwrap().count(),
        0,
        "a refused cwd must not leave a session file behind"
    );

    // Inside the root (and the default, omitted cwd) still open.
    let body = json!({ "cwd": inside.to_str().unwrap() }).to_string();
    let (status, payload) = route(&state, "POST", "/sessions", &body);
    assert_eq!(status, 200, "{payload}");
    assert_eq!(
        payload["workspace"],
        json!(inside.canonicalize().unwrap().to_str().unwrap())
    );
    let (status, payload) = route(&state, "POST", "/sessions", "{}");
    assert_eq!(status, 200, "{payload}");
    let _ = std::fs::remove_dir_all(&base);
}

/// #947: `plan_mode` on `POST /sessions` removes the mutation tools from
/// that session's registry (absence, not approval); off by default; a bad
/// type is a 400; and a process-level plan mode cannot be disabled.
#[test]
fn plan_mode_request_filters_the_registry_and_cannot_disable_the_process_flag() {
    let base = crate::test_dir::TestDir::new("tole-serve-plan-mode");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let mut state = test_state(root.clone(), sessions);
    let cwd = root.to_str().unwrap();
    let open = |state: &State, body: serde_json::Value| {
        let (s, v) = route(state, "POST", "/sessions", &body.to_string());
        (s, v)
    };
    let reg = |state: &State, v: &serde_json::Value| {
        let id = v["sessionId"].as_str().unwrap().to_string();
        let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
        st.registry.clone()
    };

    // Default: write tools present, no planMode on the wire.
    let (s, v) = open(&state, json!({"cwd": cwd}));
    assert_eq!(s, 200, "{v}");
    assert!(v.get("planMode").is_none());
    assert!(reg(&state, &v).get("write_file").is_some());

    // Explicit false is the default too.
    let (s, v) = open(&state, json!({"cwd": cwd, "plan_mode": false}));
    assert_eq!(s, 200, "{v}");
    assert!(reg(&state, &v).get("write_file").is_some());

    // On: mutation tools are absent, read tools stay.
    let (s, v) = open(&state, json!({"cwd": cwd, "plan_mode": true}));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["planMode"], true);
    let r = reg(&state, &v);
    assert!(r.get("read_file").is_some());
    for name in ["write_file", "run_command", "agent_start", "todo_write"] {
        assert!(r.get(name).is_none(), "{name} in plan mode");
    }

    // Bad type, and the resume combination, are 400s.
    for bad in [json!("yes"), json!(1), json!([true])] {
        let (s, _) = open(&state, json!({"cwd": cwd, "plan_mode": bad}));
        assert_eq!(s, 400);
    }
    let (s, _) = open(&state, json!({"resume": "x", "plan_mode": true}));
    assert_eq!(s, 400);

    // A process-level plan mode cannot be disabled by the request.
    state.plan_mode = true;
    let (s, v) = open(&state, json!({"cwd": cwd, "plan_mode": false}));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["planMode"], true);
    assert!(reg(&state, &v).get("write_file").is_none());
    let (s, v) = open(&state, json!({"cwd": cwd}));
    assert_eq!(s, 200, "{v}");
    assert!(reg(&state, &v).get("write_file").is_none());
}

/// #947: fork and tree over serve: success shape, the family tree, 404,
/// 409 while busy, and 4xx for an `at` that is not a settled entry.
#[test]
fn fork_and_tree_routes_answer_success_404_409_and_bad_at() {
    let base = crate::test_dir::TestDir::new("tole-serve-fork");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let state = test_state(root.clone(), sessions.clone());
    let (id, answer) = session_with_a_turn(&state, &root);

    // Unknown session: 404 on both routes.
    let (s, _) = route(&state, "POST", "/sessions/nope/fork", "{}");
    assert_eq!(s, 404);
    let (s, _) = route(&state, "GET", "/sessions/nope/tree", "");
    assert_eq!(s, 404);

    // A user message is not a settled point; an unknown seq is no entry.
    let user_seq = {
        let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
        let g = st.storage.lock().unwrap();
        g.entries()
            .iter()
            .find(|e| e.payload["role"] == "user")
            .unwrap()
            .seq
    };
    let path = format!("/sessions/{id}/fork");
    for bad in [
        json!({"at": user_seq}),
        json!({"at": 99999}),
        json!({"at": "x"}),
    ] {
        let (s, v) = route(&state, "POST", &path, &bad.to_string());
        assert_eq!(s, 400, "{bad}: {v}");
    }
    let (s, _) = route(&state, "POST", &path, "not json");
    assert_eq!(s, 400);
    let logs = || {
        std::fs::read_dir(&sessions)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .count()
    };
    assert_eq!(logs(), 1, "a refused fork leaves no log behind");

    // 409 while a turn runs.
    let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
    *st.busy.lock().unwrap() = true;
    let (s, _) = route(&state, "POST", &path, "{}");
    assert_eq!(s, 409);
    *st.busy.lock().unwrap() = false;

    // Success at an explicit seq, and again at the default.
    let (s, v) = route(&state, "POST", &path, &json!({"at": answer}).to_string());
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["forkedFrom"], json!(id));
    assert_eq!(v["seq"], json!(answer));
    let fork_id = v["id"].as_str().unwrap().to_string();
    assert!(sessions.join(format!("{fork_id}.jsonl")).exists());
    let (s, v) = route(&state, "POST", &path, "");
    assert_eq!(s, 200, "{v}");
    let fork2 = v["id"].as_str().unwrap().to_string();
    assert_ne!(fork2, fork_id);

    // The tree: the source at depth 0, both forks below it.
    let (s, v) = route(&state, "GET", &format!("/sessions/{id}/tree"), "");
    assert_eq!(s, 200, "{v}");
    let rows = v["tree"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{v}");
    assert_eq!(rows[0]["id"], json!(id));
    assert_eq!(rows[0]["depth"], 0);
    assert_eq!(rows[0]["current"], true);
    assert!(rows[0]["parent"].is_null());
    let child = rows.iter().find(|r| r["id"] == json!(fork_id)).unwrap();
    assert_eq!(child["parent"], json!(id));
    assert_eq!(child["depth"], 1);
    assert_eq!(child["current"], false);
    assert_eq!(child["turns"], 1);
    assert!(child["at"].is_string());
    // The tree is a file read: it answers while a turn runs.
    *st.busy.lock().unwrap() = true;
    let (s, _) = route(&state, "GET", &format!("/sessions/{id}/tree"), "");
    assert_eq!(s, 200);
    *st.busy.lock().unwrap() = false;
}

/// #947: compact and name over serve write what the TUI writes (read
/// back through the session's own readers), 404 an unknown session, 409
/// while a turn runs, and 400 a bad name.
#[test]
fn compact_and_name_routes_write_the_tui_registers_404_409_and_bad_name() {
    let base = crate::test_dir::TestDir::new("tole-serve-compact-name");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let state = test_state(root.clone(), sessions);
    let id = open_session(&state, &root);
    let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
    let compact = format!("/sessions/{id}/compact");
    let name = format!("/sessions/{id}/name");
    let named = json!({"name": "nightly"}).to_string();

    for (path, body) in [
        ("/sessions/nope/compact", ""),
        ("/sessions/nope/name", named.as_str()),
    ] {
        let (s, _) = route(&state, "POST", path, body);
        assert_eq!(s, 404, "{path}");
    }

    // 409 while a turn runs, and nothing is written.
    *st.busy.lock().unwrap() = true;
    let (s, _) = route(&state, "POST", &compact, "");
    assert_eq!(s, 409);
    let (s, _) = route(&state, "POST", &name, &named);
    assert_eq!(s, 409);
    *st.busy.lock().unwrap() = false;
    {
        let g = st.storage.lock().unwrap();
        assert_eq!(g.session_name(), None);
        assert!(!tole_core::compaction::overflow_pending(&*g));
    }
    // The turn lock itself held (a turn between the check and the write).
    {
        let _held = st.storage.lock().unwrap();
        let (s, _) = route(&state, "POST", &compact, "");
        assert_eq!(s, 409);
    }

    // Bad names are a 400 and write nothing.
    for bad in [
        "not json".to_string(),
        json!({}).to_string(),
        json!({"name": 5}).to_string(),
        json!({"name": "  "}).to_string(),
        json!({"name": "a\u{1b}b"}).to_string(),
        json!({"name": "x".repeat(121)}).to_string(),
    ] {
        let (s, v) = route(&state, "POST", &name, &bad);
        assert_eq!(s, 400, "{bad}: {v}");
    }

    // Success: the response shape and what the TUI would have written.
    let (s, v) = route(
        &state,
        "POST",
        &name,
        &json!({"name": " nightly "}).to_string(),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, json!({"id": id, "name": "nightly"}));
    let (s, v) = route(&state, "POST", &compact, "");
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, json!({"id": id, "compaction": "requested"}));
    let g = st.storage.lock().unwrap();
    assert_eq!(g.session_name(), Some("nightly"));
    assert!(tole_core::compaction::overflow_pending(&*g));
}
