//! The Approvals page on its own, one PNG per state (`approvals-page-*.png`),
//! and the checks that drive it through the real window (render harness only).
//!
//! The host is a bare view over the model's own [`ApprovalsState`]: a press
//! goes through the model's reducers and rules, as it does behind the real
//! window, and the intents the model hands out are what a test reads.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use super::super::adapter::offscreen::Offscreen;
use super::super::adapter::*;
use super::super::harness::{Rendered, WindowSize};
use super::scenes::{
    create_report, from_helper, long_target, outside_folder, replace_notes, run_command, NOW,
};
use super::{approvals_page, CardHost, PageHost, PageUi};
use crate::driver::approvals::ApprovalsView;
use tole_desktop_model::approvals::{
    ApprovalRequest, ApprovalsAction as A, ApprovalsState, Decision, DecisionIntent, FetchId,
    PageStop, Scope, Tab,
};
use tole_desktop_model::tokens::{font, DARK};

/// The board the page is compared with.
const BOARD: &str = "s5-approvals.png";
/// The main area of the window of board S5 (1440 by 900, less the 264 px sidebar).
const PAGE: WindowSize = WindowSize {
    width: 1176.0,
    height: 860.0,
};

/// What a scene shows: the model's state and what the driver would say beside it.
pub struct Scene {
    pub name: &'static str,
    pub keys: &'static str,
    pub state: ApprovalsState,
    pub now: u64,
    pub fetch_failures: u32,
    pub parked: Vec<String>,
    /// Whether the model's clock is moved to `now` before the first frame (a
    /// request whose time is up is then already Decided). A scene that must
    /// show a card whose time ran out while it is still listed sets it false.
    pub tick: bool,
}

/// A queue that was fetched once: `waiting` and `decided` are what the server listed.
pub fn known(waiting: Vec<ApprovalRequest>, decided: Vec<ApprovalRequest>) -> ApprovalsState {
    let mut state = ApprovalsState::new();
    state.apply(A::FetchStarted(Scope::All, FetchId(1)));
    let rows = waiting.into_iter().chain(decided).collect();
    state.apply(A::Snapshot(rows, Scope::All, FetchId(1), NOW));
    state
}

fn at(mut r: ApprovalRequest, id: &str, minutes_ago: u64) -> ApprovalRequest {
    r.id = id.into();
    r.created_at_ms = Some(NOW - minutes_ago * 60_000);
    r
}

fn ended(mut r: ApprovalRequest, id: &str, status: &str, minutes_ago: u64) -> ApprovalRequest {
    r.status = status.into();
    at(r, id, minutes_ago)
}

fn waiting_three() -> Vec<ApprovalRequest> {
    vec![
        at(create_report(), "apr-1", 3),
        at(run_command(), "apr-4", 2),
        at(outside_folder(), "apr-5", 1),
    ]
}

fn decided_three() -> Vec<ApprovalRequest> {
    vec![
        ended(replace_notes(), "apr-8", "approved", 30),
        ended(run_command(), "apr-9", "denied", 40),
        ended(from_helper(), "dap-10", "expired", 50),
    ]
}

fn scene(name: &'static str, state: ApprovalsState) -> Scene {
    Scene {
        name,
        keys: "",
        state,
        now: NOW,
        fetch_failures: 0,
        parked: Vec::new(),
        tick: true,
    }
}

pub fn scenes() -> Vec<Scene> {
    let one = known(vec![create_report()], decided_three());
    let mut decided = known(waiting_three(), decided_three());
    decided.apply(A::SelectTab(Tab::Decided));
    let mut unselected = known(waiting_three(), vec![]);
    unselected.apply(A::Select(None));
    // A frame arrived and no list has been read: what is listed is certain,
    // the rest is not.
    let mut partial = ApprovalsState::new();
    partial.apply(A::Requested(create_report(), NOW));
    // A row the server sent that this app could not read.
    let mut refused = known(vec![create_report()], vec![]);
    refused.apply(A::Requested(
        ApprovalRequest::new("bad id!", "write_file", "pending"),
        NOW,
    ));
    let mut unreachable = scene("approvals-page-unreachable", ApprovalsState::new());
    unreachable.fetch_failures = 3;
    let mut parked = scene("approvals-page-parked", known(waiting_three(), vec![]));
    parked.parked = vec!["sess-9".into()];
    let mut focused = scene("approvals-page-row-focused", known(waiting_three(), vec![]));
    focused.keys = "tab tab tab";
    let mut late = scene(
        "approvals-page-expired",
        known(vec![create_report()], vec![]),
    );
    late.now = NOW + 901_000;
    // The same, seen before the model's clock has moved the request: it is
    // still listed, and its card must say the time is up and offer no answer.
    let mut late_listed = scene(
        "approvals-page-expired-listed",
        known(vec![create_report()], vec![]),
    );
    late_listed.now = NOW + 901_000;
    late_listed.tick = false;
    let long = at(long_target(), "apr-7", 1);
    vec![
        scene("approvals-page-one-waiting", one),
        scene(
            "approvals-page-three-waiting",
            known(waiting_three(), vec![]),
        ),
        focused,
        scene("approvals-page-decided", decided),
        scene("approvals-page-none-picked", unselected),
        scene("approvals-page-empty", known(vec![], vec![])),
        scene("approvals-page-unknown", ApprovalsState::new()),
        unreachable,
        scene("approvals-page-partial", partial),
        scene("approvals-page-refused", refused),
        parked,
        late,
        late_listed,
        scene("approvals-page-long-target", known(vec![long], vec![])),
    ]
}

