//! The Automations page on its own, one PNG per state (`automations-page-*.png`,
//! compared with board S6; render harness only).
//!
//! The host is a bare view over the driver's own [`AutomationsView`]: a press
//! goes through the model's reducers, as it does behind the real window.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use super::super::adapter::offscreen::Offscreen;
use super::super::adapter::*;
use super::super::harness::{Rendered, WindowSize};
use super::page::automations_page;
use super::types::{AutomationsHost, AutomationsUi};
use crate::driver::automations::AutomationsView;
use tole_desktop_model::tokens::{font, DARK};
use tole_desktop_model::{AutomationId, RawJob, Scheduler};

/// The board the page is compared with.
const BOARD: &str = "s6-automations.png";
/// The main area of the window of board S6 (1440 by 900, less the 264 px sidebar).
const PAGE: WindowSize = WindowSize {
    width: 1176.0,
    height: 860.0,
};

/// What a scene shows: the view the driver would give.
pub struct Scene {
    pub name: &'static str,
    pub view: AutomationsView,
}

fn job(id: &str, name: &str, schedule: &str, state: &str, last: Option<&str>) -> RawJob {
    RawJob {
        id: id.into(),
        name: name.into(),
        schedule: schedule.into(),
        timezone: "UTC".into(),
        state: state.into(),
        next_run: (state == "active").then_some(1_795_000_000),
        next: "2026-11-25 02:00 UTC".into(),
        last: last.map(str::to_owned),
        missed: 0,
        blocked: 0,
        created_by: "cli".into(),
        awaiting_review: false,
        folder: Some("/home/me/Reports".into()),
    }
}

/// Four jobs: one that passed, one that failed and is paused, one that stopped
/// to ask, and one Tole made that waits for the owner.
pub fn jobs() -> Vec<RawJob> {
    let mut asking = job(
        "backups",
        "Check backups",
        "30 3 * * *",
        "active",
        Some("awaiting_approval"),
    );
    asking.missed = 2;
    let mut made = job("sorter", "Inbox sorter", "0 8 * * 1-5", "paused", None);
    made.created_by = "agent".into();
    made.awaiting_review = true;
    vec![
        job(
            "nightly",
            "Nightly report",
            "0 2 * * *",
            "active",
            Some("ok"),
        ),
        job(
            "cleanup",
            "Weekly cleanup",
            "0 9 * * 1",
            "paused",
            Some("error"),
        ),
        asking,
        made,
    ]
}

/// A list that was read.
pub fn read(jobs: Vec<RawJob>) -> AutomationsView {
    let mut view = AutomationsView::not_loaded();
    view.page.loaded(jobs, Scheduler::new(true, Some("serve")));
    view
}

pub fn scenes() -> Vec<Scene> {
    vec![
        Scene {
            name: "automations-page-table",
            view: read(jobs()),
        },
        Scene {
            name: "automations-page-unknown",
            view: AutomationsView::not_loaded(),
        },
        Scene {
            name: "automations-page-empty",
            view: read(Vec::new()),
        },
    ]
}

/// A bare host of the page over a view.
pub struct Gallery {
    pub view: AutomationsView,
    ui: AutomationsUi,
}

impl Gallery {
    fn new(scene: Scene) -> Self {
        Gallery {
            view: scene.view,
            ui: AutomationsUi::default(),
        }
    }

    fn select(&mut self, id: &AutomationId, cx: &mut Context<Self>) {
        self.view.page.select(id);
        cx.notify();
    }

    fn toggle(&mut self, _: &AutomationId, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.view.page.dismiss_notice();
        cx.notify();
    }

    fn open_approvals(&mut self, _: &str, cx: &mut Context<Self>) {
        cx.notify();
    }
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = DARK;
        self.ui.sync(&self.view, cx);
        let host = AutomationsHost {
            select: Gallery::select,
            toggle: Gallery::toggle,
            dismiss_notice: Gallery::dismiss,
            open_in_approvals: Gallery::open_approvals,
        };
        div()
            .size_full()
            .bg(color(p.background))
            .text_color(color(p.text_secondary))
            .font_family(font::FAMILY_UI)
            .text_size(len(font::BODY))
            .child(automations_page(&self.view, &self.ui, host, false, cx))
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
        let name = scene.name;
        let at = |what: &str, err: String| format!("{name}: {what}: {err}");
        let mut offscreen = open(scene).map_err(|e| at("open", e))?;
        let path = dir.join(format!("{name}.png"));
        let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
        let bytes = fs::metadata(&path)
            .map_err(|e| at("stat", e.to_string()))?
            .len();
        done.push(Rendered {
            name,
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
