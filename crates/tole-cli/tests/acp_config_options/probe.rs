//! Issues #195 and #319: the lazy `/models` probe and how it degrades.

use crate::harness::{temp_cwd, AcpProcess};
use crate::mock::{spawn_mock, spawn_mock_with, ModelsReply};
use serde_json::json;
use std::time::Duration;

/// Issue #195: with NO TOLE_MODELS, the picker is auto-advertised from
/// the provider's GET /models — probed once per process, cached.
#[test]
fn model_picker_auto_advertised_from_provider_models() {
    use std::sync::atomic::Ordering;
    let (base_url, _seen, models_hits) = spawn_mock();
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_BASE_URL", base_url.as_str()),
        ("TOLE_MODEL", "model-a"),
        ("TOLE_API_KEY", "sk-test"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("autopick");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let opts = created["result"]["configOptions"].as_array().unwrap();
    let model = opts
        .iter()
        .find(|o| o["id"] == "model")
        .expect("auto picker");
    assert_eq!(
        model["options"].as_array().unwrap().len(),
        2,
        "options come from the provider's /models list"
    );
    assert_eq!(model["currentValue"], "model-a");
    assert_eq!(models_hits.load(Ordering::SeqCst), 1);

    // A second session/new must NOT re-probe (cached per process).
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(20));
    assert_eq!(
        models_hits.load(Ordering::SeqCst),
        1,
        "probe is once per process"
    );
}

/// Issue #195: an explicit TOLE_MODELS wins as-is — deterministic, no
/// network (the /models endpoint is never touched).
#[test]
fn tole_models_override_skips_provider_probe() {
    use std::sync::atomic::Ordering;
    let (base_url, _seen, models_hits) = spawn_mock();
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_BASE_URL", base_url.as_str()),
        ("TOLE_MODEL", "model-a"),
        ("TOLE_API_KEY", "sk-test"),
        ("TOLE_MODELS", "model-a,model-z"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("override");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let model = created["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "model")
        .expect("env picker")
        .clone();
    assert_eq!(
        model["options"].as_array().unwrap().len(),
        2,
        "exactly the env list, not the provider's"
    );
    assert_eq!(
        models_hits.load(Ordering::SeqCst),
        0,
        "no probe when env set"
    );
}

/// Issue #195: a failing probe degrades to today's behavior — approval
/// entry only, no model picker, no error surfaced to the client.
#[test]
fn failed_probe_degrades_to_no_picker() {
    let (base_url, _seen, _hits) = spawn_mock_with(ModelsReply::ServerError);
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_BASE_URL", base_url.as_str()),
        ("TOLE_MODEL", "model-a"),
        ("TOLE_API_KEY", "sk-test"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("probe500");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let opts = created["result"]["configOptions"].as_array().unwrap();
    assert_eq!(opts.len(), 1, "approval only: {opts:?}");
    assert_eq!(opts[0]["id"], "approval");
}

/// Issue #195: an empty /models list degrades exactly like a failure —
/// approval only, one stderr note, no picker.
#[test]
fn empty_models_list_degrades_to_no_picker() {
    let (base_url, _seen, _hits) = spawn_mock_with(ModelsReply::Empty);
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_BASE_URL", base_url.as_str()),
        ("TOLE_MODEL", "model-a"),
        ("TOLE_API_KEY", "sk-test"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("emptymodels");
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let created = acp.wait_response(2, Duration::from_secs(20));
    let opts = created["result"]["configOptions"].as_array().unwrap();
    assert_eq!(opts.len(), 1, "approval only: {opts:?}");
    assert_eq!(opts[0]["id"], "approval");
}

/// Issue #319: a /models endpoint that never answers must not stall
/// session/new for the old 60s wait, and the failure is remembered (30s
/// sentinel) so the next session/new neither re-probes nor waits again.
#[test]
fn hanging_models_endpoint_does_not_stall_session_new() {
    use std::sync::atomic::Ordering;
    let (base_url, _seen, models_hits) = spawn_mock_with(ModelsReply::Hang);
    let mut acp = AcpProcess::spawn_with(&[
        ("TOLE_BASE_URL", base_url.as_str()),
        ("TOLE_MODEL", "model-a"),
        ("TOLE_API_KEY", "sk-test"),
    ]);
    acp.initialize(1);
    let cwd = temp_cwd("probehang");
    let started = std::time::Instant::now();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    // Old code waited 60s; the bound is now 5s (+ slack for CI).
    let created = acp.wait_response(2, Duration::from_secs(15));
    assert!(started.elapsed() < Duration::from_secs(15));
    let opts = created["result"]["configOptions"].as_array().unwrap();
    assert_eq!(opts.len(), 1, "approval only: {opts:?}");
    assert_eq!(models_hits.load(Ordering::SeqCst), 1);

    // Within the failure TTL: instant answer, no second probe.
    let again = std::time::Instant::now();
    acp.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy()}
    }));
    let _ = acp.wait_response(3, Duration::from_secs(3));
    assert!(again.elapsed() < Duration::from_secs(3));
    assert_eq!(
        models_hits.load(Ordering::SeqCst),
        1,
        "failure sentinel suppresses the re-probe"
    );
}
