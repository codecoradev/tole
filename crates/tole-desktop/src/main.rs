//! The `tole-desktop` binary: opens the window, or says why it cannot.
//!
//! On macOS and Linux the window shows the real local serve: the app starts it
//! for the folder the binary is run from (see `tole_desktop::app`). Elsewhere
//! there is no serve supervisor yet, and the window is a preview with sample
//! data.

use std::process::ExitCode;

use tole_desktop::cli::{self, Command};

fn main() -> ExitCode {
    match cli::parse(std::env::args().skip(1)) {
        Ok(Command::Help) => {
            println!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("{}", cli::version());
            ExitCode::SUCCESS
        }
        Ok(Command::Run) => run(),
        Err(why) => {
            eprintln!("tole-desktop: {why}\n{}", cli::USAGE);
            ExitCode::from(2)
        }
    }
}

#[cfg(unix)]
fn run() -> ExitCode {
    use std::cell::RefCell;
    use std::rc::Rc;

    use tole_desktop::app::{App, Boot, Limits};
    use tole_desktop::ui::{self, Source};

    let boot = match Boot::from_env() {
        Ok(boot) => boot,
        Err(err) => {
            eprintln!("tole-desktop: {err}");
            return ExitCode::FAILURE;
        }
    };
    // The window starts the app once its display is up (a machine with no
    // display never starts a serve) and lets go of it when it closes; this
    // keeps the other handle, to end the app in order afterwards.
    let started: Rc<RefCell<Option<Rc<App>>>> = Rc::default();
    let slot = Rc::clone(&started);
    let start_boot = boot.clone();
    let shown = ui::run(boot.shell(), move || {
        let app = App::start(&start_boot).map_err(|err| err.to_string())?;
        let app = Rc::new(app);
        *slot.borrow_mut() = Some(Rc::clone(&app));
        Ok(Box::new(app) as Box<dyn Source>)
    });
    // The window is closed. The backend stops, then the serve is let go of
    // (it keeps running), each within a bound. When the toolkit still holds
    // its handle the process ends right after, which ends the backend's
    // threads and leaves the serve running.
    let app = started.borrow_mut().take();
    if let Some(Ok(app)) = app.map(Rc::try_unwrap) {
        let _ = app.quit(Limits::default());
    }
    finish(shown)
}

#[cfg(not(unix))]
fn run() -> ExitCode {
    use tole_desktop::ui;
    use tole_desktop_model::ShellState;

    finish(ui::preview(ShellState::placeholder()))
}

fn finish(shown: Result<(), tole_desktop::ui::UiError>) -> ExitCode {
    match shown {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tole-desktop: {err}");
            ExitCode::FAILURE
        }
    }
}
