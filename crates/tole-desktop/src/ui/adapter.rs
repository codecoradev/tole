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

pub use gpui::AnyElement;
#[cfg(unix)]
pub use gpui::{Div, FontWeight, ScrollHandle};

/// A length relative to the font size: `1.5` is a line and a half.
#[cfg(unix)]
pub fn rel(v: f32) -> gpui::DefiniteLength {
    gpui::relative(v)
}

/// One run of a line of text and how it is drawn.
#[cfg(unix)]
#[derive(Clone, Copy)]
pub struct Run<'a> {
    pub text: &'a str,
    pub bold: bool,
    pub italic: bool,
    /// Text colour; `None` is the colour of the element.
    pub fill: Option<Rgb>,
    /// Background colour.
    pub back: Option<Rgb>,
}

/// The runs as one wrapping piece of text.
#[cfg(unix)]
pub fn rich_text(runs: &[Run<'_>]) -> gpui::StyledText {
    let mut text = String::new();
    let mut styles = Vec::new();
    for run in runs {
        let start = text.len();
        text.push_str(run.text);
        let style = gpui::HighlightStyle {
            color: run.fill.map(|c| color(c).into()),
            font_weight: run.bold.then_some(gpui::FontWeight::BOLD),
            font_style: run.italic.then_some(gpui::FontStyle::Italic),
            background_color: run.back.map(|c| color(c).into()),
            ..Default::default()
        };
        if !run.text.is_empty() {
            styles.push((start..text.len(), style));
        }
    }
    gpui::StyledText::new(text).with_highlights(styles)
}

/// What a scrolling list laid out in its last frame (all zero before the first).
#[cfg(unix)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListFrame {
    /// How far the list is scrolled.
    pub top: f32,
    pub viewport: f32,
    /// The height of everything in the list.
    pub content: f32,
    /// Each direct child: where it starts in the content, and its height.
    pub children: Vec<(f32, f32)>,
}

/// The last frame of the list `handle` tracks.
#[cfg(unix)]
pub fn laid_out(handle: &ScrollHandle) -> ListFrame {
    let (offset, max) = (handle.offset(), handle.max_offset());
    let bounds = handle.bounds();
    let origin = f32::from(bounds.origin.y);
    let viewport = f32::from(bounds.size.height);
    let children = (0..)
        .map_while(|i| handle.bounds_for_item(i))
        .map(|b| (f32::from(b.origin.y) - origin, f32::from(b.size.height)))
        .collect();
    ListFrame {
        top: -f32::from(offset.y),
        viewport,
        content: viewport + f32::from(max.y),
        children,
    }
}

/// Scroll the list `handle` tracks to `top`.
#[cfg(unix)]
pub fn scroll_to(handle: &ScrollHandle, top: f32) {
    let x = handle.offset().x;
    handle.set_offset(gpui::point(x, gpui::px(-top.max(0.0))));
}

/// The Enter key as the text box's own action (`shift` for Shift+Enter). Handle
/// it with `capture_action` on an element around the box and call
/// `Context::stop_propagation` when the window takes it: the box then never
/// sees the key, so it makes no line break, whatever the platform does with a
/// key nobody handled.
#[cfg(unix)]
pub use gpui_component::input::Enter as EnterKey;

/// A text box of several lines that grows with its text. Enter and Shift+Enter
/// break the line, unless the window takes Enter first (see [`EnterKey`]). The input method is the toolkit's.
#[cfg(unix)]
pub struct TextBox {
    state: gpui::Entity<gpui_component::input::TextareaState>,
}

