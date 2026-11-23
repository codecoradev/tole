//! Checks of the task view in the offscreen render harness (#1031): what each
//! row and the status row show, how the list follows a running task, and what
//! Enter does in the composer. They run only through
//! `scripts/desktop-render.sh --test` (feature `render-harness`, with the local
//! patch of the manifest); a plain `cargo test` skips this file.

use std::path::PathBuf;
use std::sync::Mutex;

use tole_desktop::ui::harness::{Rendered, WindowSize};
use tole_desktop::ui::harness_task::{
    choose_section, close_shown, compose, composer_focused, composer_refused, exchanges, jump,
    open as open_scene, render_tasks, replace_driver, restarted_long_task, scenes, scroll_state,
    scroll_to, show_tab, shown, start, write, written, Scene, NEW_SESSION,
};
use tole_desktop::ui::TaskCall;
use tole_desktop_model::tokens::{size, Rgb, DARK};
use tole_desktop_model::Section;

/// One window at a time: each opens its own GPU device.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn out_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("render-harness")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn rgb(c: Rgb) -> [u8; 4] {
    [c.r(), c.g(), c.b(), 255]
}

fn scene(name: &str) -> Scene {
    scenes().into_iter().find(|s| s.name == name).expect(name)
}

/// Every pixel of `colour` in the transcript: right of the sidebar, below the
/// strip and above the composer.
fn count_in_main(r: &Rendered, colour: Rgb) -> usize {
    let from_x = size::SIDEBAR_WIDTH as u32 * 2 + 4;
    let (from_y, to_y) = ((size::TOPBAR_HEIGHT as u32 + 60) * 2, r.height - 170 * 2);
    let at = rgb(colour);
    let mut n = 0;
    for y in from_y..to_y {
        for x in from_x..r.width {
            n += usize::from(r.pixel(x, y) == at);
        }
    }
    n
}

#[test]
fn every_task_scene_is_drawn_and_shows_what_its_rows_say() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = out_dir("tasks");
    std::fs::create_dir_all(&dir).expect("dir");
    let rendered = render_tasks(&dir, WindowSize::default()).expect("render");
    let by_name = |n: &str| rendered.iter().find(|r| r.name == n).expect(n);
    for r in &rendered {
        assert!(r.colours() > 8 && r.bytes > 1000, "{}: blank", r.name);
    }
    // The owner's words are a bubble (the raised colour); a task with none has no bubble.
    for with in ["task-working", "task-thinking", "task-answer-markdown"] {
        assert!(
            count_in_main(by_name(with), DARK.raised) > 2_000,
            "{with}: no bubble"
        );
    }
    for without in ["task-empty", "task-opening"] {
        assert_eq!(count_in_main(by_name(without), DARK.raised), 0, "{without}");
    }
    // A step that did not work is shown in the danger colour, and only then.
    assert!(count_in_main(by_name("task-failed-step"), DARK.danger) > 50);
    assert_eq!(count_in_main(by_name("task-working"), DARK.danger), 0);
    // An opened group shows its link (accent); a folded one does not.
    assert!(
        count_in_main(by_name("task-working"), DARK.accent) > 50,
        "an open group"
    );
    assert_eq!(
        count_in_main(by_name("task-writing"), DARK.accent),
        0,
        "a folded group"
    );
}

#[test]
fn a_long_task_builds_a_window_of_rows_and_the_list_does_not_grow_with_it() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let long = scene("task-long-at-the-end");
    let task = long.task.clone().expect("a task");
    let mut window = open_scene(&long, WindowSize::default()).expect("open");
    let (following, top) = scroll_state(&mut window).expect("state");
    let view = task.view();
    // 500 exchanges are 1,500 rows; only those near the screen are built.
    assert_eq!(view.total_rows, 1_500);
    assert!(view.rows.len() < 60, "built {} rows", view.rows.len());
    assert!(
        following && view.window.end == view.total_rows,
        "at the end"
    );
    assert!(top > 20_000.0, "scrolled to the end of a long list: {top}");
}

#[test]
fn a_running_task_follows_its_end_until_the_owner_scrolls_up_and_jump_returns() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let long = scene("task-long-at-the-end");
    let task = long.task.clone().expect("a task");
    let mut w = open_scene(&long, WindowSize::default()).expect("open");
    let (_, at_end) = scroll_state(&mut w).expect("state");
    // More words arrive: the list moves with them.
    task.feed(exchanges(100_000, 20));
    w.settle(8).expect("settle");
    let (following, top) = scroll_state(&mut w).expect("state");
    assert!(
        following && top > at_end,
        "it did not follow: {top} <= {at_end}"
    );
    // The owner scrolls up: it stays where they put it however much arrives.
    scroll_to(&mut w, top - 3_000.0).expect("scroll");
    let (following, reading) = scroll_state(&mut w).expect("state");
    assert!(!following);
    task.feed(exchanges(200_000, 20));
    w.settle(8).expect("settle");
    assert_eq!(
        scroll_state(&mut w),
        Ok((false, reading)),
        "it moved under the reader"
    );
    // "Jump to latest" follows again.
    jump(&mut w).expect("jump");
    let (following, top) = scroll_state(&mut w).expect("state");
    assert!(following && top > reading);
}

