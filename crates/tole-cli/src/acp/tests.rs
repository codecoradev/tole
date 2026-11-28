use super::config_options::{model_config_option, parse_model_list, PROBE_FAIL_TTL};
use super::*;

#[test]
fn resolve_model_list_env_wins_without_probe() {
    let mut probed = false;
    let out = resolve_model_list(Some("a, b"), || {
        probed = true;
        vec!["z".to_string()]
    });
    assert_eq!(out, vec!["a", "b"]);
    assert!(!probed, "a non-empty TOLE_MODELS must bypass the probe");
}

#[test]
fn resolve_model_list_falls_to_probe_when_env_unset_or_empty() {
    assert_eq!(resolve_model_list(None, || vec!["z".into()]), vec!["z"]);
    assert_eq!(
        resolve_model_list(Some("  , "), || vec!["z".into()]),
        vec!["z"]
    );
}

#[test]
fn resolve_model_list_keeps_empty_probe_result() {
    assert!(resolve_model_list(None, Vec::new).is_empty());
}

fn probe_ok() -> Option<Vec<String>> {
    Some(vec!["m".into()])
}

#[test]
fn probe_cache_fresh_failure_served_from_sentinel() {
    let t0 = std::time::Instant::now();
    let mut c = ProbeCache::default();
    assert!(c.get(t0, || None).is_empty());
    let almost = t0 + PROBE_FAIL_TTL - std::time::Duration::from_secs(1);
    let out = c.get(almost, || panic!("must not re-probe within TTL"));
    assert!(out.is_empty());
}

#[test]
fn probe_cache_retries_after_ttl_and_caches_success() {
    let t0 = std::time::Instant::now();
    let mut c = ProbeCache::default();
    assert!(c.get(t0, || None).is_empty());
    let later = t0 + PROBE_FAIL_TTL;
    assert_eq!(c.get(later, probe_ok), vec!["m"]);
    // Success is permanent: no probe, even far in the future.
    let far = later + PROBE_FAIL_TTL * 100;
    assert_eq!(c.get(far, || panic!("success must stay cached")), vec!["m"]);
}

#[test]
fn probe_cache_failure_after_ttl_restarts_window() {
    let t0 = std::time::Instant::now();
    let mut c = ProbeCache::default();
    assert!(c.get(t0, || None).is_empty());
    let t1 = t0 + PROBE_FAIL_TTL;
    assert!(c.get(t1, || None).is_empty());
    let soon = t1 + std::time::Duration::from_secs(1);
    assert!(c.get(soon, || panic!("window restarted")).is_empty());
}

#[test]
fn model_list_unset_or_empty_advertises_nothing() {
    assert!(parse_model_list(None).is_empty());
    assert!(parse_model_list(Some("")).is_empty());
    assert!(parse_model_list(Some("  , , ")).is_empty());
}

#[test]
fn model_list_trims_dedupes_keeps_order() {
    assert_eq!(
        parse_model_list(Some(
            " glm/glm-5.3-flash , glm/glm-5.3 , glm/glm-5.3-flash "
        )),
        vec!["glm/glm-5.3-flash", "glm/glm-5.3"]
    );
}

#[test]
fn config_options_without_models_still_advertise_approval() {
    assert!(model_config_option("m", &[]).is_none());
    // The approval selector is always advertised; the model picker
    // is the optional one (issue #176).
    let opts = config_options_for("m", &[], false).expect("approval entry");
    assert_eq!(opts.len(), 1);
    assert_eq!(opts[0]["id"], "approval");
    assert_eq!(opts[0]["category"], "mode");
    assert_eq!(opts[0]["currentValue"], "ask");
}

#[test]
fn config_option_wire_shape_matches_spec() {
    let o =
        model_config_option("model-a", &["model-a".into(), "model-b".into()]).expect("advertised");
    assert_eq!(o["id"], "model");
    assert_eq!(o["name"], "Model");
    assert_eq!(o["category"], "model");
    assert_eq!(o["type"], "select");
    assert_eq!(o["currentValue"], "model-a");
    assert_eq!(
        o["options"],
        json!([
            {"value": "model-a", "name": "model-a"},
            {"value": "model-b", "name": "model-b"},
        ])
    );
}

