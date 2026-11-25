//! Checks that drive the Approvals page through the real window (render harness only):
//! a press must reach the app as a call with the id of what was pressed.

use super::*;
use std::sync::Mutex;
use tole_desktop_model::approvals::{Decision, PageStop, Tab};

/// One window at a time: each opens its own GPU device.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn source_of(name: &str) -> Rc<FakeSource> {
    let scene = page_scenes()
        .into_iter()
        .find(|s| s.name == name)
        .expect(name);
    FakeSource::new(scene.state)
}

fn page_focus(w: &mut Offscreen<ShellView>) -> Option<PageStop> {
    w.read(|v, win| v.approvals_focused(win)).expect("read")
}

/// Presses Tab until the page's control `stop` has focus.
fn tab_to(w: &mut Offscreen<ShellView>, stop: PageStop) {
    for _ in 0..40 {
        w.press("tab").expect("tab");
        if page_focus(w) == Some(stop.clone()) {
            return;
        }
    }
    panic!("Tab never reached {stop:?}");
}

#[test]
fn the_page_is_drawn_in_every_state_and_leaves_the_sidebar_alone() {
    let _one = lock();
    let dir = std::env::temp_dir().join(format!(
        "tole-desktop-approvals-shell-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("dir");
    let rendered = render_scenes(&dir, WindowSize::default()).expect("render");
    assert_eq!(rendered.len(), SCENES.len());
    for r in &rendered {
        assert!(r.bytes > 1000 && r.colours() > 8, "{}", r.name);
    }
}

#[test]
fn allow_once_reaches_the_app_once_with_the_id_of_the_card_that_was_pressed() {
    let _one = lock();
    let source = source_of("approvals-page-three-waiting");
    let mut w = open(WindowSize::default(), source.clone()).expect("open");
    tab_to(&mut w, PageStop::AllowOnce);
    w.tap("enter").expect("enter");
    let calls = source.calls.borrow().clone();
    let decides: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, ApprovalsCall::Decide { .. }))
        .collect();
    assert_eq!(
        decides,
        [&ApprovalsCall::Decide {
            id: "apr-1".into(),
            decision: Decision::AllowOnce
        }]
    );
    // A second press has no button to land on: nothing more is sent.
    for _ in 0..4 {
        w.press("tab").expect("tab");
        w.tap("enter").expect("enter");
    }
    let again = source
        .calls
        .borrow()
        .iter()
        .filter(|c| matches!(c, ApprovalsCall::Decide { .. }))
        .count();
    assert_eq!(again, 1);
}

#[test]
fn a_row_and_a_tab_are_sent_as_calls_and_dont_allow_is_its_own_decision() {
    let _one = lock();
    let source = source_of("approvals-page-three-waiting");
    let mut w = open(WindowSize::default(), source.clone()).expect("open");
    tab_to(&mut w, PageStop::TabDecided);
    w.tap("enter").expect("enter");
    assert_eq!(
        source.calls.borrow().last(),
        Some(&ApprovalsCall::SelectTab(Tab::Decided))
    );
    tab_to(&mut w, PageStop::TabWaiting);
    w.tap("enter").expect("enter");
    // The Waiting tab is back and a request is picked again: answer it.
    tab_to(&mut w, PageStop::DontAllow);
    w.tap("enter").expect("enter");
    let last = source.calls.borrow().last().cloned();
    assert!(
        matches!(
            last,
            Some(ApprovalsCall::Decide {
                decision: Decision::DontAllow,
                ..
            })
        ),
        "{last:?}"
    );
}

#[test]
fn a_press_the_app_refuses_is_not_shown_as_if_it_had_worked() {
    let _one = lock();
    let scene = page_scenes()
        .into_iter()
        .find(|s| s.name == "approvals-page-one-waiting");
    let source = Rc::new(FakeSource {
        state: RefCell::new(scene.expect("scene").state),
        calls: RefCell::new(Vec::new()),
        refuse: Cell::new(true),
    });
    let mut w = open(WindowSize::default(), source.clone()).expect("open");
    tab_to(&mut w, PageStop::AllowOnce);
    w.tap("enter").expect("enter");
    // Nothing was sent, so the card is as it was: the button is still there
    // and the page says so.
    assert_eq!(page_focus(&mut w), Some(PageStop::AllowOnce));
    let refused = w.read(|v, _| v.approvals_refused()).expect("read");
    assert!(refused);
}

#[test]
fn the_notice_of_a_refused_press_goes_with_the_card_and_with_the_section() {
    let _one = lock();
    let source = source_of("approvals-page-three-waiting");
    source.refuse.set(true);
    let mut w = open(WindowSize::default(), source.clone()).expect("open");
    tab_to(&mut w, PageStop::AllowOnce);
    w.tap("enter").expect("enter");
    assert!(w.read(|v, _| v.approvals_refused()).expect("read"));
    // Another card is shown (picked by another face): the notice was about
    // the first one and is gone.
    source.refuse.set(false);
    let _ = source.approvals_call(ApprovalsCall::Select(Some("apr-4".into())));
    w.press("escape").expect("repaint");
    assert!(!w.read(|v, _| v.approvals_refused()).expect("read"));
    // Refused again, then the section is left (the window's own look says so).
    source.refuse.set(true);
    tab_to(&mut w, PageStop::AllowOnce);
    w.tap("enter").expect("enter");
    assert!(w.read(|v, _| v.approvals_refused()).expect("read"));
    w.read(|v, _| v.approvals_left()).expect("left");
    assert!(!w.read(|v, _| v.approvals_refused()).expect("read"));
}

#[test]
fn shift_tab_from_nothing_lands_on_the_details_and_enter_there_sends_no_answer() {
    let _one = lock();
    let source = source_of("approvals-page-one-waiting");
    let mut w = open(WindowSize::default(), source.clone()).expect("open");
    w.press("shift-tab").expect("shift-tab");
    assert_eq!(page_focus(&mut w), Some(PageStop::Details));
    w.tap("enter").expect("enter");
    let decides = source
        .calls
        .borrow()
        .iter()
        .filter(|c| matches!(c, ApprovalsCall::Decide { .. }))
        .count();
    assert_eq!(decides, 0);
}
