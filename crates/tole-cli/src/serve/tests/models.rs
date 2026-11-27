//! Tests of the model and effort routes and the model list.

use super::support::*;
use crate::serve::*;

/// #574: the model list is the owner's, `model` and `effort` write the
/// registers the TUI writes (read back through the session's readers),
/// and 404 / 409 / 400 hold, including a model outside the list.
#[test]
fn model_and_effort_routes_write_the_tui_registers_and_obey_the_allow_list() {
    let base = crate::test_dir::TestDir::new("tole-serve-model-effort");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let mut state = test_state(root.clone(), sessions);
    state.models = models::ModelCatalog::new(
        Some(" gpt-a, gpt-b ,gpt-a,,bad\u{7}id"),
        Some("gpt-default".into()),
    );
    let id = open_session(&state, &root);
    let st = lock_sessions(&state.sessions).get(&id).cloned().unwrap();
    let model = format!("/sessions/{id}/model");
    let effort = format!("/sessions/{id}/effort");
    let pick = |m: &str| json!({"model": m}).to_string();

    // The list: the default first, then the owner's allow list.
    let (s, v) = route(&state, "GET", "/models", "");
    assert_eq!(s, 200);
    assert_eq!(
        v,
        json!({"models": ["gpt-default", "gpt-a", "gpt-b"], "default": "gpt-default"})
    );

    // 404 on an unknown session.
    let (s, _) = route(&state, "POST", "/sessions/nope/model", &pick("gpt-a"));
    assert_eq!(s, 404);
    let (s, _) = route(
        &state,
        "POST",
        "/sessions/nope/effort",
        r#"{"effort":"low"}"#,
    );
    assert_eq!(s, 404);

    // 409 while a turn runs, and nothing is written.
    *st.busy.lock().unwrap() = true;
    assert_eq!(route(&state, "POST", &model, &pick("gpt-a")).0, 409);
    assert_eq!(route(&state, "POST", &effort, r#"{"effort":"low"}"#).0, 409);
    *st.busy.lock().unwrap() = false;
    {
        let _held = st.storage.lock().unwrap();
        assert_eq!(route(&state, "POST", &model, &pick("gpt-a")).0, 409);
        assert_eq!(route(&state, "POST", &effort, r#"{"effort":"low"}"#).0, 409);
    }
    {
        let g = st.storage.lock().unwrap();
        assert_eq!(tole_cli::turn_runner::session_model(&*g, "dflt"), "dflt");
        assert_eq!(tole_cli::turn_runner::session_effort(&*g, None), None);
    }

    // 400: bad bodies, a model outside the list (also a provider URL,
    // another case, a prefix of an allowed id) and bad levels.
    for bad in [
        "not json".to_string(),
        json!({}).to_string(),
        json!({"model": 5}).to_string(),
        json!({"model": "  "}).to_string(),
        pick("gpt-c"),
        pick("GPT-A"),
        pick("gpt"),
        pick("https://evil.example/v1"),
        json!({"model": "gpt-a", "base_url": "http://evil", "api_key": "k"})
            .to_string()
            .replace("gpt-a", "not-allowed"),
    ] {
        let (s, v) = route(&state, "POST", &model, &bad);
        assert_eq!(s, 400, "{bad}: {v}");
    }
    for bad in [
        "not json".to_string(),
        json!({}).to_string(),
        json!({"effort": "ultra"}).to_string(),
        json!({"effort": ""}).to_string(),
        json!({"effort": 3}).to_string(),
    ] {
        let (s, v) = route(&state, "POST", &effort, &bad);
        assert_eq!(s, 400, "{bad}: {v}");
    }
    {
        let g = st.storage.lock().unwrap();
        assert_eq!(tole_cli::turn_runner::session_model(&*g, "dflt"), "dflt");
        assert_eq!(tole_cli::turn_runner::session_effort(&*g, None), None);
    }

    // Success: the response shape and the registers the TUI reads.
    let (s, v) = route(&state, "POST", &model, &pick(" gpt-b "));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, json!({"id": id, "model": "gpt-b"}));
    let (s, v) = route(&state, "POST", &effort, r#"{"effort":" HIGH "}"#);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, json!({"id": id, "effort": "high"}));
    {
        let g = st.storage.lock().unwrap();
        assert_eq!(g.get_register("fact", "model"), Some(&json!("gpt-b")));
        assert_eq!(
            g.get_register("fact", "reasoning_effort"),
            Some(&json!("high"))
        );
        assert_eq!(tole_cli::turn_runner::session_model(&*g, "dflt"), "gpt-b");
        assert_eq!(
            tole_cli::turn_runner::session_effort(&*g, Some("low")),
            Some("high".into())
        );
    }
    // The default is always allowed (switch back); null and "unset" clear.
    assert_eq!(route(&state, "POST", &model, &pick("gpt-default")).0, 200);
    let (s, v) = route(&state, "POST", &effort, r#"{"effort":null}"#);
    assert_eq!((s, v["effort"].clone()), (200, json!(null)));
    route(&state, "POST", &effort, r#"{"effort":"low"}"#);
    assert_eq!(
        route(&state, "POST", &effort, r#"{"effort":"unset"}"#).0,
        200
    );
    let g = st.storage.lock().unwrap();
    assert_eq!(
        tole_cli::turn_runner::session_model(&*g, "dflt"),
        "gpt-default"
    );
    assert_eq!(g.get_register("fact", "reasoning_effort"), None);
}

/// #574: with no `TOLE_MODELS` the default is the only model a client
/// can pick, and with no default either the list is empty.
#[test]
fn without_an_owner_list_only_the_default_can_be_picked() {
    let base = crate::test_dir::TestDir::new("tole-serve-model-default");
    let root = base.join("ws");
    let sessions = base.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let mut state = test_state(root.clone(), sessions);
    state.models = models::ModelCatalog::new(None, Some("only".into()));
    let id = open_session(&state, &root);
    let model = format!("/sessions/{id}/model");
    assert_eq!(
        route(&state, "GET", "/models", "").1["models"],
        json!(["only"])
    );
    assert_eq!(route(&state, "POST", &model, r#"{"model":"other"}"#).0, 400);
    assert_eq!(route(&state, "POST", &model, r#"{"model":"only"}"#).0, 200);
    state.models = models::ModelCatalog::new(None, None);
    let (s, v) = route(&state, "GET", "/models", "");
    assert_eq!((s, v), (200, json!({"models": [], "default": null})));
    assert_eq!(route(&state, "POST", &model, r#"{"model":"only"}"#).0, 400);
}
