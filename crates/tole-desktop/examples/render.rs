//! Writes one PNG per screen and state of the real window, offscreen.
//!
//! Do not run this directly: it needs a local patch of the manifest.
//! `scripts/desktop-render.sh <output-dir> [--size WxH]` applies it to a
//! temporary copy, runs this example there and leaves the repository as it was.

use std::process::ExitCode;

fn main() -> ExitCode {
    match tole_desktop::ui::harness::cli(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("render: {err}");
            ExitCode::FAILURE
        }
    }
}
