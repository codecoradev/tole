//! The Approvals page in the whole window, one PNG per state
//! (`shell-approvals-page-*.png`), and the checks that drive it through the real
//! window: a press must reach the app as a call with the id of what was
//! pressed (render harness only).

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::rc::Rc;

use super::super::adapter::offscreen::Offscreen;
use super::super::harness::{Rendered, WindowSize};
use super::super::shell::ShellView;
use super::super::status::{Source, Status};
use super::scenes::NOW;
use super::scenes_page::scenes as page_scenes;
use super::ApprovalsCall;
use crate::driver::approvals::ApprovalsView;
use tole_desktop_model::approvals::{ApprovalsAction as A, ApprovalsState};
use tole_desktop_model::{Action, Badge, OwnerAction, Section, ServeState, ShellState};

const BOARD: &str = "s5-approvals.png";

/// A source that shows a model state and applies the owner's calls to it with
/// the model's own rules, keeping what it was asked.
pub struct FakeSource {
    state: RefCell<ApprovalsState>,
    /// Every call the window made, in order.
    pub calls: RefCell<Vec<ApprovalsCall>>,
    /// Whether the app is not there to take a call.
    pub refuse: Cell<bool>,
}

impl FakeSource {
    pub fn new(state: ApprovalsState) -> Rc<Self> {
        Rc::new(FakeSource {
            state: RefCell::new(state),
            calls: RefCell::new(Vec::new()),
            refuse: Cell::new(false),
        })
    }
}

impl Source for FakeSource {
    fn changed(&self) -> bool {
        false
    }

    fn refresh(&self, shell: &mut ShellState) -> Status {
        let badge = self.state.borrow().waiting_badge();
        shell.apply(Action::SetApprovalsPending(badge));
        status()
    }

    fn owner_action(&self, _: OwnerAction) {}

    fn approvals(&self) -> ApprovalsView {
        ApprovalsView::preview(&self.state.borrow(), NOW, 0, Vec::new())
    }

    fn approvals_call(&self, call: ApprovalsCall) -> Result<(), super::ApprovalsRefused> {
        self.calls.borrow_mut().push(call.clone());
        if self.refuse.get() {
            return Err(super::ApprovalsRefused::NoServe);
        }
        let mut state = self.state.borrow_mut();
        match call {
            ApprovalsCall::Decide { id, decision } => {
                let _ = state.answer(&id, decision, NOW);
            }
            ApprovalsCall::Select(id) => {
                state.apply(A::Select(id));
            }
            ApprovalsCall::SelectTab(tab) => {
                state.apply(A::SelectTab(tab));
            }
        }
        Ok(())
    }
}

fn status() -> Status {
    Status {
        serve: ServeState::Running {
            version: "0.12.0".into(),
        },
        skills_unknown: false,
        actionable: true,
    }
}

fn shell(source: &FakeSource) -> ShellState {
    let mut state = ShellState::new();
    state.apply(Action::AddFolder {
        name: "Reports".to_owned(),
        path: "/home/me/Reports".to_owned(),
    });
    state.apply(Action::SelectSection(Section::Approvals));
    state.apply(Action::SetConnectionStatus(
        status().serve.connection_status(),
    ));
    state.apply(Action::SetApprovalsPending(
        source.state.borrow().waiting_badge(),
    ));
    state.apply(Action::SetSkillsNew(Badge::Count(0)));
    state
}

fn open(window: WindowSize, source: Rc<FakeSource>) -> Result<Offscreen<ShellView>, String> {
    let state = shell(&source);
    Offscreen::open(window.width, window.height, move |_cx| {
        ShellView::with_source(state, status(), Box::new(source))
    })
}

/// The states worth a look in the whole window, as (page scene, image): the
/// page scenes that show something the sidebar and header change the look of.
pub const SCENES: [(&str, &str); 4] = [
    (
        "approvals-page-one-waiting",
        "shell-approvals-page-one-waiting",
    ),
    ("approvals-page-decided", "shell-approvals-page-decided"),
    ("approvals-page-empty", "shell-approvals-page-empty"),
    ("approvals-page-unknown", "shell-approvals-page-unknown"),
];

pub fn render_scenes(dir: &Path, window: WindowSize) -> Result<Vec<Rendered>, String> {
    let mut done = Vec::new();
    for (page, file) in SCENES {
        let scene = page_scenes().into_iter().find(|s| s.name == page);
        let scene = scene.ok_or_else(|| format!("no page scene {page}"))?;
        let at = |what: &str, err: String| format!("{file}: {what}: {err}");
        let source = FakeSource::new(scene.state);
        let mut offscreen = open(window, source).map_err(|e| at("open", e))?;
        let path = dir.join(format!("{file}.png"));
        let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
        let bytes = fs::metadata(&path)
            .map_err(|e| at("stat", e.to_string()))?
            .len();
        done.push(Rendered {
            name: file,
            board: BOARD,
            keys: "",
            path,
            width,
            height,
            bytes,
            rgba,
        });
    }
    let index = dir.join("index.tsv");
    if index.exists() {
        let mut out = fs::OpenOptions::new()
            .append(true)
            .open(&index)
            .map_err(|e| format!("{}: {e}", index.display()))?;
        for r in &done {
            let line = format!(
                "{}.png\t{}\t{}\t{}\t{}\t{}",
                r.name, r.board, r.keys, r.width, r.height, r.bytes
            );
            writeln!(out, "{line}").map_err(|e| format!("{}: {e}", index.display()))?;
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
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
}
