//! #212: images ride on the user message as durable references (never the
//! blob), survive replay, and reach the wire as `image_url` parts.

use serde_json::json;
use std::path::PathBuf;
use tole_core::cancel::CancelToken;
use tole_core::image::{self, ImageRef};
use tole_core::mock::MockProvider;
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::{Provider, ProviderOutput};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{run_turn_with_images, TurnOutcome};

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89,
];

fn ws(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-img-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

#[test]
fn user_message_records_the_reference_not_the_blob_and_replays() {
    let w = ws("turn");
    std::fs::write(w.join("shot.png"), PNG).unwrap();
    let r: ImageRef = image::resolve("shot.png", std::slice::from_ref(&w), None).unwrap();
    std::fs::create_dir_all(w.join("sess")).unwrap();
    let mut s = JsonlStorage::create(w.join("sess"), "s", None).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final { text: "ok".into() }]);
    let out = run_turn_with_images(
        &mut s,
        &mut p,
        &ToolRegistry::default(),
        "what is this?",
        std::slice::from_ref(&r),
        &CancelToken::default(),
        None,
    )
    .unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");

    let user = s
        .entries()
        .iter()
        .find(|e| e.payload["role"] == json!("user"))
        .unwrap();
    assert_eq!(user.payload["text"], json!("what is this?"));
    assert_eq!(user.payload["images"], json!([r.to_value()]));
    let f = std::fs::read_dir(w.join("sess"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let raw = std::fs::read_to_string(f.path()).unwrap();
    assert!(!raw.contains("iVBOR"), "the base64 blob must not be logged");

    // The wire built from the replayed transcript carries the image part.
    let prov = OpenAiProvider::new(OpenAiConfig::new("https://x.example/v1", "m", "k"))
        .with_image_roots(vec![w]);
    let body = prov.request_body(s.entries());
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0], json!({"type": "text", "text": "what is this?"}));
    assert_eq!(content[1]["type"], json!("image_url"));
    assert!(content[1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
}

#[test]
fn a_non_vision_model_with_images_is_a_loud_capability_error() {
    let w = ws("cap");
    std::fs::write(w.join("a.png"), PNG).unwrap();
    let r = image::resolve("a.png", std::slice::from_ref(&w), None).unwrap();
    std::fs::create_dir_all(w.join("sess")).unwrap();
    let mut s = JsonlStorage::create(w.join("sess"), "s", None).unwrap();
    // Nothing is listening: the capability check must fail BEFORE any request.
    let mut prov = OpenAiProvider::new(OpenAiConfig::new("http://127.0.0.1:1/v1", "m", "k"))
        .with_image_roots(vec![w])
        .with_vision(false);
    let _ = run_turn_with_images(
        &mut s,
        &mut prov,
        &ToolRegistry::default(),
        "look",
        &[r],
        &CancelToken::default(),
        None,
    );
    let e = prov.complete(s.entries()).unwrap_err();
    assert!(e.0.contains("model-capability"), "{e}");
    assert!(e.0.contains("image"), "{e}");
}
