//! Directory-relative file access that never follows a symlink (#639).
//!
//! `O_NOFOLLOW` on the session file only refuses a symlink in the **last**
//! path component. A directory on the way (`.tole`, `.tole/sessions` in a
//! job's working directory) that a job swaps for a symlink was still
//! followed, so the `.lock` sidecar could be created somewhere else.
//!
//! [`SafeDir`] closes that. It resolves the directory once (the canonical
//! path has no symlink at that moment), then walks it from `/` one component
//! at a time with `O_NOFOLLOW | O_DIRECTORY`, so a component swapped for a
//! link in between is an error, not a detour. Every file the storage touches
//! afterwards (the session file, its `.lock` sidecar, the compaction temp
//! file and the rename over the live file) is opened *relative to that
//! directory handle*, so a later swap of the path cannot redirect anything.
//!
//! Unix only; elsewhere the same API works on the path as before.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

/// How to open a file (the `OpenOptions` flags the storage uses).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Mode {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub create: bool,
    pub create_new: bool,
    pub truncate: bool,
}

/// `name` is one plain path component: not empty, not `.` or `..`, no `/`
/// and no NUL.
fn plain_name(name: &OsStr) -> io::Result<()> {
    let s = name.as_encoded_bytes();
    if s.is_empty() || s == b"." || s == b".." || s.contains(&b'/') || s.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a plain file name"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    /// An open directory, reached without following a symlink.
    #[derive(Debug)]
    pub(crate) struct SafeDir {
        fd: OwnedFd,
    }

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name holds a NUL"))
    }

    fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    const DIR_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    impl SafeDir {
        /// Resolve `dir` once, then hold it without following a link.
        pub(crate) fn open(dir: &Path) -> io::Result<Self> {
            Self::walk(&dir.canonicalize()?)
        }

        /// The directory of a file path (`.` for a bare name) and the file's
        /// name.
        pub(crate) fn for_file(path: &Path) -> io::Result<(Self, std::ffi::OsString)> {
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            Ok((Self::open(parent)?, name.to_os_string()))
        }

        /// Walk an absolute path from `/` with `O_NOFOLLOW | O_DIRECTORY` at
        /// every component: a link anywhere on it is an error.
        pub(crate) fn walk(path: &Path) -> io::Result<Self> {
            use std::path::Component;
            if !path.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "walk needs an absolute path",
                ));
            }
            let root = cstr(OsStr::new("/"))?;
            // SAFETY: a valid NUL-terminated path and plain flags.
            let mut fd =
                unsafe { OwnedFd::from_raw_fd(check(libc::open(root.as_ptr(), DIR_FLAGS))?) };
            for comp in path.components() {
                match comp {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        let c = cstr(name)?;
                        // SAFETY: `fd` is an open directory, `c` is NUL-terminated.
                        let next = unsafe { libc::openat(fd.as_raw_fd(), c.as_ptr(), DIR_FLAGS) };
                        // SAFETY: `next` is a fresh descriptor we own.
                        fd = unsafe { OwnedFd::from_raw_fd(check(next)?) };
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{} is not a canonical path", path.display()),
                        ))
                    }
                }
            }
            Ok(Self { fd })
        }

        /// Open `name` in this directory without following a link at it,
        /// without waiting for a FIFO's other end, and refusing anything but
        /// a regular file (checked on the opened handle).
        pub(crate) fn open_file(&self, name: &OsStr, m: Mode) -> io::Result<File> {
            plain_name(name)?;
            let c = cstr(name)?;
            let rw = m.read && (m.write || m.append);
            let mut flags = if rw {
                libc::O_RDWR
            } else if m.write || m.append {
                libc::O_WRONLY
            } else {
                libc::O_RDONLY
            };
            flags |= libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
            if m.append {
                flags |= libc::O_APPEND;
            }
            if m.create_new {
                flags |= libc::O_CREAT | libc::O_EXCL;
            } else if m.create {
                flags |= libc::O_CREAT;
            }
            if m.truncate {
                flags |= libc::O_TRUNC;
            }
            // SAFETY: `fd` is an open directory, `c` is NUL-terminated; the
            // mode is only read when O_CREAT is set.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    flags,
                    0o666 as libc::c_uint,
                )
            };
            // SAFETY: `fd` is a fresh descriptor we own.
            let f = unsafe { File::from_raw_fd(check(fd)?) };
            if !f.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name:?} is not a regular file"),
                ));
            }
            Ok(f)
        }

        /// `rename(from, to)` inside this directory.
        pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            plain_name(from)?;
            plain_name(to)?;
            let (a, b) = (cstr(from)?, cstr(to)?);
            let d = self.fd.as_raw_fd();
            // SAFETY: both names are NUL-terminated, `d` is an open directory.
            check(unsafe { libc::renameat(d, a.as_ptr(), d, b.as_ptr()) }).map(|_| ())
        }

        /// Flush the directory entry (a rename) to disk.
        pub(crate) fn sync(&self) -> io::Result<()> {
            // SAFETY: `fd` is an open descriptor.
            check(unsafe { libc::fsync(self.fd.as_raw_fd()) }).map(|_| ())
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;
    use std::path::PathBuf;

    /// A directory by path: no handle semantics off unix.
    #[derive(Debug)]
    pub(crate) struct SafeDir {
        path: PathBuf,
    }

    impl SafeDir {
        pub(crate) fn open(dir: &Path) -> io::Result<Self> {
            Ok(Self {
                path: dir.canonicalize()?,
            })
        }

        pub(crate) fn for_file(path: &Path) -> io::Result<(Self, std::ffi::OsString)> {
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            Ok((Self::open(parent)?, name.to_os_string()))
        }

        pub(crate) fn open_file(&self, name: &OsStr, m: Mode) -> io::Result<File> {
            plain_name(name)?;
            let mut o = std::fs::OpenOptions::new();
            o.read(m.read)
                .write(m.write)
                .append(m.append)
                .create(m.create)
                .create_new(m.create_new)
                .truncate(m.truncate);
            let f = o.open(self.path.join(name))?;
            if !f.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name:?} is not a regular file"),
                ));
            }
            Ok(f)
        }

        pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            plain_name(from)?;
            plain_name(to)?;
            std::fs::rename(self.path.join(from), self.path.join(to))
        }

        pub(crate) fn sync(&self) -> io::Result<()> {
            Ok(())
        }
    }
}

pub(crate) use imp::SafeDir;

#[cfg(test)]
#[path = "safe_dir_tests.rs"]
mod tests;