/// A bare host of the page over a model state.
pub struct Gallery {
    pub state: ApprovalsState,
    ui: PageUi,
    now: u64,
    fetch_failures: u32,
    parked: Vec<String>,
    /// What the model let through: the answers a real host would send.
    pub sent: Vec<DecisionIntent>,
}

impl Gallery {
    fn new(scene: Scene) -> Self {
        let mut state = scene.state;
        // A tick makes the clock the model's own, so a late scene has expired.
        if scene.tick {
            state.apply(A::Tick(scene.now));
        }
        Gallery {
            state,
            ui: PageUi::default(),
            now: scene.now,
            fetch_failures: scene.fetch_failures,
            parked: scene.parked,
            sent: Vec::new(),
        }
    }

    /// Something the model is told between two frames (a frame from the
    /// server, a list): repaints.
    pub fn tell(&mut self, action: A, cx: &mut Context<Self>) {
        self.state.apply(action);
        cx.notify();
    }

    fn view(&self) -> ApprovalsView {
        let parked = self.parked.clone();
        ApprovalsView::preview(&self.state, self.now, self.fetch_failures, parked)
    }

    fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.state.apply(A::Select(Some(id.to_owned())));
        cx.notify();
    }

    fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.state.apply(A::SelectTab(tab));
        cx.notify();
    }

    fn answer(&mut self, id: &str, decision: Decision, cx: &mut Context<Self>) {
        if let Ok(intent) = self.state.answer(id, decision, self.now) {
            self.sent.push(intent);
        }
        cx.notify();
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        self.ui.toggle_details(id);
        cx.notify();
    }

    /// The control that has focus (for the checks).
    pub fn focused(&self, window: &Window) -> Option<PageStop> {
        let stops = PageUi::stops(&self.view());
        self.ui.focus.focused(&stops, window)
    }
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        let view = self.view();
        self.ui.sync(&view, cx);
        let host = PageHost {
            card: CardHost {
                answer: Gallery::answer,
                toggle_details: Gallery::toggle,
            },
            select: Gallery::select,
            select_tab: Gallery::select_tab,
        };
        div()
            .size_full()
            .bg(color(p.background))
            .text_color(color(p.text_secondary))
            .font_family(font::FAMILY_UI)
            .text_size(len(font::BODY))
            .child(approvals_page(&view, &self.ui, host, cx))
    }
}

fn open(scene: Scene) -> Result<Offscreen<Gallery>, String> {
    Offscreen::open(PAGE.width, PAGE.height, move |_cx| Gallery::new(scene))
}

/// Renders every scene into `dir` (which must exist) and adds a line for each
/// to `index.tsv`, when there is one.
pub fn render_scenes(dir: &Path) -> Result<Vec<Rendered>, String> {
    let mut done = Vec::new();
    for scene in scenes() {
        let (name, keys) = (scene.name, scene.keys);
        let at = |what: &str, err: String| format!("{name}: {what}: {err}");
        let mut offscreen = open(scene).map_err(|e| at("open", e))?;
        for key in keys.split_whitespace() {
            offscreen.press(key).map_err(|e| at("press", e))?;
        }
        let path = dir.join(format!("{name}.png"));
        let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
        let bytes = fs::metadata(&path)
            .map_err(|e| at("stat", e.to_string()))?
            .len();
        done.push(Rendered {
            name,
            board: BOARD,
            keys,
            path,
            width,
            height,
            bytes,
            rgba,
        });
    }
    let index = dir.join("index.tsv");
    if index.exists() {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&index)
            .map_err(|e| format!("{}: {e}", index.display()))?;
        for r in &done {
            let line = format!(
                "{}.png\t{}\t{}\t{}\t{}\t{}",
                r.name, r.board, r.keys, r.width, r.height, r.bytes
            );
            writeln!(file, "{line}").map_err(|e| format!("{}: {e}", index.display()))?;
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests;
