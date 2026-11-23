//! Where the app starts: the folder, and the private directory of the serve.
//!
//! The folder the owner starts the app in is the first working folder: the
//! local serve is started with it as its workspace. The runtime directory
//! (socket, locks, credential file) is the supervisor's own default
//! ([`default_runtime_dir`]): `$XDG_RUNTIME_DIR/tole-desktop` when that is an
//! absolute path, else `<temp dir>/tole-desktop-<uid>`. The user id comes from
//! the supervisor's own `euid`, so this adds no dependency. Nothing here
//! writes to the disk: the supervisor creates the directory and refuses one
//! that is not the owner's alone.
//!
//! A file manager or a launcher starts an app in `/` or in the home folder.
//! Neither is a folder to work in, and a serve rooted there could read the
//! whole disk or the whole home, so [`Boot::refusal`] names them and
//! [`App::start`](super::App::start) refuses to start a serve for them: the
//! window opens and says so.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use tole_desktop_model::strings;

use crate::supervisor::sys::euid;
use crate::supervisor::{default_runtime_dir, Config};

/// A folder that is not a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    /// `/`.
    Root,
    /// The owner's home folder.
    Home,
}

/// Why [`App::start`](super::App::start) did not start.
#[derive(Debug)]
pub enum StartError {
    /// The folder the app was started in is not a workspace. No serve was
    /// started and nothing is running.
    Workspace(Workspace),
    /// The system could not start the backend's threads. Nothing is running.
    Threads(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Workspace(Workspace::Root) => f.write_str(strings::WORKSPACE_IS_ROOT),
            StartError::Workspace(Workspace::Home) => f.write_str(strings::WORKSPACE_IS_HOME),
            StartError::Threads(e) => f.write_str(&strings::start_threads_failed(&e.to_string())),
        }
    }
}

impl std::error::Error for StartError {}

/// What the app needs to know to start the serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The folder the app was started in: the serve's workspace.
    pub workspace: PathBuf,
    /// The private directory of the serve.
    pub runtime_dir: PathBuf,
    /// The owner's home folder, when the environment names one.
    pub home: Option<PathBuf>,
}

fn same_folder(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(x), Ok(y)) if x == y
        )
}

