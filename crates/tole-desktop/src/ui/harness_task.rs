//! The task view's states, one PNG each (`task-*.png`), drawn from the model's
//! own events through a [`Preview`], so the window is the one a driver feeds.
//!
//! Compared with board S3 (the transcript part; the status row and composer
//! are in later captures): the owner's bubble at the right, Tole's words as
//! plain text, a "Working" group that folds, a plan as a list.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::rc::Rc;

use tole_desktop_model::task::TaskEvent;
use tole_desktop_model::{Action, ConnectionStatus, OwnerAction, Section, ServeState, ShellState};

use super::adapter::offscreen::Offscreen;
use super::harness::{Rendered, WindowSize};
use super::harness_scenes::said;
pub use super::harness_scenes::{exchanges, scenes};
use super::shell::ShellView;
use super::status::{Source, Status};
use super::task::TaskCall;
use crate::backend::TaskRefused;
use crate::driver::task::{Preview, TaskView};

const BOARD: &str = "S3 (transcript)";
/// The session the scenes show.
pub const SESSION: &str = "20261123-1";

/// The calls the window made on tasks, in order.
pub type Calls = Rc<RefCell<Vec<(String, TaskCall)>>>;
/// The folder asked for by each "New task".
pub type Starts = Rc<RefCell<Vec<Option<String>>>>;
/// The ids of the tasks open, in order.
pub type Ids = Rc<RefCell<Vec<String>>>;

/// The tasks open and their drivers (a [`Preview`] each): a check replaces one
/// to stand for the serve restarting, which makes the task a new driver.
pub type Slots = Rc<RefCell<Vec<(String, Option<Rc<Preview>>)>>>;

/// The task a "New task" opens.
pub const NEW_SESSION: &str = "20261123-9";

/// The tasks the window has open: each a [`Preview`], or none yet.
struct Tasks {
    open: Slots,
    calls: Calls,
    /// The folder of every "New task", and the ids open now (for the checks).
    starts: Starts,
    ids: Ids,
    /// Refuse `Send` and `Steer`, as a task that is not connected does.
    refuse: bool,
    /// What a prompt does to the task, as a driver that accepted it would.
    seq: Cell<u64>,
}

impl Tasks {
    fn find(&self, session: &str) -> Option<Rc<Preview>> {
        let open = self.open.borrow();
        open.iter().find(|(id, _)| id == session)?.1.clone()
    }
}

impl Source for Tasks {
    fn changed(&self) -> bool {
        false
    }

    fn refresh(&self, _shell: &mut ShellState) -> Status {
        status()
    }

    fn owner_action(&self, _action: OwnerAction) {}