#[test]
fn config_option_prepends_current_when_missing_from_list() {
    // The spec requires currentValue ∈ options; the env default (or a
    // stale override after an env change) must therefore be prepended.
    let o = model_config_option("model-z", &["model-a".into()]).expect("advertised");
    assert_eq!(o["currentValue"], "model-z");
    assert_eq!(
        o["options"][0],
        json!({"value": "model-z", "name": "model-z"})
    );
    assert_eq!(o["options"].as_array().expect("options").len(), 2);
}

#[test]
fn config_options_order_and_empty_current_fallback() {
    // Approval first (priority order per the spec), model second;
    // an empty current model falls back to the first advertised id.
    let opts = config_options_for("", &["model-a".into(), "model-b".into()], true).expect("opts");
    assert_eq!(opts.len(), 2);
    assert_eq!(opts[0]["id"], "approval");
    assert_eq!(opts[0]["currentValue"], "auto");
    assert_eq!(opts[1]["id"], "model");
    assert_eq!(opts[1]["currentValue"], "model-a");
}

/// A stand-in resolver: records the base64 it was handed as the path.
fn fake_resolve(b64: &str) -> Result<tole_core::image::ImageRef, String> {
    if b64 == "bad" {
        return Err("image: inline image: not png/jpeg/gif/webp".into());
    }
    Ok(tole_core::image::ImageRef {
        path: format!("/spill/{b64}"),
        mime: "image/png".into(),
        bytes: 1,
        fnv1a64: "0".repeat(16),
    })
}

#[test]
fn prompt_content_maps_text_and_image_blocks() {
    let p = json!([
        {"type": "text", "text": "what is "},
        {"type": "image", "data": "AAA", "mimeType": "image/png"},
        {"type": "text", "text": "this?"},
        {"type": "audio", "data": "BBB", "mimeType": "audio/wav"},
        {"type": "resource_link", "uri": "file:///x", "name": "x"},
    ]);
    let (text, images) = prompt_content(Some(&p), &fake_resolve).unwrap();
    assert_eq!(text, "what is this?");
    assert_eq!(images.len(), 1, "audio and links stay ignored");
    assert_eq!(images[0].path, "/spill/AAA");
    // a plain string is still a prompt
    let s = json!("hi");
    assert_eq!(prompt_content(Some(&s), &fake_resolve).unwrap().0, "hi");
}

#[test]
fn prompt_content_accepts_image_only_and_refuses_empty() {
    let only = json!([{"type": "image", "data": "AAA", "mimeType": "image/png"}]);
    let (text, images) = prompt_content(Some(&only), &fake_resolve).unwrap();
    assert!(text.is_empty());
    assert_eq!(images.len(), 1);
    for empty in [
        json!([]),
        json!([{"type": "text", "text": "  "}]),
        json!([{"type": "audio", "data": "BBB", "mimeType": "audio/wav"}]),
        json!(42),
    ] {
        assert_eq!(
            prompt_content(Some(&empty), &fake_resolve).unwrap_err(),
            "session/prompt: missing prompt",
            "{empty}"
        );
    }
    assert!(prompt_content(None, &fake_resolve).is_err());
}

#[test]
fn prompt_content_refuses_a_bad_image_instead_of_dropping_it() {
    let bad = json!([{"type": "text", "text": "x"}, {"type": "image", "data": "bad"}]);
    let e = prompt_content(Some(&bad), &fake_resolve).unwrap_err();
    assert!(e.contains("not png/jpeg/gif/webp"), "{e}");
    let no_data = json!([{"type": "text", "text": "x"}, {"type": "image"}]);
    let e = prompt_content(Some(&no_data), &fake_resolve).unwrap_err();
    assert!(e.contains("without base64"), "{e}");
}
