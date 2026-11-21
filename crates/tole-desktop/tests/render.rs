//! Checks of the offscreen render harness. They run only through
//! `scripts/desktop-render.sh --test` (feature `render-harness`, with the local
//! patch of the manifest); a plain `cargo test` skips this file.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use tole_desktop::ui::harness::{focus_walk, png_size, render_all, Rendered, WindowSize};
use tole_desktop_model::tokens::{size, Rgb, DARK};
use tole_desktop_model::{
    shell_captures, Action, ConnectionStatus, FocusTarget, Section, ShellState,
};

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

/// Whether a pixel of `colour` lies in the box (logical pixels, `scale` device
/// pixels each) of `r`.
fn has_colour(r: &Rendered, scale: u32, x: (u32, u32), y: (u32, u32), colour: Rgb) -> bool {
    (y.0 * scale..y.1 * scale)
        .any(|py| (x.0 * scale..x.1 * scale).any(|px| r.pixel(px, py) == rgb(colour)))
}

/// Whether every pixel of the box is `colour`.
fn all_colour(r: &Rendered, scale: u32, x: (u32, u32), y: (u32, u32), colour: Rgb) -> bool {
    (y.0 * scale..y.1 * scale)
        .all(|py| (x.0 * scale..x.1 * scale).all(|px| r.pixel(px, py) == rgb(colour)))
}

#[test]
fn the_footer_and_the_header_show_a_dot_and_nothing_runs_over_the_sidebar_edge() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let window = WindowSize::default();
    let rendered = render_all(&out_dir("footer"), window).expect("render");
    let scale = rendered[0].width / window.width as u32;
    let (w, h) = (window.width as u32, window.height as u32);
    let edge = size::SIDEBAR_WIDTH as u32;
    let footer_y = (h - size::FOOTER_HEIGHT as u32 - 8, h);
    for (r, c) in rendered.iter().zip(shell_captures()) {
        let tone = DARK.tone(c.state.connection.status.tone());
        // The dot of the header, in the connection indicator on the right.
        let header_x = (w - size::HEADER_LABEL_MAX_WIDTH as u32 - 8, w);
        assert!(
            has_colour(r, scale, header_x, (0, size::TOPBAR_HEIGHT as u32), tone),
            "{}: no status dot in the header",
            c.name
        );
        if !c.state.sidebar_visible() {
            continue;
        }
        // The dot of the footer (it was not drawn in the preview state: the
        // text pushed it to zero width).
        assert!(
            has_colour(r, scale, (14, 28), footer_y, tone),
            "{}: no status dot in the sidebar footer",
            c.name
        );
        // The footer text is cut at the sidebar: the strip right of its edge is
        // background (it was text, "Not connected (preview)", before).
        assert!(
            all_colour(r, scale, (edge + 2, edge + 120), footer_y, DARK.background),
            "{}: the footer text runs over the sidebar edge",
            c.name
        );
    }
}

#[test]
fn the_counts_show_only_above_zero_and_a_hidden_sidebar_leaves_only_the_background() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let window = WindowSize::default();
    let rendered = render_all(&out_dir("counts"), window).expect("render");
    let scale = rendered[0].width / window.width as u32;
    let by_name = |n: &str| rendered.iter().find(|r| r.name == n).expect(n);
    let row_band = |section: Section| {
        let top = (size::TOPBAR_HEIGHT + size::GUTTER) as u32
            + section.index() as u32 * (size::ROW_HEIGHT + size::ROW_GAP) as u32;
        // The right end of the row, where the count is; the row is not selected.
        (
            (
                size::SIDEBAR_WIDTH as u32 - 100,
                size::SIDEBAR_WIDTH as u32 - 12,
            ),
            (top + 8, top + 26),
        )
    };
    let counted = [
        Section::Approvals,
        Section::Automations,
        Section::Discussions,
        Section::SkillsAndTools,
    ];
    for section in counted {
        let (x, y) = row_band(section);
        assert!(
            all_colour(by_name("shell-preview"), scale, x, y, DARK.panel),
            "{section:?}: a count at zero is drawn"
        );
        assert!(
            !all_colour(by_name("shell-counts"), scale, x, y, DARK.panel),
            "{section:?}: the count is not drawn"
        );
    }
    let (x, y) = row_band(Section::Search);
    assert!(all_colour(by_name("shell-counts"), scale, x, y, DARK.panel));
    // Hidden sidebar: no panel colour anywhere in the left strip.
    let collapsed = by_name("shell-sidebar-collapsed");
    assert!(all_colour(
        collapsed,
        scale,
        (0, 200),
        (size::TOPBAR_HEIGHT as u32 + 1, window.height as u32),
        DARK.background
    ));
}

#[test]
fn the_home_page_button_is_a_stop_only_when_it_can_start_and_the_toggle_is_first() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let window = WindowSize::default();
    let mut state = ShellState::placeholder();
    state.apply(Action::SetConnectionStatus(ConnectionStatus::Connected));
    let order = state.tab_order();
    assert_eq!(order.last(), Some(&FocusTarget::HomeStart));
    let seen = focus_walk(state.clone(), &vec!["tab"; order.len()], window).expect("walk");
    let forward: Vec<_> = order.iter().copied().map(Some).collect();
    assert_eq!(seen, forward);
    assert_eq!(seen[0], Some(FocusTarget::SidebarToggle));

    // With the sidebar hidden the toggle and the home page are what is left.
    state.apply(Action::ToggleSidebar);
    let order = state.tab_order();
    let seen = focus_walk(state, &vec!["tab"; order.len()], window).expect("walk");
    assert_eq!(seen, order.iter().copied().map(Some).collect::<Vec<_>>());
    assert!(!order.iter().any(|t| matches!(t, FocusTarget::Section(_))));
}
