//! The checks of the scenes: they drive the real offscreen window.

use super::super::scenes::amber_run_of;
use super::*;
use std::sync::Mutex;

/// One window at a time: each opens its own GPU device.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn scene_named(name: &str) -> Scene {
    scenes().into_iter().find(|s| s.name == name).expect(name)
}

fn focus_after(window: &mut Offscreen<Gallery>, key: &'static str) -> Option<PageStop> {
    window.press(key).expect("press");
    window.read(|v, w| v.focused(w)).expect("read")
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn every_scene_is_drawn_and_none_is_blank() {
    let _one = lock();
    let dir = std::env::temp_dir().join(format!(
        "tole-desktop-approvals-pages-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("dir");
    let rendered = render_scenes(&dir).expect("render");
    assert_eq!(rendered.len(), scenes().len());
    for r in &rendered {
        assert!(r.bytes > 1000 && r.colours() > 8, "{}", r.name);
    }
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn tab_walks_the_tabs_the_rows_and_the_cards_buttons_in_the_models_order() {
    let _one = lock();
    let mut window = open(scene_named("approvals-page-three-waiting")).expect("open");
    let want = window.read(|v, _| PageUi::stops(&v.view())).expect("stops");
    let mut seen = Vec::new();
    for _ in 0..want.len() {
        seen.push(focus_after(&mut window, "tab").expect("a stop"));
    }
    assert_eq!(seen, want);
    assert_eq!(&want[..2], [PageStop::TabWaiting, PageStop::TabDecided]);
    assert_eq!(
        &want[want.len() - 3..],
        [PageStop::DontAllow, PageStop::AllowOnce, PageStop::Details]
    );
    // Past the last stop it leaves the page (nothing else is in this window).
    // It wraps to the first stop, never onto an answer.
    assert_eq!(focus_after(&mut window, "tab"), Some(PageStop::TabWaiting));
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn a_key_that_starts_from_nothing_never_lands_on_an_answer() {
    let _one = lock();
    for (key, harmless) in [
        ("tab", PageStop::TabWaiting),
        ("shift-tab", PageStop::Details),
    ] {
        let mut window = open(scene_named("approvals-page-one-waiting")).expect("open");
        assert_eq!(focus_after(&mut window, key), Some(harmless), "{key}");
        window.tap("enter").expect("enter");
        let sent = window.read(|v, _| v.sent.len()).expect("read");
        assert_eq!(sent, 0, "{key} then Enter answered something");
    }
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn the_keyboard_never_follows_a_focused_answer_to_a_card_the_owner_did_not_read() {
    let _one = lock();
    let mut window = open(scene_named("approvals-page-one-waiting")).expect("open");
    for _ in 0..8 {
        if focus_after(&mut window, "tab") == Some(PageStop::AllowOnce) {
            break;
        }
    }
    assert_eq!(
        window.read(|v, w| v.focused(w)).unwrap(),
        Some(PageStop::AllowOnce)
    );
    // The card in front closes (a list without it) and another request
    // arrives, both before the next paint: the same buttons are on screen.
    let other = at(create_report(), "apr-20", 1);
    // One repaint for all three: they arrive between two paints.
    window
        .act(|v, cx| {
            v.state.apply(A::FetchStarted(Scope::All, FetchId(2)));
            v.state
                .apply(A::Snapshot(vec![], Scope::All, FetchId(2), NOW));
            v.tell(A::Requested(other, NOW), cx);
        })
        .expect("act");
    let shown = window
        .read(|v, _| v.state.selected().map(str::to_owned))
        .expect("read");
    let waiting = window.read(|v, _| v.state.waiting_count()).expect("read");
    assert_eq!(
        shown.as_deref(),
        Some("apr-20"),
        "the new request is picked ({waiting} waiting)"
    );
    let at = window.read(|v, w| v.focused(w)).expect("read");
    assert_ne!(
        at,
        Some(PageStop::AllowOnce),
        "focus moved onto the new card"
    );
    assert_ne!(at, Some(PageStop::DontAllow));
    window.tap("enter").expect("enter");
    window.tap("space").expect("space");
    let sent = window.read(|v, _| v.sent.clone()).expect("read");
    assert!(
        sent.is_empty(),
        "an answer went to a card nobody read: {sent:?}"
    );
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn enter_on_a_row_picks_it_and_on_a_tab_shows_that_tab() {
    let _one = lock();
    let mut window = open(scene_named("approvals-page-three-waiting")).expect("open");
    let picked = |w: &mut Offscreen<Gallery>| {
        w.read(|v, _| v.state.selected().map(str::to_owned))
            .expect("read")
    };
    let first = picked(&mut window).expect("the soonest is picked");
    for _ in 0..4 {
        window.press("tab").expect("tab"); // tabs, first row, second row
    }
    let at = window.read(|v, w| v.focused(w)).expect("read");
    assert!(matches!(at, Some(PageStop::Row(_))), "{at:?}");
    window.tap("enter").expect("enter");
    let second = picked(&mut window).expect("a request is picked");
    assert_ne!(first, second);
    // Back to the second tab (Shift+Tab walks the same order backwards).
    for _ in 0..4 {
        if window.read(|v, w| v.focused(w)).unwrap() == Some(PageStop::TabDecided) {
            break;
        }
        window.press("shift-tab").expect("back");
    }
    assert_eq!(
        window.read(|v, w| v.focused(w)).unwrap(),
        Some(PageStop::TabDecided)
    );
    window.tap("enter").expect("enter");
    let tab = window.read(|v, _| v.state.tab()).expect("read");
    assert_eq!(tab, Tab::Decided);
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn an_answer_is_sent_once_for_the_card_that_was_pressed() {
    let _one = lock();
    let mut window = open(scene_named("approvals-page-one-waiting")).expect("open");
    let stops = window.read(|v, _| PageUi::stops(&v.view())).expect("stops");
    let allow = stops
        .iter()
        .position(|s| *s == PageStop::AllowOnce)
        .expect("allow");
    for _ in 0..=allow {
        window.press("tab").expect("tab");
    }
    window.tap("enter").expect("enter");
    let sent = window.read(|v, _| v.sent.clone()).expect("read");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].id.as_str(), sent[0].decision),
        ("apr-1", Decision::AllowOnce)
    );
    // The card shows it is sending and offers no button: a second press
    // (a double click, Enter held down) reaches nothing, and sends nothing.
    for _ in 0..3 {
        window.tap("enter").expect("enter");
        window.press("tab").expect("tab");
    }
    window.tap("enter").expect("enter");
    let sent = window.read(|v, _| v.sent.len()).expect("read");
    assert_eq!(sent, 1);
    let buttons = window
        .read(|v, _| {
            PageUi::stops(&v.view())
                .iter()
                .any(|s| matches!(s, PageStop::DontAllow | PageStop::AllowOnce))
        })
        .expect("read");
    assert!(!buttons);
}

#[test]
fn the_technical_details_belong_to_one_card_and_toggle() {
    let state = |pick: &str| {
        let mut st = known(waiting_three(), vec![]);
        st.apply(A::Select(Some(pick.to_owned())));
        ApprovalsView::preview(&st, NOW, 0, Vec::new())
    };
    let (first, second) = (state("apr-1"), state("apr-4"));
    let mut ui = PageUi::default();
    assert!(!ui.details_open(&first));
    ui.toggle_details("apr-1");
    assert!(
        ui.details_open(&first),
        "open for the card it was opened on"
    );
    assert!(!ui.details_open(&second), "another card starts closed");
    ui.toggle_details("apr-1");
    assert!(!ui.details_open(&first), "a second press closes it");
    // Nothing picked: nothing is open, whatever was toggled.
    let none = ApprovalsView::preview(&known(vec![], vec![]), NOW, 0, Vec::new());
    ui.toggle_details("apr-1");
    assert!(!ui.details_open(&none));
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn a_request_whose_time_is_up_cannot_be_allowed_from_the_page() {
    let _one = lock();
    let mut window = open(scene_named("approvals-page-expired")).expect("open");
    // The model moved it to Decided: nothing waits, so no card is picked
    // on the Waiting tab and no button exists.
    let waiting = window.read(|v, _| v.state.waiting_count()).expect("read");
    assert_eq!(waiting, 0);
    for _ in 0..6 {
        window.press("tab").expect("tab");
        window.tap("enter").expect("enter");
    }
    let sent = window.read(|v, _| v.sent.len()).expect("read");
    assert_eq!(sent, 0);
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn a_listed_card_whose_time_is_up_is_drawn_without_buttons() {
    let _one = lock();
    let drawn = |name: &str| {
        let mut window = open(scene_named(name)).expect("open");
        let path =
            std::env::temp_dir().join(format!("tole-desktop-{}-{name}.png", std::process::id()));
        let (width, _, rgba) = window.write_png(&path).expect("png");
        (amber_run_of(width, &rgba), window)
    };
    // The request is still listed and picked, only its time is up.
    let (run, mut window) = drawn("approvals-page-expired-listed");
    let still_listed = window.read(|v, _| v.state.waiting_count()).expect("read");
    assert_eq!(still_listed, 1, "the scene must show the listed card");
    assert!(
        run < 120,
        "an amber button is drawn on a card whose time is up ({run})"
    );
    let stops = window.read(|v, _| PageUi::stops(&v.view())).expect("stops");
    assert!(!stops
        .iter()
        .any(|s| matches!(s, PageStop::DontAllow | PageStop::AllowOnce)));
    // The control: the same card with time left does draw the button.
    let (run, _) = drawn("approvals-page-one-waiting");
    assert!(run > 120, "the control scene has no button ({run})");
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "GPUI needs main thread (#1172)")]
fn a_target_of_1100_characters_is_cut_in_the_list_and_its_card_keeps_its_buttons() {
    let _one = lock();
    let mut window = open(scene_named("approvals-page-long-target")).expect("open");
    let path = std::env::temp_dir().join(format!("tole-desktop-{}-long.png", std::process::id()));
    let (width, _, rgba) = window.write_png(&path).expect("png");
    assert!(
        amber_run_of(width, &rgba) > 120,
        "no button on the card of a long target"
    );
    let stops = window.read(|v, _| PageUi::stops(&v.view())).expect("stops");
    assert!(stops.contains(&PageStop::AllowOnce));
}
