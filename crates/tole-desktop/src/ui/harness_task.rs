//! The task view's states, one PNG each (`task-*.png`), drawn from the model's
//! own events through a [`Preview`], so the window is the one a driver feeds.
//!
//! Compared with board S3 (the transcript part; the status row and composer
//! are in later captures): the owner's bubble at the right, Tole's words as
//! plain text, a "Working" group that folds, a plan as a list.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::rc::Rc;

use tole_desktop_model::task::TaskEvent;
use tole_desktop_model::{Action, ConnectionStatus, OwnerAction, ServeState, ShellState};

use super::adapter::offscreen::Offscreen;
use super::harness::{Rendered, WindowSize};
pub use super::harness_scenes::{exchanges, scenes};
use super::shell::ShellView;
use super::status::{Source, Status};
use super::task::TaskCall;
use crate::backend::TaskRefused;
use crate::driver::task::{Preview, TaskView};

const BOARD: &str = "S3 (transcript)";
/// The session the scenes show.
pub const SESSION: &str = "20261123-1";

/// A source with one task, read from a [`Preview`].
struct Tasks(Option<Rc<Preview>>);

impl Source for Tasks {
    fn changed(&self) -> bool {
        false
    }

    fn refresh(&self, _shell: &mut ShellState) -> Status {
        status()
    }

    fn owner_action(&self, _action: OwnerAction) {}

    fn task_view(&self, session: &str) -> Option<TaskView> {
        let task = self.0.as_ref().filter(|_| session == SESSION)?;
        Some(task.view())
    }

    fn task_call(&self, session: &str, call: TaskCall) -> Result<(), TaskRefused> {
        let task = (self.0.as_ref().filter(|_| session == SESSION)).ok_or(TaskRefused::NotOpen)?;
        match call {
            TaskCall::Viewport {
                top,
                height,
                overscan,
            } => task.set_viewport(top, height, overscan),
            TaskCall::Heights(heights) => {
                task.set_heights(&heights);
            }
            TaskCall::Toggle(row) => task.toggle_group(row),
        }
        Ok(())
    }
}

fn status() -> Status {
    let serve = ServeState::Running {
        version: "0.12.0".into(),
    };
    Status {
        serve,
        skills_unknown: false,
        actionable: true,
    }
}

fn shell() -> ShellState {
    let mut state = ShellState::new();
    state.apply(Action::AddFolder {
        name: "Invoices".into(),
        path: "/home/me/Invoices".into(),
    });
    state.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
    state
}

/// How the list of `window` stands: following the end, and scrolled how far.
pub fn scroll_state(window: &mut Offscreen<ShellView>) -> Result<(bool, f32), String> {
    window
        .read(|view, _| view.task_scroll())?
        .ok_or_else(|| "no task is shown".to_owned())
}

/// What the owner's wheel does: scroll the list to `top`, and let it settle.
pub fn scroll_to(window: &mut Offscreen<ShellView>, top: f32) -> Result<(), String> {
    window.update(|view, _, _| view.scroll_task_to(top))?;
    window.settle(8)
}

/// The "Jump to latest" button, pressed.
pub fn jump(window: &mut Offscreen<ShellView>) -> Result<(), String> {
    window.update(|view, _, cx| view.task_jump(cx))?;
    window.settle(8)
}

/// One state to draw.
pub struct Scene {
    pub name: &'static str,
    /// The task, or none yet (the window says it is opening).
    pub task: Option<Rc<Preview>>,
    /// Rows of "Working" groups to open.
    pub open: Vec<usize>,
    /// Rows whose technical details to show.
    pub technical: Vec<usize>,
    /// Scroll the list here after it settled.
    pub scroll: Option<f32>,
}

impl Scene {
    pub(super) fn of(name: &'static str, events: Vec<TaskEvent>) -> Scene {
        let task = Preview::new(false);
        task.feed(events);
        Scene {
            name,
            task: Some(Rc::new(task)),
            open: vec![],
            technical: vec![],
            scroll: None,
        }
    }
}

/// The window showing `scene`, settled, with its groups opened and its list scrolled.
pub fn open(scene: &Scene, window: WindowSize) -> Result<Offscreen<ShellView>, String> {
    let source = Box::new(Tasks(scene.task.clone()));
    let mut offscreen = Offscreen::open(window.width, window.height, move |_cx| {
        let mut view = ShellView::with_source(shell(), status(), source);
        view.show_task(SESSION);
        view
    })?;
    offscreen.settle(8)?;
    if let Some(task) = &scene.task {
        scene.open.iter().for_each(|row| task.toggle_group(*row));
    }
    for row in &scene.technical {
        offscreen.update(|view, _, _| view.toggle_technical(*row))?;
    }
    offscreen.settle(8)?;
    if let Some(top) = scene.scroll {
        offscreen.update(|view, _, _| view.scroll_task_to(top))?;
        offscreen.settle(8)?;
    }
    Ok(offscreen)
}

/// Renders every scene into `dir` (which must exist) and adds a line for each
/// to `index.tsv`, when there is one.
pub fn render_tasks(dir: &Path, window: WindowSize) -> Result<Vec<Rendered>, String> {
    let mut done = Vec::new();
    for scene in scenes() {
        let at = |what: &str, err: String| format!("{}: {what}: {err}", scene.name);
        let mut offscreen = open(&scene, window).map_err(|e| at("open", e))?;
        let path = dir.join(format!("{}.png", scene.name));
        let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
        let bytes = fs::metadata(&path)
            .map_err(|e| at("stat", e.to_string()))?
            .len();
        done.push(Rendered {
            name: scene.name,
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
            .map_err(|e| e.to_string())?;
        for r in &done {
            let line = format!(
                "{}.png\t{}\t{}\t{}\t{}\t{}\n",
                r.name, r.board, r.keys, r.width, r.height, r.bytes
            );
            file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        }
    }
    Ok(done)
}
