//! The one file that names the GUI toolkit.
//!
//! GPUI and gpui-component are pinned exactly in `Cargo.toml` and used only
//! here; the views import what they need from this module. Moving to another
//! GPUI release means changing this file (plus the pin), nothing else.
//! `scripts/desktop-check.sh deps` fails if another file names the toolkit.

use std::cell::RefCell;
use std::rc::Rc;

use tole_desktop_model::tokens::Rgb;

pub use gpui::prelude::*;
pub use gpui::{div, App, Context, Pixels, Rgba, Window};

/// A token colour as a GPUI colour.
pub fn color(c: Rgb) -> Rgba {
    gpui::rgb(c.hex())
}

/// A token length as a GPUI length.
pub fn len(v: f32) -> Pixels {
    gpui::px(v)
}

/// Opens one window showing the view `build` makes and runs the application
/// until the window closes. Returns the reason if the window could not open.
pub fn launch<V: Render>(
    title: &'static str,
    width: f32,
    height: f32,
    build: impl FnOnce(&mut Context<V>) -> V + 'static,
) -> Result<(), String> {
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let report = failure.clone();
    // GPUI's default keeps a macOS process alive after its last window closes
    // (there is no app menu with Quit yet), which would leave `launch` blocked.
    // Closing the window ends the process on every platform.
    gpui_platform::application()
        .with_quit_mode(gpui::QuitMode::LastWindowClosed)
        .run(move |cx: &mut App| {
            // Must run before any gpui-component feature is used.
            gpui_component::init(cx);
            let bounds = gpui::Bounds::centered(None, gpui::size(len(width), len(height)), cx);
            let options = gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let opened = cx.open_window(options, |window, cx| {
                let view = cx.new(build);
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            });
            match opened {
                Ok(_) => cx.activate(true),
                Err(err) => {
                    *report.borrow_mut() = Some(err.to_string());
                    cx.quit();
                }
            }
        });
    let reason = failure.borrow_mut().take();
    match reason {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}