impl Boot {
    /// From the process: its current directory, `XDG_RUNTIME_DIR`, `HOME`, the
    /// temporary directory and the user id. `Err` only when the current
    /// directory cannot be read (it was removed under the app, or is not
    /// reachable): the app then has no first folder to give the serve.
    pub fn from_env() -> io::Result<Boot> {
        let workspace = std::env::current_dir().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot read the folder the app was started in: {e}"),
            )
        })?;
        Ok(Boot::from_parts(
            workspace,
            std::env::var_os("XDG_RUNTIME_DIR"),
            std::env::var_os("HOME"),
            &std::env::temp_dir(),
            euid(),
        ))
    }

    /// The pure part of [`Boot::from_env`].
    pub fn from_parts(
        workspace: PathBuf,
        xdg_runtime_dir: Option<OsString>,
        home: Option<OsString>,
        temp_dir: &Path,
        uid: u32,
    ) -> Boot {
        let xdg = xdg_runtime_dir.map(PathBuf::from);
        Boot {
            workspace,
            runtime_dir: default_runtime_dir(xdg.as_deref(), temp_dir, uid),
            home: home.filter(|h| !h.is_empty()).map(PathBuf::from),
        }
    }

    /// Whether the folder the app was started in cannot be a workspace. Reads
    /// the disk only to follow links (a link to `/` is `/`).
    pub fn refusal(&self) -> Option<Workspace> {
        let resolved = self.workspace.canonicalize();
        let root = resolved.as_deref().unwrap_or(&self.workspace).parent();
        if root.is_none() {
            return Some(Workspace::Root);
        }
        let home = self.home.as_deref()?;
        same_folder(&self.workspace, home).then_some(Workspace::Home)
    }

    /// The supervisor's configuration: every other setting is its default (the
    /// serve keeps running when the window closes, no scheduler in it).
    pub fn supervisor_config(&self) -> Config {
        Config::new(self.workspace.clone(), self.runtime_dir.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::CloseBehavior;

    fn boot(workspace: &str, home: Option<&str>) -> Boot {
        Boot::from_parts(
            PathBuf::from(workspace),
            None,
            home.map(OsString::from),
            Path::new("/tmp"),
            7,
        )
    }

    #[test]
    fn the_runtime_dir_is_under_an_absolute_xdg_dir() {
        let boot = Boot::from_parts(
            PathBuf::from("/home/me/project"),
            Some("/run/user/1000".into()),
            None,
            Path::new("/tmp"),
            1000,
        );
        assert_eq!(boot.workspace, PathBuf::from("/home/me/project"));
        assert_eq!(
            boot.runtime_dir,
            PathBuf::from("/run/user/1000/tole-desktop")
        );
    }

    #[test]
    fn without_a_usable_xdg_dir_it_is_in_the_temp_dir_named_for_the_user() {
        for xdg in [
            None,
            Some(OsString::from("relative")),
            Some(OsString::new()),
        ] {
            let boot = Boot::from_parts(PathBuf::from("/w"), xdg, None, Path::new("/var/tmp"), 501);
            assert_eq!(boot.runtime_dir, PathBuf::from("/var/tmp/tole-desktop-501"));
        }
    }

    #[test]
    fn the_supervisor_gets_the_folder_and_leaves_the_serve_running_on_close() {
        let boot = boot("/w", None);
        let cfg = boot.supervisor_config();
        assert_eq!(cfg.workspace, PathBuf::from("/w"));
        assert_eq!(cfg.runtime_dir, boot.runtime_dir);
        assert_eq!(cfg.close_behavior, CloseBehavior::LeaveRunning);
        assert!(!cfg.cron);
    }

    #[test]
    fn the_top_of_the_disk_and_the_home_folder_are_not_workspaces() {
        assert_eq!(boot("/", None).refusal(), Some(Workspace::Root));
        assert_eq!(boot("/", Some("/home/me")).refusal(), Some(Workspace::Root));
        assert_eq!(
            boot("/home/me", Some("/home/me")).refusal(),
            Some(Workspace::Home)
        );
        // A folder below either is fine, and so is any folder when HOME is not set.
        assert_eq!(boot("/home/me/project", Some("/home/me")).refusal(), None);
        assert_eq!(boot("/home", Some("/home/me")).refusal(), None);
        assert_eq!(boot("/home/me", None).refusal(), None);
        // An empty HOME names nothing.
        assert_eq!(boot("/home/me", Some("")).refusal(), None);
    }

    #[test]
    fn a_link_to_the_home_folder_or_to_root_is_that_folder() {
        let dir = std::env::temp_dir().join(format!("tdboot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::os::unix::fs::symlink(dir.join("home"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink("/", dir.join("rootlink")).unwrap();
        let home = dir.join("home");
        let at = |p: PathBuf| {
            Boot::from_parts(p, None, Some(home.clone().into()), Path::new("/tmp"), 7).refusal()
        };
        assert_eq!(at(dir.join("link")), Some(Workspace::Home));
        assert_eq!(at(dir.join("rootlink")), Some(Workspace::Root));
        assert_eq!(at(dir.clone()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_says_why_in_the_models_words() {
        let root = StartError::Workspace(Workspace::Root).to_string();
        let home = StartError::Workspace(Workspace::Home).to_string();
        assert_eq!(root, strings::WORKSPACE_IS_ROOT);
        assert_eq!(home, strings::WORKSPACE_IS_HOME);
        let threads = StartError::Threads(io::Error::other("no threads")).to_string();
        assert!(threads.contains("no threads"), "{threads}");
    }

    #[test]
    fn from_env_reads_the_current_folder() {
        let boot = Boot::from_env().expect("a test runs in a folder");
        assert_eq!(boot.workspace, std::env::current_dir().unwrap());
    }
}