/// How many pixels of `colour` the strip (tabs, status, chip, Stop) of a task
/// holds in the box (logical pixels) of the 1200 x 780 window.
fn strip_count(r: &Rendered, x: (u32, u32), colour: Rgb) -> usize {
    let (y0, y1) = (
        size::TOPBAR_HEIGHT as u32 * 2,
        (size::TOPBAR_HEIGHT as u32 + 48) * 2,
    );
    let at = rgb(colour);
    let mut n = 0;
    for y in y0..y1 {
        for px in x.0 * 2..x.1 * 2 {
            n += usize::from(r.pixel(px, y) == at);
        }
    }
    n
}

#[test]
fn the_status_row_has_its_dot_and_stop_only_while_a_turn_can_be_stopped() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = out_dir("status");
    std::fs::create_dir_all(&dir).expect("dir");
    let rendered = render_tasks(&dir, WindowSize::default()).expect("render");
    let by_name = |n: &str| rendered.iter().find(|r| r.name == n).expect(n);
    let (strip, stop) = ((size::SIDEBAR_WIDTH as u32, 1200), (1200 - 90, 1200 - 16));
    // The Stop button is white text on the strip at its right end: only a running turn has it.
    let text = DARK.text_primary;
    assert!(
        strip_count(by_name("task-working"), stop, text) > 20,
        "no Stop while working"
    );
    for idle in [
        "task-answer-markdown",
        "task-waiting-approval",
        "task-offline",
        "task-look-only",
    ] {
        assert_eq!(
            strip_count(by_name(idle), stop, text),
            0,
            "{idle}: Stop offered"
        );
    }
    // The dot says how it is going: going on and done are good, waiting asks, a lost link is bad.
    let tone = |scene: &str, c: Rgb| strip_count(by_name(scene), strip, c);
    assert!(tone("task-working", DARK.ok) > 20 && tone("task-working", DARK.amber) == 0);
    assert!(tone("task-waiting-approval", DARK.amber) > 20);
    assert!(tone("task-offline", DARK.danger) > 20 && tone("task-offline", DARK.ok) == 0);
    assert_eq!(tone("task-look-only", DARK.amber), 0);
}

fn calls_of(scene: &Scene) -> Vec<TaskCall> {
    scene
        .calls
        .borrow()
        .iter()
        .map(|(_, c)| c.clone())
        .collect()
}

#[test]
fn enter_sends_shift_enter_breaks_the_line_and_an_input_method_keeps_its_enter() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scene = scene("task-answer-markdown");
    let mut w = open_scene(&scene, WindowSize::default()).expect("open");
    assert!(
        composer_focused(&mut w).expect("focus"),
        "the box takes focus"
    );

    // Blank text sends nothing and makes no line break.
    w.press("enter").expect("press");
    assert!(calls_of(&scene).is_empty());
    assert_eq!(written(&mut w).expect("text"), "");

    // Shift+Enter breaks the line and sends nothing.
    write(&mut w, "first line").expect("write");
    w.press("shift-enter").expect("press");
    assert!(calls_of(&scene).is_empty(), "Shift+Enter sent");
    assert!(
        written(&mut w).expect("text").contains('\n'),
        "no line break"
    );

    // Enter while an input method composes is the input method's.
    write(&mut w, "tidy").expect("write");
    compose(&mut w, "\u{304B}").expect("compose");
    w.press("enter").expect("press");
    assert!(calls_of(&scene).is_empty(), "Enter sent while composing");
    assert!(!written(&mut w).expect("text").is_empty(), "the words went");

    // Enter sends the words, and the box is empty for the next.
    write(&mut w, "tidy the folder").expect("write");
    w.press("enter").expect("press");
    assert_eq!(calls_of(&scene), [TaskCall::Send("tidy the folder".into())]);
    assert_eq!(written(&mut w).expect("text"), "");
}

#[test]
fn a_running_turn_takes_a_note_and_a_waiting_or_lost_task_takes_nothing() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let working = scene("task-working");
    let mut w = open_scene(&working, WindowSize::default()).expect("open");
    write(&mut w, "use July as the first month").expect("write");
    w.press("enter").expect("press");
    assert_eq!(
        calls_of(&working),
        [TaskCall::Steer("use July as the first month".into())]
    );

    for blocked in ["task-waiting-approval", "task-offline"] {
        let scene = scene(blocked);
        let mut w = open_scene(&scene, WindowSize::default()).expect("open");
        write(&mut w, "hello").expect("write");
        w.press("enter").expect("press");
        assert!(calls_of(&scene).is_empty(), "{blocked}: Enter sent");
        assert_eq!(
            written(&mut w).expect("text"),
            "hello",
            "{blocked}: the words are kept"
        );
    }
}

