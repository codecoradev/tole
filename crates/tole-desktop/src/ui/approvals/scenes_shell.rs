//! The Approvals page in the whole window, one PNG per state
//! (`shell-approvals-page-*.png`), and the source that stands in for the app
//! (render harness only).

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
