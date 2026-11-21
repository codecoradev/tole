//! The few operating-system calls the supervisor needs, in one place (macOS
//! and Linux): the owner of the process, a private directory, `flock`, signals
//! and the process that listens on a socket.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

/// The effective user id of this process.
pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Create `dir` `0700` when it is missing, then require what a private
/// directory is: not a link, a directory, owned by this user, with no
/// permission for group or other. A directory that is too wide is refused, not
/// repaired: it may hold other things, and the owner decides. The error is the
/// reason, in words.
pub fn prepare_private_dir(dir: &Path) -> Result<(), String> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("cannot create it: {e}"))?;
            std::fs::symlink_metadata(dir).map_err(|e| format!("cannot read it: {e}"))?
        }
        Err(e) => return Err(format!("cannot read it: {e}")),
    };
    if meta.file_type().is_symlink() {
        return Err("it is a symbolic link, which is not followed".into());
    }
    if !meta.is_dir() {
        return Err("it is not a directory".into());
    }
    if meta.uid() != euid() {
        return Err("it belongs to another user".into());
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "its mode is {mode:04o}; it must be 0700 (owner only): chmod 700 {}",
            dir.display()
        ));
    }
    Ok(())
}

/// Open a lock file of this user without following a link: created `0600`
/// when `create` is set, and required to be a regular file of this user.
pub fn open_lock_file(path: &Path, create: bool) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a regular file of this user", path.display()),
        ));
    }
    Ok(file)
}

/// Try the exclusive `flock` of `file`: `Ok(true)` when taken (until `file` is
/// closed), `Ok(false)` when another process holds it.
pub fn try_lock(file: &File) -> io::Result<bool> {
    // SAFETY: the descriptor belongs to `file`, which outlives the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(e)
    }
}

/// The launch lock: one desktop at a time decides whether to attach or start,
/// so two launches cannot both start a serve or overwrite each other's
/// credential file. Released when dropped (or when the process ends).
#[derive(Debug)]
pub struct LaunchLock {
    _file: File,
}

impl LaunchLock {
    /// Take the lock, waiting up to `wait` for another launch to finish.
    pub fn acquire(path: &Path, wait: Duration) -> io::Result<LaunchLock> {
        let file = open_lock_file(path, true)?;
        let deadline = Instant::now() + wait;
        loop {
            if try_lock(&file)? {
                return Ok(LaunchLock { _file: file });
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "another launch of the desktop is still starting the serve",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Take the start-up lock of the serve on `lock_path` (`<socket>.lock`, held
/// by the serve for as long as it lives). `Some` means no serve process holds
/// it, which proves none is running: the file is held until dropped, so a
/// caller can clean up behind a serve that is gone. `None` means a serve holds
/// it. A lock file that does not exist yet means no serve has ever run there:
/// `Some(None)`.
pub fn take_serve_lock(lock_path: &Path) -> io::Result<Option<Option<File>>> {
    match open_lock_file(lock_path, false) {
        Ok(file) => Ok(try_lock(&file)?.then_some(Some(file))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Some(None)),
        Err(e) => Err(e),
    }
}

/// Whether no serve process holds `lock_path`.
pub fn serve_lock_is_free(lock_path: &Path) -> io::Result<bool> {
    Ok(take_serve_lock(lock_path)?.is_some())
}

/// Ask a process to stop (`SIGTERM`).
pub fn terminate(pid: u32) -> io::Result<()> {
    signal(pid, libc::SIGTERM)
}

/// Stop a process now (`SIGKILL`).
pub fn kill(pid: u32) -> io::Result<()> {
    signal(pid, libc::SIGKILL)
}

fn signal(pid: u32, sig: libc::c_int) -> io::Result<()> {
    // Never the desktop itself, never init, never a process group (a pid that
    // does not fit in the positive range would become one).
    if pid <= 1 || pid == std::process::id() || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal process {pid}"),
        ));
    }
    // SAFETY: plain kill(2) on a checked positive pid.
    if unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The process that listens on the socket at `path`, from the credentials the
/// kernel records for a listening socket. On Linux it must also belong to this
/// user. `None` where the system does not say (or on any failure).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn socket_owner_pid(path: &Path) -> Option<u32> {
    let stream = std::os::unix::net::UnixStream::connect(path).ok()?;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the size passed; the descriptor
    // belongs to `stream`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.uid == euid() && cred.pid > 0).then_some(cred.pid as u32)
}

#[cfg(target_os = "macos")]
pub fn socket_owner_pid(path: &Path) -> Option<u32> {
    // sys/un.h: SOL_LOCAL = 0, LOCAL_PEERPID = 0x002.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;
    let stream = std::os::unix::net::UnixStream::connect(path).ok()?;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid` and `len` are valid for the size passed; the descriptor
    // belongs to `stream`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            SOL_LOCAL,
            LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    (rc == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub fn socket_owner_pid(_path: &Path) -> Option<u32> {
    None
}

/// `n` bytes from the system's random source.
pub fn random_bytes(n: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tdsys-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    #[test]
    fn a_missing_directory_is_created_owner_only_and_a_wide_one_is_refused_not_repaired() {
        let root = scratch("dir");
        let new = root.join("a/b");
        prepare_private_dir(&new).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&new), 0o700);

        for wide in [0o770, 0o755, 0o707, 0o701] {
            let dir = root.join(format!("w{wide:o}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(wide)).unwrap();
            let why = prepare_private_dir(&dir).unwrap_err();
            assert!(
                why.contains("0700") && why.contains(&format!("{wide:04o}")),
                "{why}"
            );
            assert_eq!(mode(&dir), wide, "the directory is left as it was");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_linked_directory_and_a_plain_file_are_refused() {
        let root = scratch("link");
        let real = root.join("real");
        prepare_private_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        assert!(prepare_private_dir(&root.join("link"))
            .unwrap_err()
            .contains("symbolic link"));
        std::fs::write(root.join("file"), "x").unwrap();
        assert!(prepare_private_dir(&root.join("file"))
            .unwrap_err()
            .contains("not a directory"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_launch_lock_is_exclusive_until_dropped() {
        let root = scratch("lock");
        let path = root.join("desktop.lock");
        let first = LaunchLock::acquire(&path, Duration::from_millis(50)).unwrap();
        let err = LaunchLock::acquire(&path, Duration::from_millis(60)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(first);
        assert!(LaunchLock::acquire(&path, Duration::from_millis(50)).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_held_serve_lock_is_not_free_and_a_missing_one_is() {
        let root = scratch("serve");
        let path = root.join("s.sock.lock");
        assert!(
            serve_lock_is_free(&path).unwrap(),
            "no lock file: no serve ever ran"
        );
        let held = open_lock_file(&path, true).unwrap();
        assert!(try_lock(&held).unwrap());
        assert!(!serve_lock_is_free(&path).unwrap());
        drop(held);
        assert!(serve_lock_is_free(&path).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn signals_are_never_sent_to_init_this_process_or_a_process_group() {
        for pid in [0, 1, std::process::id(), u32::MAX] {
            assert!(terminate(pid).is_err(), "{pid}");
        }
    }

    #[test]
    fn random_bytes_differ_between_calls() {
        assert_ne!(random_bytes(32).unwrap(), random_bytes(32).unwrap());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_owner_of_a_listening_socket_is_found() {
        let root = scratch("peer");
        let path = root.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(socket_owner_pid(&path), Some(std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
