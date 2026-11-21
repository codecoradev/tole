//! Checks of the offscreen render harness. They run only through
//! `scripts/desktop-render.sh --test` (feature `render-harness`, with the local
//! patch of the manifest); a plain `cargo test` skips this file.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use tole_desktop::ui::harness::{focus_walk, png_size, render_all, WindowSize};
use tole_desktop_model::tokens::{Rgb, DARK};
use tole_desktop_model::{shell_captures, FocusTarget, ShellState};

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

#[test]
fn every_capture_is_a_non_empty_png_of_the_window_size() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = out_dir("all");
    let window = WindowSize::default();
    let rendered = render_all(&dir, window).expect("render");
    let captures = shell_captures();
    assert_eq!(rendered.len(), captures.len());

    // The test platform renders at a fixed whole scale factor (2 today).
    let scale = rendered[0].width / window.width as u32;
    assert!((1..=4).contains(&scale), "scale {scale}");
    for (r, c) in rendered.iter().zip(&captures) {
        assert_eq!(r.name, c.name);
        assert!(r.bytes > 1000, "{}: {} bytes", r.name, r.bytes);
        assert_eq!(
            (r.width, r.height),
            (window.width as u32 * scale, window.height as u32 * scale),
            "{}",
            r.name
        );
        // The file on disk says the same as the frame in memory.
        assert_eq!(
            png_size(&r.path).expect("png"),
            (r.width, r.height),
            "{}",
            r.name
        );
        // Not a blank frame: background, panel, text, ... .
        assert!(r.colours() > 8, "{}: {} colours", r.name, r.colours());
    }

    // The frame is the model's tokens, not just any picture: the sidebar is the
    // panel colour and the main area the background colour.
    let first = &rendered[0];
    let (sidebar_x, main_x, y) = (100 * scale, 700 * scale, 500 * scale);
    assert_eq!(first.pixel(sidebar_x, y), rgb(DARK.panel), "sidebar");
    assert_eq!(
        first.pixel(main_x, 100 * scale),
        rgb(DARK.background),
        "main area"
    );

    // One line per image in the index, and nothing else but the images.
    let index = std::fs::read_to_string(dir.join("index.tsv")).expect("index");
    assert_eq!(index.lines().count(), captures.len() + 1);
    let pngs = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
        .count();
    assert_eq!(pngs, captures.len());
}

#[test]
fn the_keyboard_focus_ring_is_visible_in_the_focus_captures() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = out_dir("focus");
    let rendered = render_all(&dir, WindowSize::default()).expect("render");
    let by_name = |n: &str| rendered.iter().find(|r| r.name == n).expect(n);
    let (plain, tabbed) = (
        by_name("shell-preview"),
        by_name("shell-focus-first-section"),
    );
    assert_eq!(plain.rgba.len(), tabbed.rgba.len());
    let differing = plain
        .rgba
        .iter()
        .zip(&tabbed.rgba)
        .filter(|(a, b)| a != b)
        .count();
    assert!(differing > 0, "Tab changed nothing on screen");
    // The ring is the accent colour.
    let accent = rgb(DARK.accent);
    let has_accent =
        |r: &tole_desktop::ui::harness::Rendered| r.rgba.chunks_exact(4).any(|px| px == accent);
    assert!(!has_accent(plain), "the plain preview shows the accent");
    assert!(has_accent(tabbed), "no accent pixel after Tab");
}

#[test]
fn tab_visits_the_rows_in_the_order_of_the_model_and_shift_tab_goes_back() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let state = ShellState::placeholder();
    let order = state.tab_order();
    let window = WindowSize::default();

    // Forward: the k-th Tab lands on the k-th row, then wraps to the first.
    let tabs = vec!["tab"; order.len() + 1];
    let seen = focus_walk(state.clone(), &tabs, window).expect("walk");
    let forward: Vec<_> = order.iter().copied().map(Some).collect();
    assert_eq!(seen[..order.len()], forward[..], "Tab order");
    assert_eq!(
        seen[order.len()],
        Some(order[0]),
        "Tab wraps to the first row"
    );

    // Backward from nothing: Shift+Tab starts at the last row and walks up.
    let backs = vec!["shift-tab"; order.len()];
    let seen = focus_walk(state.clone(), &backs, window).expect("walk");
    let reverse: Vec<_> = order.iter().rev().copied().map(Some).collect();
    assert_eq!(seen, reverse, "Shift+Tab order");

    // Tab then Shift+Tab returns to where it started.
    let seen = focus_walk(state, &["tab", "tab", "tab", "shift-tab"], window).expect("walk");
    assert_eq!(seen[3], seen[1]);
    let unique: HashSet<FocusTarget> = order.iter().copied().collect();
    assert_eq!(unique.len(), order.len(), "every row is a distinct stop");
}