#[cfg(unix)]
impl TextBox {
    /// A box showing `hint` while it is empty, from `min_rows` to `max_rows` tall.
    pub fn new<V: 'static>(
        window: &mut Window,
        cx: &mut Context<V>,
        hint: &str,
        rows: (usize, usize),
    ) -> Self {
        let hint = hint.to_owned();
        let state = cx.new(|cx| {
            gpui_component::input::TextareaState::new(window, cx)
                .auto_grow(rows.0, rows.1)
                .placeholder(hint)
        });
        TextBox { state }
    }

    pub fn element(&self) -> impl IntoElement {
        gpui_component::input::Textarea::new(&self.state)
            .appearance(false)
            .bordered(false)
    }

    pub fn text(&self, cx: &App) -> String {
        self.state.read(cx).value().to_string()
    }

    pub fn set_text(&self, text: &str, window: &mut Window, cx: &mut App) {
        let text = text.to_owned();
        self.state
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    pub fn set_hint(&self, hint: &str, window: &mut Window, cx: &mut App) {
        let hint = hint.to_owned();
        self.state
            .update(cx, |state, cx| state.set_placeholder(hint, window, cx));
    }

    /// An input method is composing text in the box (Enter then belongs to it).
    pub fn composing(&self, window: &mut Window, cx: &mut App) -> bool {
        use gpui::EntityInputHandler as _;
        self.state.update(cx, |state, cx| {
            state.marked_text_range(window, cx).is_some()
        })
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.state.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Whether the box has keyboard focus.
    #[cfg(feature = "render-harness")]
    pub fn focused(&self, window: &Window, cx: &App) -> bool {
        use gpui::Focusable as _;
        self.state.read(cx).focus_handle(cx).is_focused(window)
    }

    /// The input method starts composing `text` in the box (for the render harness).
    #[cfg(feature = "render-harness")]
    pub fn compose(&self, text: &str, window: &mut Window, cx: &mut App) {
        use gpui::EntityInputHandler as _;
        let text = text.to_owned();
        self.state.update(cx, |state, cx| {
            state.replace_and_mark_text_in_range(None, &text, None, window, cx)
        });
    }
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

/// The component library's colours are the design tokens, so a text box is
/// drawn in the window's own palette and not the library's light default.
fn set_theme(cx: &mut App) {
    use gpui_component::{Theme, ThemeMode};
    use tole_desktop_model::tokens::DARK as P;
    Theme::change(ThemeMode::Dark, None, cx);
    let hsla = |c: Rgb| -> Hsla { color(c).into() };
    Theme::update(cx, |theme| {
        theme.colors.background = hsla(P.background);
        theme.colors.foreground = hsla(P.text_primary);
        theme.colors.muted = hsla(P.raised);
        theme.colors.muted_foreground = hsla(P.text_faint);
        theme.colors.caret = hsla(P.accent);
        theme.colors.selection = hsla(P.accent).opacity(0.3);
        theme.colors.border = hsla(P.line);
        theme.colors.input = hsla(P.line);
        theme.colors.ring = hsla(P.accent);
    });
}

/// Sets the application up: the component library and the keyboard focus keys.
/// Runs once before the first window opens, for the real window and for the
/// offscreen harness alike, so both behave the same.
pub fn init(cx: &mut App) {
    // Must run before any gpui-component feature is used.
    gpui_component::init(cx);
    set_theme(cx);
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

        /// Changes the view with the window at hand, then paints.
        pub fn update<R>(
            &mut self,
            f: impl FnOnce(&mut V, &mut Window, &mut Context<V>) -> R,
        ) -> Result<R, String> {
            let view = self.view.clone();
            let out = self
                .cx
                .update_window(self.window, |_, window, cx| {
                    view.update(cx, |view, cx| f(view, window, cx))
                })
                .map_err(|err| err.to_string())?;
            self.draw()?;
            Ok(out)
        }

        /// Paints `frames` more frames: a list measures its rows in one frame
        /// and moves in the next.
        pub fn settle(&mut self, frames: usize) -> Result<(), String> {
            (0..frames).try_for_each(|_| self.draw())
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

        /// Presses and releases a key (`"enter"`) and paints: the release activates a button.
        pub fn tap(&mut self, keystroke: &'static str) -> Result<(), String> {
            let keystroke = gpui::Keystroke::parse(keystroke).map_err(|err| err.to_string())?;
            self.cx
                .update_window(self.window, |_, window, cx| {
                    window.dispatch_keystroke(keystroke.clone(), cx);
                    let up = gpui::PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke });
                    window.dispatch_event(up, cx);
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

        /// Changes the view with its own context (so it can notify), then paints.
        pub fn act(&mut self, f: impl FnOnce(&mut V, &mut Context<V>)) -> Result<(), String> {
            let view = self.view.clone();
            self.cx
                .update_window(self.window, |_, _window, cx| view.update(cx, f))
                .map_err(|err| err.to_string())?;
            self.draw()
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
