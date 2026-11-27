//! Bracketed paste and image attachments.

use crate::mock::{spawn_mock_recording, Bodies};
use crate::pty::{workdir, Pty};
use serde_json::{json, Value};
use std::time::Duration;
/// The user messages of one recorded request, as the provider saw them.
pub fn user_contents(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].clone())
        .collect()
}

/// #560 regression. Before the fix the TUI never asked for bracketed paste,
/// so a multi-line paste arrived as keys: its first line was SENT at the
/// first CR, the next line steered that turn, and the rest was left in the
/// editor; a paste wrapped in bracketed-paste markers was dropped entirely.
/// Now the terminal is asked for bracketed paste, the block lands in the
/// editor as one edit (CRLF/CR become LF, control chars are dropped) and
/// nothing is sent until Enter.
#[test]
fn tui_bracketed_paste_is_one_block_that_waits_for_enter() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    assert!(
        String::from_utf8_lossy(&t.out.lock().unwrap()).contains("\x1b[?2004h"),
        "the TUI enables bracketed paste"
    );

    // What a terminal in bracketed-paste mode sends for a 3-line paste.
    t.send("\x1b[200~line-one\r\nline-two\rline-\x07three\x1b[201~");
    t.expect("line-three");
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        bodies.lock().unwrap().is_empty(),
        "a paste never submits: {:?}",
        bodies.lock().unwrap()
    );

    t.send("\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    {
        let b = bodies.lock().unwrap();
        assert_eq!(b.len(), 1, "one message, sent by Enter");
        assert_eq!(
            user_contents(&b[0]),
            vec![json!("line-one\nline-two\nline-three")]
        );
    }
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert!(
        String::from_utf8_lossy(&t.out.lock().unwrap()).contains("\x1b[?2004l"),
        "bracketed paste is disabled again on exit"
    );
}

/// A 1x1 PNG (the host sniffs the bytes, so it must be a real one).
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// Whether a user message content carries an image part.
fn has_image(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|parts| parts.iter().any(|p| p["type"] == "image_url"))
}

/// #560: a pasted image path is attached (shown as a row, sent with the next
/// message only); `/image` outside the jail is refused with the host's error;
/// `/image clear` drops the queue; a non-image paste stays text.
#[test]
fn tui_attaches_pasted_and_slash_image_paths_to_the_next_message() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    std::fs::write(dir.join("shot.png"), PNG_1X1).unwrap();
    let outside = dir
        .parent()
        .unwrap()
        .join(format!("tole-e2e-outside-{}.png", std::process::id()));
    std::fs::write(&outside, PNG_1X1).unwrap();
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");

    // A pasted path to an image inside the workspace: attached, not inserted.
    t.send("\x1b[200~shot.png\x1b[201~");
    t.expect("image attached to your next message: shot.png (image/png, 67 B)");
    t.expect("image: shot.png (image/png, 67 B) · next message");
    t.send("look\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    // The next message goes without it.
    t.send("again\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // Outside the jail: refused with the host's error, nothing queued.
    t.send(&format!("/image {}\r", outside.display()));
    t.expect_unwrapped("outside the workspace jail");
    // A pasted path outside the jail is text after all (and says why).
    t.send("see ");
    t.send(&format!("\x1b[200~{}\x1b[201~", outside.display()));
    t.expect_unwrapped("pasted as text, not attached");
    t.send("\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // `/image` + `/image clear`: the next message carries nothing.
    t.send("/image shot.png\r");
    t.expect("image: shot.png");
    t.send("/image clear\r");
    t.expect("(1 queued image(s) dropped)");
    t.send("plain\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let _ = std::fs::remove_file(&outside);

    let b = bodies.lock().unwrap();
    assert_eq!(b.len(), 4, "{b:?}");
    let last_user = |i: usize| user_contents(&b[i]).last().cloned().unwrap();
    assert!(
        has_image(&last_user(0)),
        "the paste rode on `look`: {:?}",
        b[0]
    );
    assert!(!has_image(&last_user(1)), "only on the next message");
    assert_eq!(
        last_user(2),
        json!(format!("see {}", outside.display())),
        "a refused path was sent as the text it was"
    );
    assert!(!has_image(&last_user(3)), "`/image clear` dropped it");
}

/// #560: with a model declared non-vision, an attached image gives the same
/// model-capability error as `--image`; it surfaces as the turn's stop note.
#[test]
fn tui_image_on_a_non_vision_model_reports_the_capability_error() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    std::fs::write(dir.join("shot.png"), PNG_1X1).unwrap();
    let mut t = Pty::spawn_full(&dir, &url, &[], &[("TOLE_VISION", "0")]);
    t.expect("effort:");
    t.send("/image shot.png\r");
    t.expect("image: shot.png");
    t.send("look\r");
    t.expect("model-capability error");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert!(
        bodies.lock().unwrap().is_empty(),
        "refused before any request"
    );
}
