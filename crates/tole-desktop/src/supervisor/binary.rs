//! Which `tole` binary the desktop starts: a configured path, else the one
//! next to the desktop binary (the bundled one), else the first on `PATH`.
//! Always an absolute path to an executable file, never one relative to the
//! working directory: the desktop must not start whatever a folder it happens
//! to be in provides.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The name of the binary.
pub const NAME: &str = "tole";

/// Where a binary was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Configured,
    NextToDesktop,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryError {
    /// The configured path is not usable; it is not replaced by another one.
    Configured { path: PathBuf, reason: &'static str },
    /// Nothing named `tole` next to the desktop or on `PATH`.
    NotFound,
}

/// Find the binary. `exe_dir` is the directory of the desktop binary and
/// `path_var` the value of `PATH`, passed in so the choice is tested without
/// touching the environment of the test process.
pub fn resolve(
    configured: Option<&Path>,
    exe_dir: Option<&Path>,
    path_var: Option<&OsStr>,
) -> Result<(PathBuf, Source), BinaryError> {
    if let Some(path) = configured {
        return match usable(path) {
            Ok(()) => Ok((path.to_owned(), Source::Configured)),
            Err(reason) => Err(BinaryError::Configured {
                path: path.to_owned(),
                reason,
            }),
        };
    }
    if let Some(dir) = exe_dir.filter(|d| d.is_absolute()) {
        let candidate = dir.join(NAME);
        if usable(&candidate).is_ok() {
            return Ok((candidate, Source::NextToDesktop));
        }
    }
    if let Some(path_var) = path_var {
        // A relative entry (empty, `.`, `bin`) means "relative to the working
        // directory": skipped.
        for dir in std::env::split_paths(path_var).filter(|d| d.is_absolute()) {
            let candidate = dir.join(NAME);
            if usable(&candidate).is_ok() {
                return Ok((candidate, Source::Path));
            }
        }
    }
    Err(BinaryError::NotFound)
}

/// [`resolve`] for the running desktop: its own directory and the real `PATH`.
pub fn resolve_here(configured: Option<&Path>) -> Result<(PathBuf, Source), BinaryError> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_owned));
    let path_var: Option<OsString> = std::env::var_os("PATH");
    resolve(configured, exe_dir.as_deref(), path_var.as_deref())
}

fn usable(path: &Path) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("the path is not absolute");
    }
    let meta = std::fs::metadata(path).map_err(|_| "there is no file at that path")?;
    if !meta.is_file() {
        return Err("it is not a file");
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err("it is not executable");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tdbin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tole_in(dir: &Path, mode: u32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(NAME);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn the_configured_path_wins_over_the_bundled_one_and_over_path() {
        let root = scratch("cfg");
        let configured = tole_in(&root.join("mine"), 0o755);
        let bundled = tole_in(&root.join("app"), 0o755);
        let on_path = tole_in(&root.join("bin"), 0o755);
        let path_var = std::env::join_paths([on_path.parent().unwrap()]).unwrap();
        let got = resolve(Some(&configured), bundled.parent(), Some(&path_var)).unwrap();
        assert_eq!(got, (configured, Source::Configured));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_one_next_to_the_desktop_wins_over_path() {
        let root = scratch("app");
        let bundled = tole_in(&root.join("app"), 0o755);
        let on_path = tole_in(&root.join("bin"), 0o755);
        let path_var = std::env::join_paths([on_path.parent().unwrap()]).unwrap();
        let got = resolve(None, bundled.parent(), Some(&path_var)).unwrap();
        assert_eq!(got, (bundled, Source::NextToDesktop));
        let got = resolve(None, Some(&root.join("empty")), Some(&path_var)).unwrap();
        assert_eq!(got, (on_path, Source::Path));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_configured_path_that_does_not_work_is_an_error_not_a_fallback() {
        let root = scratch("bad");
        let bundled = tole_in(&root.join("app"), 0o755);
        for (path, why) in [
            (root.join("missing"), "there is no file"),
            (tole_in(&root.join("noexec"), 0o644), "not executable"),
            (root.clone(), "not a file"),
            (PathBuf::from("tole"), "not absolute"),
            (PathBuf::from("./tole"), "not absolute"),
        ] {
            let err = resolve(Some(&path), bundled.parent(), None).unwrap_err();
            match err {
                BinaryError::Configured { reason, .. } => assert!(reason.contains(why), "{reason}"),
                other => panic!("{other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_relative_path_entry_and_a_relative_desktop_dir_are_never_used() {
        let root = scratch("rel");
        let here = std::env::current_dir().unwrap();
        // A `tole` in the working directory must not be picked up through ".",
        // "" or a relative entry.
        let local = tole_in(
            &here.join(format!("tdbin-rel-{}", std::process::id())),
            0o755,
        );
        let rel_dir = local.parent().unwrap().file_name().unwrap().to_owned();
        let path_var = OsString::from(format!(":.:{}", rel_dir.to_string_lossy()));
        let got = resolve(None, Some(Path::new(&rel_dir)), Some(&path_var));
        std::fs::remove_dir_all(local.parent().unwrap()).unwrap();
        assert_eq!(got, Err(BinaryError::NotFound));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_found_is_not_found() {
        assert_eq!(resolve(None, None, None), Err(BinaryError::NotFound));
        let root = scratch("none");
        let path_var = std::env::join_paths([&root]).unwrap();
        assert_eq!(
            resolve(None, Some(&root), Some(&path_var)),
            Err(BinaryError::NotFound)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
