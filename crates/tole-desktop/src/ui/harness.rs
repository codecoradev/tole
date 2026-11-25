//! The offscreen render harness: draws the real window without showing it and
//! writes one PNG per screen and state (#1028).
//!
//! Compiled only with the `render-harness` feature, which needs GPUI's
//! `test-support` feature that the committed manifest never enables; run it
//! through `scripts/desktop-render.sh` (docs/design/desktop/README.md,
//! "Reviewing the UI offscreen"). This file names no toolkit type: the window
//! is [`Offscreen`] from the adapter, the screens come from the model
//! ([`shell_captures`]).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use tole_desktop_model::tokens::size;
use tole_desktop_model::{shell_captures, Capture, FocusTarget, ShellState};

use super::adapter::offscreen::Offscreen;
use super::shell::ShellView;

/// The size of the window in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSize {
    pub width: f32,
    pub height: f32,
}

impl Default for WindowSize {
    /// The size the shipped window opens at.
    fn default() -> Self {
        WindowSize {
            width: size::WINDOW_WIDTH,
            height: size::WINDOW_HEIGHT,
        }
    }
}

impl WindowSize {
    /// `1440x900`, as `--size` takes it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("`{text}` is not a size like 1440x900");
        let (w, h) = text.split_once('x').ok_or_else(bad)?;
        let (width, height) = (
            w.parse::<f32>().map_err(|_| bad())?,
            h.parse::<f32>().map_err(|_| bad())?,
        );
        if width < 200.0 || height < 200.0 || width > 8000.0 || height > 8000.0 {
            return Err(format!("`{text}` is outside 200 to 8000 pixels"));
        }
        Ok(WindowSize { width, height })
    }
}

/// An image that was rendered: the PNG on disk and the pixels it holds.
#[derive(Debug)]
pub struct Rendered {
    pub name: &'static str,
    pub board: &'static str,
    pub keys: &'static str,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    /// Size of the PNG file.
    pub bytes: u64,
    /// RGBA, row by row.
    pub rgba: Vec<u8>,
}

impl Rendered {
    /// The pixel at `x`, `y` (device pixels) as `[r, g, b, a]`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[at],
            self.rgba[at + 1],
            self.rgba[at + 2],
            self.rgba[at + 3],
        ]
    }

    /// How many different colours the image holds (a blank frame has one).
    pub fn colours(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for px in self.rgba.chunks_exact(4) {
            seen.insert([px[0], px[1], px[2], px[3]]);
        }
        seen.len()
    }
}

/// The window with the shell showing `state`.
fn open(state: ShellState, window: WindowSize) -> Result<Offscreen<ShellView>, String> {
    Offscreen::open(window.width, window.height, move |_cx| {
        ShellView::new(state)
    })
}

/// Renders one capture: opens the window, presses its keys, writes `<dir>/<name>.png`.
pub fn render_capture(
    capture: &Capture,
    dir: &Path,
    window: WindowSize,
) -> Result<Rendered, String> {
    let at = |what: &str, err: String| format!("{}: {what}: {err}", capture.name);
    let mut offscreen = open(capture.state.clone(), window).map_err(|e| at("open", e))?;
    for key in capture.keys.split_whitespace() {
        offscreen.press(key).map_err(|e| at("press", e))?;
    }
    let path = dir.join(format!("{}.png", capture.name));
    let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
    let bytes = fs::metadata(&path)
        .map_err(|e| at("stat", e.to_string()))?
        .len();
    Ok(Rendered {
        name: capture.name,
        board: capture.board,
        keys: capture.keys,
        path,
        width,
        height,
        bytes,
        rgba,
    })
}

/// Renders every capture of the model into `dir` (created if missing) and
/// writes `index.tsv` beside the images: file, board, keys, size.
pub fn render_all(dir: &Path, window: WindowSize) -> Result<Vec<Rendered>, String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut done = Vec::new();
    for capture in shell_captures() {
        done.push(render_capture(&capture, dir, window)?);
    }
    let mut index = ["file", "board", "keys", "width", "height", "bytes"].join("\t") + "\n";
    for r in &done {
        let _ = writeln!(
            index,
            "{}.png\t{}\t{}\t{}\t{}\t{}",
            r.name, r.board, r.keys, r.width, r.height, r.bytes
        );
    }
    let index_path = dir.join("index.tsv");
    fs::write(&index_path, index).map_err(|e| format!("{}: {e}", index_path.display()))?;
    Ok(done)
}

/// Opens the shell showing `state`, presses `keys` one at a time and returns
/// which row has keyboard focus after each press (`None`: nothing focused).
pub fn focus_walk(
    state: ShellState,
    keys: &[&'static str],
    window: WindowSize,
) -> Result<Vec<Option<FocusTarget>>, String> {
    let mut offscreen = open(state, window)?;
    let mut seen = Vec::new();
    for key in keys {
        offscreen.press(key)?;
        seen.push(offscreen.read(|view, window| view.focused(window))?);
    }
    Ok(seen)
}

/// Reads a PNG's size from its header; fails on anything that is not a PNG.
pub fn png_size(path: &Path) -> Result<(u32, u32), String> {
    let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if data.len() < 33 || data[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Err(format!("{}: not a PNG", path.display()));
    }
    if &data[12..16] != b"IHDR" {
        return Err(format!("{}: no IHDR chunk", path.display()));
    }
    let word = |at: usize| u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    Ok((word(16), word(20)))
}

/// The command line of `examples/render.rs`: `<dir> [--size WxH]`.
pub fn cli(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let usage = "usage: render <output-dir> [--size WxH]";
    let mut dir = None;
    let mut window = WindowSize::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--size" => window = WindowSize::parse(&args.next().ok_or(usage)?)?,
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n{usage}")),
            _ if dir.is_none() => dir = Some(PathBuf::from(arg)),
            _ => return Err(usage.to_owned()),
        }
    }
    let dir = dir.ok_or(usage)?;
    let mut all = render_all(&dir, window)?;
    all.extend(super::harness_live::render_live(&dir, window)?);
    all.extend(super::harness_task::render_tasks(&dir, window)?);
    all.extend(super::harness_skills::render_skills(&dir, window)?);
    all.extend(super::approvals::scenes::render_scenes(&dir)?);
    #[cfg(unix)]
    all.extend(super::approvals::scenes_page::render_scenes(&dir)?);
    #[cfg(unix)]
    all.extend(super::approvals::scenes_shell::render_scenes(&dir, window)?);
    #[cfg(unix)]
    all.extend(super::automations::scenes::render_scenes(&dir)?);
    for r in all {
        println!(
            "{}  {}x{}  {} bytes  board: {}",
            r.path.display(),
            r.width,
            r.height,
            r.bytes,
            r.board
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_parses_and_refuses_nonsense() {
        assert_eq!(
            WindowSize::parse("1440x900"),
            Ok(WindowSize {
                width: 1440.0,
                height: 900.0
            })
        );
        for bad in ["1440", "x900", "axb", "10x10", "99999x900"] {
            assert!(WindowSize::parse(bad).is_err(), "{bad}");
        }
    }
}
