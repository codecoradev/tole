//! The checks of the scenes: they drive the real offscreen window.

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

fn focus_after(window: &mut Offscreen<Gallery>, key: &'static str) -> Option<AutomationsStop> {
    window.press(key).expect("press");
    window.read(|v, w| v.focused(w)).expect("read")
}

#[test]
fn every_scene_is_drawn_and_none_is_blank() {
    let _one = lock();
    let dir = std::env::temp_dir().join(format!("tole-desktop-automations-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("dir");
    let rendered = render_scenes(&dir).expect("render");
    assert_eq!(rendered.len(), scenes().len());
    for r in &rendered {
        assert!(r.bytes > 1000 && r.colours() > 8, "{}", r.name);
    }
}

#[test]
fn tab_walks_the_rows_and_their_switches_then_the_links_in_the_models_order() {
    let _one = lock();
    let mut window = open(scene_named("automations-page-history")).expect("open");
    let want = window
        .read(|v, _| AutomationsUi::stops(&v.view))
        .expect("stops");
    assert!(want.len() > 8, "{want:?}");
    let mut seen = Vec::new();
    for _ in 0..want.len() {
        seen.push(focus_after(&mut window, "tab").expect("a stop"));
    }
    assert_eq!(seen, want);
    assert_eq!(want[0], AutomationsStop::Row(AutomationId::new("nightly")));
    assert_eq!(
        want[1],
        AutomationsStop::Switch(AutomationId::new("nightly"))
    );
    assert_eq!(
        want.last(),
        Some(&AutomationsStop::OpenInApprovals("f2".into()))
    );
    // Past the last stop it wraps to the first, and nothing was pressed.
    assert_eq!(focus_after(&mut window, "tab"), Some(want[0].clone()));
    let (toggled, opened) = window
        .read(|v, _| (v.toggled.len(), v.opened.len()))
        .expect("read");
    assert_eq!((toggled, opened), (0, 0));
}

#[test]
fn enter_on_a_row_picks_it() {
    let _one = lock();
    let mut window = open(scene_named("automations-page-table")).expect("open");
    for _ in 0..3 {
        window.press("tab").expect("tab"); // row, switch, next row
    }
    assert_eq!(
        window.read(|v, w| v.focused(w)).unwrap(),
        Some(AutomationsStop::Row(AutomationId::new("cleanup")))
    );
    window.tap("enter").expect("enter");
    let picked = window
        .read(|v, _| v.view.page.selected.clone())
        .expect("read");
    assert_eq!(picked, Some(AutomationId::new("cleanup")));
}

#[test]
fn enter_on_a_switch_reports_that_row_and_nothing_else() {
    let _one = lock();
    let mut window = open(scene_named("automations-page-table")).expect("open");
    for _ in 0..2 {
        window.press("tab").expect("tab"); // row, switch
    }
    assert_eq!(
        window.read(|v, w| v.focused(w)).unwrap(),
        Some(AutomationsStop::Switch(AutomationId::new("nightly")))
    );
    window.tap("enter").expect("enter");
    let toggled = window.read(|v, _| v.toggled.clone()).expect("read");
    assert_eq!(toggled, vec![AutomationId::new("nightly")]);
    let picked = window
        .read(|v, _| v.view.page.selected.clone())
        .expect("read");
    assert_eq!(picked, None, "a switch does not pick its row");
}

#[test]
fn enter_on_the_link_of_a_run_that_stopped_to_ask_opens_that_request() {
    let _one = lock();
    let mut window = open(scene_named("automations-page-history")).expect("open");
    let stops = window
        .read(|v, _| AutomationsUi::stops(&v.view))
        .expect("stops");
    for _ in 0..stops.len() {
        window.press("tab").expect("tab");
    }
    assert_eq!(
        window.read(|v, w| v.focused(w)).unwrap(),
        Some(AutomationsStop::OpenInApprovals("f2".into()))
    );
    window.tap("enter").expect("enter");
    let opened = window.read(|v, _| v.opened.clone()).expect("read");
    assert_eq!(opened, vec!["apr-1".to_owned()]);
}

#[test]
fn a_run_whose_request_is_not_known_has_no_link_to_reach() {
    let _one = lock();
    let mut window = open(scene_named("automations-page-history-partial")).expect("open");
    let stops = window
        .read(|v, _| AutomationsUi::stops(&v.view))
        .expect("stops");
    assert!(
        !stops
            .iter()
            .any(|s| matches!(s, AutomationsStop::OpenInApprovals(_))),
        "{stops:?}"
    );
}
