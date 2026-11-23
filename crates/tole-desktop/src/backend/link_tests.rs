//! `SupervisorLink` over a real supervisor that has no serve to use: its
//! directory is not private, so nothing is started and the state says why.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tole_desktop_model::{Action, Badge, ConnectionStatus, ServeFailure, ServeState, ShellState};

use super::shell::apply;
use super::{Backend, Config, ServeLink, SupervisorLink};
use crate::driver::clock::SystemClock;
use crate::supervisor::Config as Launch;

/// A directory anyone can enter: the supervisor refuses to put a socket in it.
struct Open(PathBuf);

impl Open {
    fn new(tag: &str) -> Open {
        let dir = std::env::temp_dir().join(format!("tole-link-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("modes");
        Open(dir)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_supervisor_that_cannot_use_its_directory_is_a_link_with_no_api() {
    let dir = Open::new("state");
    let link = SupervisorLink::launch(Launch::new(&dir.0, &dir.0));
    assert!(
        matches!(
            link.state(),
            ServeState::Failed(ServeFailure::DirectoryNotPrivate { .. })
        ),
        "{:?}",
        link.state()
    );
    assert!(link.api().is_none(), "no client, so no api, and no panic");
    assert!(
        link.epoch() >= 2,
        "Starting, then the failure, were counted"
    );
    assert_eq!(link.supervisor().state(), link.state());
}

#[test]
fn a_backend_over_it_shows_the_failure_and_attaches_nothing() {
    let dir = Open::new("backend");
    let link = Arc::new(SupervisorLink::launch(Launch::new(&dir.0, &dir.0)));
    let backend =
        Backend::start(link.clone(), Arc::new(SystemClock), Config::default()).expect("start");
    std::thread::sleep(Duration::from_millis(350)); // a few real ticks
    let view = backend.snapshot();
    assert!(matches!(view.serve, ServeState::Failed(_)));
    assert!(backend.with_approvals(|_| ()).is_none());
    let mut shell = ShellState::new();
    shell.apply(Action::SetConnectionStatus(ConnectionStatus::Preview));
    apply(&mut shell, &view);
    assert_eq!(shell.connection.status, ConnectionStatus::Unreachable);
    assert_eq!(shell.skills_new, Badge::Unknown);
    assert_eq!(
        backend.thread_report(),
        Default::default(),
        "nothing was started"
    );
    let report = backend.shutdown(Duration::from_secs(2));
    assert_eq!((report.panicked, report.detached), (0, 0), "{report:?}");
    assert_eq!(report.joined, 2, "the worker and the ticker");
}
