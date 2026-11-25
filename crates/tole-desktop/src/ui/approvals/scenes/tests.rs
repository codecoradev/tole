//! The checks of the scenes: they drive the real offscreen window.

use super::*;
use std::sync::Mutex;
use tole_desktop_model::approvals::Button;

/// One window at a time: each opens its own GPU device.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn amber_run(r: &Rendered) -> usize {
    amber_run_of(r.width, &r.rgba)
}

fn dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tole-desktop-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("dir");
    dir
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn every_scene_is_drawn_and_only_a_waiting_card_has_the_amber_button() {
    let _one = lock();
    let rendered = render_scenes(&dir("approvals-cards")).expect("render");
    assert_eq!(rendered.len(), scenes().len());
    for r in &rendered {
        assert!(r.bytes > 1000 && r.colours() > 8, "{}", r.name);
        let answerable = !["expired", "answering", "allowed"]
            .iter()
            .any(|s| r.name.ends_with(s));
        let button = amber_run(r) > 120; // "Allow once" is wider than 60 px, drawn at 2x
        assert_eq!(button, answerable, "{}: a run of {}", r.name, amber_run(r));
    }
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn tab_reaches_the_buttons_in_order_and_enter_answers_the_card_that_was_pressed() {
    let _one = lock();
    let card = card_of(&create_report(), CardState::Waiting, NOW);
    let mut window = open(card, false).expect("open");
    let mut seen = Vec::new();
    for _ in 0..3 {
        window.press("tab").expect("tab");
        seen.push(window.read(|v, w| v.focused(w)).expect("read"));
    }
    // The answers come first and the details last: the last stop is never an answer.
    assert_eq!(
        seen,
        [
            Some(PageStop::DontAllow),
            Some(PageStop::AllowOnce),
            Some(PageStop::Details)
        ]
    );
    window.press("shift-tab").expect("back");
    window.tap("enter").expect("enter");
    window.press("shift-tab").expect("back");
    window.tap("enter").expect("enter");
    let answered = window.read(|v, _| v.answered.clone()).expect("read");
    assert_eq!(
        answered,
        [
            ("apr-1".to_owned(), Button::AllowOnce.decision()),
            ("apr-1".to_owned(), Button::DontAllow.decision())
        ]
    );
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn a_card_that_cannot_be_answered_has_nothing_to_press() {
    let _one = lock();
    let late = card_of(&create_report(), CardState::Waiting, NOW + 901_000);
    let mut window = open(late, false).expect("open");
    for _ in 0..4 {
        window.press("tab").expect("tab");
        let at = window.read(|v, w| v.focused(w)).expect("read");
        assert!(matches!(at, None | Some(PageStop::Details)), "{at:?}");
        window.tap("enter").expect("enter");
    }
    let answered = window.read(|v, _| v.answered.len()).expect("read");
    assert_eq!(answered, 0);
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn the_technical_details_open_from_the_keyboard() {
    let _one = lock();
    let card = card_of(&delete_invoice(), CardState::Waiting, NOW);
    let mut window = open(card, false).expect("open");
    for _ in 0..3 {
        window.press("tab").expect("tab"); // Don't allow, Allow once, the details
    }
    window.tap("enter").expect("enter");
    let open = window.read(|v, _| v.details_open).expect("read");
    assert!(open);
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn shift_tab_from_nothing_lands_on_the_details_and_never_on_an_answer() {
    let _one = lock();
    let card = card_of(&create_report(), CardState::Waiting, NOW);
    let mut window = open(card, false).expect("open");
    window.press("shift-tab").expect("shift-tab");
    let at = window.read(|v, w| v.focused(w)).expect("read");
    assert_eq!(at, Some(PageStop::Details));
    window.tap("enter").expect("enter");
    let answered = window.read(|v, _| v.answered.len()).expect("read");
    assert_eq!(answered, 0, "Enter there only opens the details");
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn a_hostile_target_is_escaped_where_it_is_drawn_and_the_buttons_stay_reachable() {
    let card = card_of(&hostile_target(), CardState::Waiting, NOW);
    let everything = std::iter::once(card.subject.clone())
        .chain(card.preview.lines.iter().map(|l| l.text.clone()))
        .chain(card.details.iter().map(|d| d.value.clone()));
    for text in everything {
        for c in ['\u{202e}', '\u{7}', '\n', '\u{200b}', '\u{2066}'] {
            assert!(!text.contains(c), "{c:?} in {text:?}");
        }
    }
    assert!(card.subject.contains("\\u{202e}"), "{}", card.subject);
    let _one = lock();
    let mut window = open(card, false).expect("open");
    window.press("tab").expect("tab");
    window.press("tab").expect("tab");
    let at = window.read(|v, w| v.focused(w)).expect("read");
    assert_eq!(at, Some(PageStop::AllowOnce));
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn a_target_of_1100_characters_and_open_details_leave_the_buttons_on_screen() {
    let _one = lock();
    for (name, details) in [("long", false), ("long-details", true)] {
        let card = card_of(&long_target(), CardState::Waiting, NOW);
        assert_eq!(
            card.subject.chars().count(),
            1100,
            "the display bound, uncut"
        );
        let mut window = open(card, details).expect("open");
        let path = dir(&format!("approvals-{name}")).join("card.png");
        let (width, height, rgba) = window.write_png(&path).expect("png");
        // The button is drawn, whole and inside the window: a wide amber
        // block, and the page was not pushed wider than the window.
        assert!(amber_run_of(width, &rgba) > 120, "{name}: no button drawn");
        let right = (width as usize - 4) * 4;
        let edge_clear = (0..height as usize).all(|y| {
            let at = y * width as usize * 4 + right;
            rgba[at..at + 3]
                == [
                    DARK.background.r(),
                    DARK.background.g(),
                    DARK.background.b(),
                ]
        });
        assert!(edge_clear, "{name}: something runs to the right edge");
    }
}
