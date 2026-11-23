//! The one file that names the GUI toolkit.
//!
//! GPUI and gpui-component are pinned exactly in `Cargo.toml` and used only
//! here; the views import what they need from this module. Moving to another
//! GPUI release means changing this file (plus the pin), nothing else.
//! `scripts/desktop-check.sh deps` fails if another file names the toolkit.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tole_desktop_model::strings;
use tole_desktop_model::tokens::Rgb;

pub use gpui::prelude::*;
pub use gpui::{div, App, Context, FocusHandle, Hsla, Pixels, Rgba, Role, Window};

// Tab and Shift+Tab: nothing in GPUI binds them, so the app does (see `init`).
gpui::actions!(tole_desktop, [FocusNextRow, FocusPrevRow]);

/// A token colour as a GPUI colour.
pub fn color(c: Rgb) -> Rgba {
    gpui::rgb(c.hex())
}

/// Fully transparent, for a border that only shows on focus.
pub fn transparent() -> Hsla {
    gpui::transparent_black()
}

/// A token length as a GPUI length.
pub fn len(v: f32) -> Pixels {
    gpui::px(v)
}

/// A repeating call on the window's thread; it stops when this is dropped.
pub struct Every {
    _task: gpui::Task<()>,
}

/// Calls `f` on the view every `period` (on the window's thread, between two
/// frames), for as long as the returned handle lives and the view does. The
/// first call comes after one period.
pub fn every<V: 'static>(
    cx: &mut Context<V>,
    period: Duration,
    mut f: impl FnMut(&mut V, &mut Context<V>) + 'static,
) -> Every {
    let task = cx.spawn(async move |this, cx| loop {
        cx.background_executor().timer(period).await;
        if this.update(cx, |view, cx| f(view, cx)).is_err() {
            // The view is gone: the window closed.
            break;
        }
    });
    Every { _task: task }
}

/// Runs `f` once when the view is released (the window closed), before the
/// application ends.
pub fn on_release<V: 'static>(cx: &mut Context<V>, f: impl FnOnce(&mut V) + 'static) {
    cx.on_release(|view, _cx| f(view)).detach();
}

/// Whether `handle` is the focused element of `window` (for the render harness).
#[cfg(feature = "render-harness")]
pub fn is_focused(handle: &FocusHandle, window: &Window) -> bool {
    handle.is_focused(window)
}

/// Sets the application up: the component library and the keyboard focus keys.
/// Runs once before the first window opens, for the real window and for the
/// offscreen harness alike, so both behave the same.
pub fn init(cx: &mut App) {
    // Must run before any gpui-component feature is used.
    gpui_component::init(cx);
    cx.bind_keys([
        gpui::KeyBinding::new("tab", FocusNextRow, None),
        gpui::KeyBinding::new("shift-tab", FocusPrevRow, None),
    ]);
    cx.on_action(|_: &FocusNextRow, cx| move_focus(cx, true));
    cx.on_action(|_: &FocusPrevRow, cx| move_focus(cx, false));
}

/// Moves the focus of the active window (or of the only window) one tab stop.
/// The window is busy while it dispatches the key, so the move is deferred.
fn move_focus(cx: &mut App, forward: bool) {
    let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, cx| {
            if forward {
                window.focus_next(cx);
            } else {
                window.focus_prev(cx);
            }
        });
    });
}

/// Ends the application from inside its loop. A `quit` made before the
/// platform's loop runs (while the app is still launching) is lost, so this
/// makes it from a task, which only runs once the loop does.
fn quit_when_running(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.update(|cx| cx.quit());
    })
    .detach();
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
            // With no display server GPUI starts headless: a window "opens"
            // and nothing is ever drawn, and the process would wait for ever.
            // Say so and end, before the view (and the app behind it) is built.
            if cx.displays().is_empty() {
                *report.borrow_mut() = Some(strings::NO_DISPLAY.to_owned());
                quit_when_running(cx);
                return;
            }
            init(cx);
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
                    quit_when_running(cx);
                }
            }
        });
    let reason = failure.borrow_mut().take();
    match reason {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// The offscreen window of the render harness (`scripts/desktop-render.sh`).
///
/// It uses GPUI's own test platform (no display server, no window is shown)
/// with a real text system and GPUI's headless GPU renderer, so what it writes
/// is what the shipped window paints. It needs GPUI's `test-support` feature,
/// which the committed manifest never enables: the script adds it to a
/// temporary copy of the manifest. It is compiled only with the
/// `render-harness` feature.
#[cfg(feature = "render-harness")]
pub mod offscreen {
    use std::path::Path;
    use std::sync::Arc;

    use super::{init, len, Context, Render, Window};
    use gpui::AppContext as _;

    /// A window nobody sees, with the keyboard and the renderer of a real one.
    pub struct Offscreen<V: 'static> {
        cx: gpui::HeadlessAppContext,
        window: gpui::AnyWindowHandle,
        view: gpui::Entity<V>,
    }

    impl<V: Render + 'static> Offscreen<V> {
        /// Opens a window of `width` by `height` logical pixels showing the
        /// view `build` makes, wrapped like the shipped window.
        pub fn open(
            width: f32,
            height: f32,
            build: impl FnOnce(&mut Context<V>) -> V,
        ) -> Result<Self, String> {
            // A headless platform needs no display server; it is only the
            // source of the text system (system fonts, as in the real window).
            let text_system = gpui_platform::current_platform(true).text_system();
            let mut cx = gpui::HeadlessAppContext::with_platform(
                text_system,
                Arc::new(()),
                gpui_platform::current_headless_renderer,
            );
            cx.update(init);
            let mut view = None;
            let window = cx
                .open_window(gpui::size(len(width), len(height)), |window, cx| {
                    let inner = cx.new(build);
                    view = Some(inner.clone());
                    cx.new(|cx| gpui_component::Root::new(inner, window, cx))
                })
                .map_err(|err| err.to_string())?;
            let view = view.ok_or("the window did not build its view")?;
            let mut open = Offscreen {
                cx,
                window: window.into(),
                view,
            };
            open.draw()?;
            Ok(open)
        }

        /// Runs pending work and paints one frame.
        pub fn draw(&mut self) -> Result<(), String> {
            self.cx.run_until_parked();
            self.cx
                .update_window(self.window, |_, window, cx| window.draw(cx).clear(cx))
                .map_err(|err| err.to_string())?;
            self.cx.run_until_parked();
            Ok(())
        }

        /// Presses one key (`"tab"`, `"shift-tab"`) and paints the result.
        pub fn press(&mut self, keystroke: &'static str) -> Result<(), String> {
            let keystroke = gpui::Keystroke::parse(keystroke).map_err(|err| err.to_string())?;
            self.cx
                .update_window(self.window, |_, window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                })
                .map_err(|err| err.to_string())?;
            self.draw()
        }

        /// Reads the view, with the window to ask which element has focus.
        pub fn read<R>(&mut self, f: impl FnOnce(&V, &Window) -> R) -> Result<R, String> {
            let view = self.view.clone();
            self.cx
                .update_window(self.window, |_, window, cx| f(view.read(cx), window))
                .map_err(|err| err.to_string())
        }

        /// Paints a frame and writes it as a PNG; returns its size in pixels
        /// and its RGBA bytes. The test platform renders at a fixed scale
        /// factor of 2.
        pub fn write_png(&mut self, path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
            self.draw()?;
            let image = self
                .cx
                .capture_screenshot(self.window)
                .map_err(|err| err.to_string())?;
            image.save(path).map_err(|err| err.to_string())?;
            Ok((image.width(), image.height(), image.into_raw()))
        }
    }
}