#[test]
fn a_message_the_task_refuses_stays_in_the_box() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut refusing = scene("task-answer-markdown");
    refusing.refuse = true;
    let mut w = open_scene(&refusing, WindowSize::default()).expect("open");
    write(&mut w, "tidy the folder").expect("write");
    w.press("enter").expect("press");
    assert!(calls_of(&refusing).is_empty());
    assert_eq!(written(&mut w).expect("text"), "tidy the folder");
}

#[test]
fn closing_the_shown_tab_shows_its_neighbour_and_the_last_one_leaves_the_task_view() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let tabs = scene("task-tabs");
    let mut w = open_scene(&tabs, WindowSize::default()).expect("open");
    // Three tasks are open; the shown one is the first.
    assert_eq!(
        close_shown(&mut w).expect("close").as_deref(),
        Some("20261123-2")
    );
    assert_eq!(
        close_shown(&mut w).expect("close").as_deref(),
        Some("20261123-3")
    );
    assert_eq!(close_shown(&mut w).expect("close"), None);
}

#[test]
fn new_task_asks_for_a_session_in_the_selected_folder_and_shows_the_task_that_appears() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let home = scene("home-new-task");
    let mut w = open_scene(&home, WindowSize::default()).expect("open");
    assert_eq!(shown(&mut w).expect("shown"), None, "the home page");
    start(&mut w).expect("start");
    assert_eq!(
        *home.starts.borrow(),
        [Some("/home/me/Invoices".to_owned())]
    );
    assert_eq!(shown(&mut w).expect("shown").as_deref(), Some(NEW_SESSION));
}

#[test]
fn a_section_takes_the_main_area_and_the_tasks_stay_open() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let tabs = scene("task-tabs");
    let mut w = open_scene(&tabs, WindowSize::default()).expect("open");
    assert_eq!(shown(&mut w).expect("shown").as_deref(), Some("20261123-1"));
    choose_section(&mut w, Section::Automations).expect("section");
    assert_eq!(
        shown(&mut w).expect("shown"),
        None,
        "the page took the area"
    );
    assert_eq!(tabs.ids.borrow().len(), 3, "nothing was closed");
}

#[test]
fn a_new_driver_after_a_restart_is_given_the_viewport_and_the_heights_again() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let long = scene("task-long-scrolled-up");
    let mut w = open_scene(&long, WindowSize::default()).expect("open");
    let (following, reading) = scroll_state(&mut w).expect("state");
    assert!(
        !following && reading > 10_000.0,
        "the owner is reading far up"
    );
    let old = long.task.clone().expect("a task");
    assert!(
        old.view().window.start > 100,
        "the window is where it is read"
    );

    // The serve restarts: the task has a new driver, at the top and blank.
    let new = restarted_long_task();
    assert_eq!(new.view().window.start, 0);
    replace_driver(&long, new.clone());
    w.settle(8).expect("settle");
    let view = new.view();
    assert!(
        view.window.start > 100 && view.window.before > 10_000.0,
        "the new driver has the window of the list: start {}",
        view.window.start
    );
    assert!(!view.rows.is_empty());
    // The owner is still reading there (the anchor may move the list a little
    // while the new heights come in), and the list did not jump to the end.
    let (following, now) = scroll_state(&mut w).expect("state");
    assert!(
        !following && (now - reading).abs() < 2_000.0,
        "now at {now}, was at {reading}"
    );
}

#[test]
fn the_note_about_a_refused_send_goes_when_the_driver_is_a_new_one() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut scene = scene("task-long-at-the-end");
    scene.refuse = true;
    let mut w = open_scene(&scene, WindowSize::default()).expect("open");
    write(&mut w, "tidy the folder").expect("write");
    w.press("enter").expect("press");
    assert!(
        composer_refused(&mut w).expect("note"),
        "the refusal is told"
    );
    assert_eq!(written(&mut w).expect("text"), "tidy the folder");
    replace_driver(&scene, restarted_long_task());
    w.settle(8).expect("settle");
    assert!(
        !composer_refused(&mut w).expect("note"),
        "a new driver, a new chance"
    );
    assert_eq!(
        written(&mut w).expect("text"),
        "tidy the folder",
        "the words stay"
    );
}

#[test]
fn choosing_a_tab_gives_its_composer_the_keyboard() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let tabs = scene("task-tabs");
    let mut w = open_scene(&tabs, WindowSize::default()).expect("open");
    assert!(
        composer_focused(&mut w).expect("focus"),
        "the first task opens ready to write"
    );
    w.press("tab").expect("press");
    assert!(
        !composer_focused(&mut w).expect("focus"),
        "Tab left the box"
    );
    show_tab(&mut w, "20261123-1").expect("tab");
    assert!(
        composer_focused(&mut w).expect("focus"),
        "the tab gives it back"
    );
}
