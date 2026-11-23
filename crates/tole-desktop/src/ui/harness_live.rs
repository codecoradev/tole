//! The shell as the app drives it: the connection, the counts and the notice of
//! a serve that is not ready, one PNG each (`live-*.png`).
//!
//! The model's own captures (`shell_captures`) are the shell with nothing
//! behind it. These are the states the window gets from the app, each built as
//! the app builds it: the shell state and the [`Status`] beside it.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use tole_desktop_model::{Action, Badge, ServeFailure, ServeState, ShellState};

use super::adapter::offscreen::Offscreen;
use super::harness::{Rendered, WindowSize};
use super::shell::ShellView;
use super::status::Status;

/// The board these are compared with: the shell's, plus the notice, which the
/// boards do not have (it is the one new element).
const BOARD: &str = "S2 plus the serve notice (not on a board)";

/// One image to render: the state the app gave the window.
pub struct LiveCapture {
    /// File stem of the PNG.
    pub name: &'static str,
    /// Keystrokes pressed before the image is taken.
    pub keys: &'static str,
    pub state: ShellState,
    pub status: Status,
}

fn running() -> ServeState {
    ServeState::Running {
        version: "0.12.0".into(),
    }
}

/// A shell with the folder the app was started in, showing `serve` and the
/// given counts (`None`: not known).
fn shell_of(serve: &ServeState, approvals: Option<usize>, skills: Option<usize>) -> LiveCapture {
    let mut state = ShellState::new();
    state.apply(Action::AddFolder {
        name: "project".to_owned(),
        path: "/home/me/project".to_owned(),
    });
    state.apply(Action::SetConnectionStatus(serve.connection_status()));
    state.apply(Action::SetApprovalsPending(
        approvals.map_or(Badge::Unknown, Badge::Count),
    ));
    state.apply(Action::SetSkillsNew(
        skills.map_or(Badge::Unknown, Badge::Count),
    ));
    LiveCapture {
        name: "",
        keys: "",
        state,
        status: Status {
            serve: serve.clone(),
            skills_unknown: skills.is_none(),
            actionable: true,
        },
    }
}

fn named(name: &'static str, mut capture: LiveCapture) -> LiveCapture {
    capture.name = name;
    capture
}

/// Where the notice's button is in the Tab order: the sidebar toggle, six
/// sections and the folder come first.
const TABS_TO_THE_BUTTON: &str = "tab tab tab tab tab tab tab tab tab";

/// Every state the app can give the window that is worth a look.
pub fn live_captures() -> Vec<LiveCapture> {
    let gave_up = ServeState::Failed(ServeFailure::GaveUp {
        attempts: 6,
        detail: "exit status 1".to_owned(),
    });
    let mismatch = ServeState::Failed(ServeFailure::VersionMismatch {
        found: Some(1),
        expected: 2,
    });
    let restarting = ServeState::Restarting {
        attempt: 2,
        retry_in: Duration::from_secs(4),
    };
    let failed = Status::failed_to_start("cannot start a thread: out of memory".to_owned());
    let mut not_started = named("live-app-not-started", shell_of(&failed.serve, None, None));
    not_started.status = failed;
    let mut focused = named("live-failed-retry-focused", shell_of(&gave_up, None, None));
    focused.keys = TABS_TO_THE_BUTTON;
    vec![
        named("live-starting", shell_of(&ServeState::Starting, None, None)),
        named(
            "live-connected-counts",
            shell_of(&running(), Some(3), Some(2)),
        ),
        named(
            "live-connected-skills-unknown",
            shell_of(&running(), Some(1), None),
        ),
        named("live-restarting", shell_of(&restarting, None, None)),
        named("live-failed-retry", shell_of(&gave_up, None, None)),
        focused,
        named("live-failed-restart", shell_of(&mismatch, None, None)),
        named(
            "live-failed-no-binary",
            shell_of(
                &ServeState::Failed(ServeFailure::BinaryNotFound),
                None,
                None,
            ),
        ),
        not_started,
    ]
}

/// Renders one capture into `dir`.
fn render(capture: LiveCapture, dir: &Path, window: WindowSize) -> Result<Rendered, String> {
    let at = |what: &str, err: String| format!("{}: {what}: {err}", capture.name);
    let (name, keys) = (capture.name, capture.keys);
    let (state, status) = (capture.state, capture.status);
    let mut offscreen = Offscreen::open(window.width, window.height, move |_cx| {
        ShellView::with_status(state, status)
    })
    .map_err(|e| at("open", e))?;
    for key in keys.split_whitespace() {
        offscreen.press(key).map_err(|e| at("press", e))?;
    }
    let path = dir.join(format!("{name}.png"));
    let (width, height, rgba) = offscreen.write_png(&path).map_err(|e| at("write", e))?;
    let bytes = fs::metadata(&path)
        .map_err(|e| at("stat", e.to_string()))?
        .len();
    Ok(Rendered {
        name,
        board: BOARD,
        keys,
        path,
        width,
        height,
        bytes,
        rgba,
    })
}

/// Renders every live capture into `dir` (which must exist) and adds a line
/// for each to `index.tsv`, when there is one.
pub fn render_live(dir: &Path, window: WindowSize) -> Result<Vec<Rendered>, String> {
    let mut done = Vec::new();
    for capture in live_captures() {
        done.push(render(capture, dir, window)?);
    }
    let index = dir.join("index.tsv");
    if index.exists() {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&index)
            .map_err(|e| format!("{}: {e}", index.display()))?;
        for r in &done {
            writeln!(
                file,
                "{}.png\t{}\t{}\t{}\t{}\t{}",
                r.name, r.board, r.keys, r.width, r.height, r.bytes
            )
            .map_err(|e| format!("{}: {e}", index.display()))?;
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_desktop_model::ConnectionStatus;

    #[test]
    fn every_capture_has_a_distinct_name_and_a_status_that_matches_its_dot() {
        let captures = live_captures();
        let mut names: Vec<_> = captures.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), captures.len());
        for c in &captures {
            assert!(c.name.starts_with("live-"));
            let dot = c.state.connection.status;
            assert_eq!(dot, c.status.serve.connection_status(), "{}", c.name);
            // A serve that is not ready has no known count.
            if !c.status.serve.is_ready() {
                assert_eq!(c.state.approvals_pending, Badge::Unknown, "{}", c.name);
                assert!(c.status.skills_unknown, "{}", c.name);
                assert_eq!(c.state.skills_new, Badge::Unknown, "{}", c.name);
            }
        }
        assert!(captures
            .iter()
            .any(|c| c.state.connection.status == ConnectionStatus::Connected));
    }
}