    fn open_tasks(&self) -> Vec<String> {
        self.open
            .borrow()
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn close_task(&self, session: &str) {
        self.open.borrow_mut().retain(|(id, _)| id != session);
        self.ids.borrow_mut().retain(|id| id != session);
    }

    fn new_task(&self, folder: Option<&str>) {
        self.starts.borrow_mut().push(folder.map(str::to_owned));
        let task = Preview::new(false);
        self.open
            .borrow_mut()
            .push((NEW_SESSION.to_owned(), Some(Rc::new(task))));
        self.ids.borrow_mut().push(NEW_SESSION.to_owned());
    }

    fn task_view(&self, session: &str) -> Option<TaskView> {
        Some(self.find(session)?.view())
    }

    fn task_call(&self, session: &str, call: TaskCall) -> Result<(), TaskRefused> {
        let task = self.find(session).ok_or(TaskRefused::NotOpen)?;
        match &call {
            TaskCall::Viewport {
                top,
                height,
                overscan,
            } => task.set_viewport(*top, *height, *overscan),
            TaskCall::Heights(heights) => {
                task.set_heights(heights);
            }
            TaskCall::Toggle(row) => task.toggle_group(*row),
            TaskCall::Send(text) | TaskCall::Steer(text) => {
                if self.refuse {
                    return Err(TaskRefused::Unavailable);
                }
                if matches!(call, TaskCall::Send(_)) {
                    let seq = self.seq.get() + 1;
                    self.seq.set(seq);
                    task.feed([said(seq, text), TaskEvent::Ready { busy: true }]);
                }
            }
            TaskCall::Stop => {}
        }
        if !matches!(
            call,
            TaskCall::Viewport { .. } | TaskCall::Heights(_) | TaskCall::Toggle(_)
        ) {
            self.calls.borrow_mut().push((session.to_owned(), call));
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
    /// Other tasks open beside it (their tabs).
    pub others: Vec<(&'static str, Rc<Preview>)>,
    /// What the window called on the tasks.
    pub calls: Calls,
    /// The tasks refuse messages, as one that is not connected does.
    pub refuse: bool,
    /// Text typed in the composer.
    pub typed: Option<&'static str>,
    /// The folder of each "New task" the window asked for.
    pub starts: Starts,
    /// The ids of the tasks open (the window closes them).
    pub ids: Ids,
    /// The tasks open and their drivers.
    pub slots: Slots,
    /// The window starts on the home page, with no task open.
    pub home: bool,
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
            others: vec![],
            calls: Calls::default(),
            refuse: false,
            typed: None,
            starts: Starts::default(),
            ids: Ids::default(),
            slots: Slots::default(),
            home: false,
            open: vec![],
            technical: vec![],
            scroll: None,
        }
    }
}

/// The window showing `scene`, settled, with its groups opened and its list scrolled.
pub fn open(scene: &Scene, window: WindowSize) -> Result<Offscreen<ShellView>, String> {
    let mut open = vec![(SESSION.to_owned(), scene.task.clone())];
    open.extend(
        scene
            .others
            .iter()
            .map(|(id, p)| ((*id).to_owned(), Some(Rc::clone(p)))),
    );
    if scene.home {
        open.clear();
    }
    *scene.ids.borrow_mut() = open.iter().map(|(id, _)| id.clone()).collect();
    *scene.slots.borrow_mut() = open;
    let source = Box::new(Tasks {
        open: Rc::clone(&scene.slots),
        calls: Rc::clone(&scene.calls),
        starts: Rc::clone(&scene.starts),
        ids: Rc::clone(&scene.ids),
        refuse: scene.refuse,
        seq: Cell::new(1_000),
    });
    let home = scene.home;
    let mut offscreen = Offscreen::open(window.width, window.height, move |_cx| {
        let mut view = ShellView::with_source(shell(), status(), source);
        // The tasks open at the start were seen by the first look, not opened since.
        view.sync_tasks();
        if !home {
            view.show_task(SESSION);
        }
        view
    })?;
    offscreen.settle(8)?;
    if let Some(task) = &scene.task {
        scene.open.iter().for_each(|row| task.toggle_group(*row));
    }
    for row in &scene.technical {
        offscreen.update(|view, _, _| view.toggle_technical(*row))?;
    }
    if let Some(text) = scene.typed {
        write(&mut offscreen, text)?;
    }
    offscreen.settle(8)?;
    if let Some(top) = scene.scroll {
        offscreen.update(|view, _, _| view.scroll_task_to(top))?;
        offscreen.settle(8)?;
    }
    Ok(offscreen)
}

/// The serve restarted: the task `SESSION` has a new driver, which starts at
/// the top with no measured heights and holds what `task` holds.
pub fn replace_driver(scene: &Scene, task: Rc<Preview>) {
    for (id, slot) in scene.slots.borrow_mut().iter_mut() {
        if id == SESSION {
            *slot = Some(Rc::clone(&task));
        }
    }
}

/// A new driver for the long task of the scene `task-long-at-the-end`.
pub fn restarted_long_task() -> Rc<Preview> {
    Rc::new(super::harness_scenes::long_task())
}

/// A task's tab, chosen.
pub fn show_tab(window: &mut Offscreen<ShellView>, id: &str) -> Result<(), String> {
    window.update(|view, window, cx| view.task_show(id, window, cx))?;
    window.settle(4)
}

/// Whether the note about a refused send is up.
pub fn composer_refused(window: &mut Offscreen<ShellView>) -> Result<bool, String> {
    window.read(|view, _| view.composer_refused())
}

/// How long each of `steps` scrolls of the list took to draw, in milliseconds:
/// the list moves by `by` pixels each time, from `from`, and draws one frame
/// (the window builds the rows near the screen, lays them out and paints).
pub fn scroll_times(
    window: &mut Offscreen<ShellView>,
    from: f32,
    by: f32,
    steps: usize,
) -> Result<Vec<f64>, String> {
    let mut times = Vec::with_capacity(steps);
    for i in 0..steps {
        let top = from + by * i as f32;
        let started = std::time::Instant::now();
        window.update(|view, _, _| view.scroll_task_to(top))?;
        times.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    Ok(times)
}

/// The home page's "New task", pressed.
pub fn start(window: &mut Offscreen<ShellView>) -> Result<(), String> {
    window.update(|view, _, cx| view.task_start(cx))?;
    window.settle(8)
}

/// A sidebar section, chosen: it takes the main area.
pub fn choose_section(window: &mut Offscreen<ShellView>, section: Section) -> Result<(), String> {
    window.update(|view, _, cx| view.dispatch(Action::SelectSection(section), cx))?;
    window.settle(4)
}

/// The task in the main area.
pub fn shown(window: &mut Offscreen<ShellView>) -> Result<Option<String>, String> {
    window.read(|view, _| view.shown_task())
}

/// Put `text` in the composer, as typing would.
pub fn write(window: &mut Offscreen<ShellView>, text: &str) -> Result<(), String> {
    window.update(|view, window, cx| view.write_composer(text, window, cx))
}

/// What the composer holds.
pub fn written(window: &mut Offscreen<ShellView>) -> Result<String, String> {
    window
        .update(|view, _, cx| view.composer_text(cx))?
        .ok_or_else(|| "no composer".to_owned())
}

/// An input method starts composing `text` in the composer.
pub fn compose(window: &mut Offscreen<ShellView>, text: &str) -> Result<(), String> {
    window.update(|view, window, cx| view.compose_composer(text, window, cx))
}

/// Whether the composer has keyboard focus.
pub fn composer_focused(window: &mut Offscreen<ShellView>) -> Result<bool, String> {
    window.update(|view, window, cx| view.composer_focused(window, cx))
}

/// The tab's × of the shown task, pressed; the task now shown.
pub fn close_shown(window: &mut Offscreen<ShellView>) -> Result<Option<String>, String> {
    window.update(|view, _, cx| {
        if let Some(id) = view.shown_task() {
            view.task_close(&id, cx);
        }
        view.shown_task()
    })
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
