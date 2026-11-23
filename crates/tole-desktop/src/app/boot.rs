//! Where the app starts: the folder, and the private directory of the serve.
//!
//! The folder the owner starts the app in is the first working folder: the
//! local serve is started with it as its workspace. The runtime directory
//! (socket, locks, credential file) is the supervisor's own default
//! ([`default_runtime_dir`]): `$XDG_RUNTIME_DIR/tole-desktop` when that is an
//! absolute path, else `<temp dir>/tole-desktop-<uid>`. The user id comes from
//! the supervisor's own `euid`, so this adds no dependency. Nothing here
//! touches the disk: the supervisor creates the directory and refuses one that
//! is not the owner's alone.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::supervisor::sys::euid;
use crate::supervisor::{default_runtime_dir, Config};

/// What the app needs to know to start the serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The folder the app was started in: the serve's workspace.
    pub workspace: PathBuf,
    /// The private directory of the serve.
    pub runtime_dir: PathBuf,
}

impl Boot {
    /// From the process: its current directory, `XDG_RUNTIME_DIR`, the
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
            &std::env::temp_dir(),
            euid(),
        ))
    }

    /// The pure part of [`Boot::from_env`].
    pub fn from_parts(
        workspace: PathBuf,
        xdg_runtime_dir: Option<OsString>,
        temp_dir: &Path,
        uid: u32,
    ) -> Boot {
        let xdg = xdg_runtime_dir.map(PathBuf::from);
        Boot {
            workspace,
            runtime_dir: default_runtime_dir(xdg.as_deref(), temp_dir, uid),
        }
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

    #[test]
    fn the_runtime_dir_is_under_an_absolute_xdg_dir() {
        let boot = Boot::from_parts(
            PathBuf::from("/home/me/project"),
            Some("/run/user/1000".into()),
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
            let boot = Boot::from_parts(PathBuf::from("/w"), xdg, Path::new("/var/tmp"), 501);
            assert_eq!(boot.runtime_dir, PathBuf::from("/var/tmp/tole-desktop-501"));
        }
    }

    #[test]
    fn the_supervisor_gets_the_folder_and_leaves_the_serve_running_on_close() {
        let boot = Boot::from_parts(PathBuf::from("/w"), None, Path::new("/tmp"), 7);
        let cfg = boot.supervisor_config();
        assert_eq!(cfg.workspace, PathBuf::from("/w"));
        assert_eq!(cfg.runtime_dir, boot.runtime_dir);
        assert_eq!(cfg.close_behavior, CloseBehavior::LeaveRunning);
        assert!(!cfg.cron);
    }

    #[test]
    fn from_env_reads_the_current_folder() {
        let boot = Boot::from_env().expect("a test runs in a folder");
        assert_eq!(boot.workspace, std::env::current_dir().unwrap());
    }
}
