//! Tests of the mission routes.

use super::support::*;
use crate::serve::*;

/// #947: a mission request names only its goal and budgets. `verify` (the
/// owner's command) and every other field is a 400 that starts nothing;
/// a running mission makes the next one a 409; cancel flips the token of
/// the running one only; a plan-mode server refuses to start one.
#[test]
fn mission_routes_refuse_owner_fields_and_keep_one_mission_at_a_time() {
    let base = crate::test_dir::TestDir::new("tole-serve-mission");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let mut state = test_state(root, sessions.clone());
    state.mission.verify = Some("true".into());

    for body in [
        json!({"goal": "g", "verify": "rm -rf /"}),
        json!({"goal": "g", "verify": null}),
        json!({"goal": "g", "sandbox": "off"}),
        json!({"goal": "g", "cwd": "/"}),
        json!({"goal": "  "}),
        json!({"goal": "g", "max_steps": 1000000}),
    ] {
        let (s, v) = route(&state, "POST", "/missions", &body.to_string());
        assert_eq!(s, 400, "{body}: {v}");
    }
    let (s, v) = route(
        &state,
        "POST",
        "/missions",
        &json!({"goal": "g", "verify": "x"}).to_string(),
    );
    assert_eq!(s, 400);
    assert!(v["error"].as_str().unwrap().contains("owner"), "{v}");
    assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), 0);

    // A running mission (planted) blocks a second one and takes a cancel.
    let cancel = tole_core::cancel::CancelToken::new();
    state.missions.plant_running("mission-a", cancel.clone());
    let (s, _) = route(&state, "POST", "/missions", r#"{"goal":"g"}"#);
    assert_eq!(s, 409);
    let (s, v) = route(&state, "GET", "/missions/mission-a", "");
    assert_eq!(
        (s, v["status"].clone(), v["running"].clone()),
        (200, json!("running"), json!(true))
    );
    assert!(!cancel.is_cancelled());
    let (s, _) = route(&state, "POST", "/missions/mission-a/cancel", "");
    assert_eq!(s, 202);
    assert!(cancel.is_cancelled());
    let (s, _) = route(&state, "POST", "/missions/mission-b/cancel", "");
    assert_eq!(s, 404);
    let (s, _) = route(&state, "GET", "/missions/mission-b", "");
    assert_eq!(s, 404);

    // Plan mode: no mission.
    let mut ro = test_state(state.workspace_root.clone(), sessions);
    ro.plan_mode = true;
    let (s, _) = route(&ro, "POST", "/missions", r#"{"goal":"g"}"#);
    assert_eq!(s, 409);
}
