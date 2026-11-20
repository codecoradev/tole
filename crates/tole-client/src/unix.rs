//! The local socket transport (macOS, Linux): `tole serve --socket <path>`
//! (#1043) from the client side (#1067).
//!
//! The boundary of a local socket is the filesystem: serve creates the
//! directory holding it `0700` and refuses one that is wider, owned by someone
//! else or a link. The client applies the same test before it sends anything,
//! because the bearer token goes to whatever listens on the path: a directory
//! another user can write to is a directory where that user can replace the
//! socket and read the token (the squatter of a loopback port).

use crate::error::{Error, Result};
use crate::transport::{Connection, Transport};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// Longest socket path `connect` accepts (`sun_path`, one byte kept for the NUL).
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SOCKET_PATH_MAX: usize = 103;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const SOCKET_PATH_MAX: usize = 107;

impl Connection for UnixStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        UnixStream::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        UnixStream::set_write_timeout(self, timeout)
    }
}

/// HTTP over a Unix domain socket.
#[derive(Debug, Clone)]
pub struct UnixTransport {
    path: PathBuf,
    connect_timeout: Duration,
}

impl UnixTransport {
    /// `path` is the socket `tole serve --socket` listens on.
    ///
    /// The directory holding it must be a real directory (not a link), owned
    /// by the caller and with no permission for group or other (`0700`); the
    /// socket must be a socket (not a link) owned by the caller. Anything else
    /// is [`Error::InsecureSocket`], which says why and is never retried. The
    /// same test runs again at every [`Transport::connect`], so a directory
    /// that is widened later is caught too. The test and the connect are two
    /// steps; the directory being owner-only is what keeps another user from
    /// slipping in between them.
    ///
    /// A socket that does not exist yet (serve is still starting) is not an
    /// error here: [`Transport::connect`] reports it as a transient
    /// [`Error::Connect`], so a caller can wait and try again. The directories
    /// above the socket's own are not examined (as in serve).
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().as_bytes().len() > SOCKET_PATH_MAX {
            return Err(Error::InvalidConfig(format!(
                "the socket path is longer than {SOCKET_PATH_MAX} bytes, the most a socket \
                 address holds on this system"
            )));
        }
        if path.file_name().is_none() {
            return Err(Error::InvalidConfig(
                "the socket path needs a file name".into(),
            ));
        }
        match check(&path, euid()) {
            Ok(()) | Err(Error::Connect(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(Self {
            path,
            connect_timeout: Duration::from_secs(10),
        })
    }

    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The policy, for the caller whose effective uid is `euid`. A path that is
/// missing or unreadable is an [`Error::Connect`] (nothing was sent, trying
/// again later may work); a path that is there and unsafe is a refusal.
fn check(path: &Path, euid: u32) -> Result<()> {
    let refuse = |why: String| Error::InsecureSocket {
        path: path.display().to_string(),
        reason: why,
    };
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let dm = std::fs::symlink_metadata(dir).map_err(Error::Connect)?;
    if dm.file_type().is_symlink() {
        return Err(refuse(format!(
            "the directory {} is a symbolic link; it is not followed",
            dir.display()
        )));
    }
    if !dm.is_dir() {
        return Err(refuse(format!("{} is not a directory", dir.display())));
    }
    if dm.uid() != euid {
        return Err(refuse(format!(
            "the directory {} is owned by another user (uid {}, you are {euid}); \
             that user could replace the socket and read the token",
            dir.display(),
            dm.uid()
        )));
    }
    let mode = dm.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(refuse(format!(
            "the directory {} has mode {mode:04o}; it must be 0700 (owner only), or another \
             user could replace the socket and read the token: chmod 700 {}",
            dir.display(),
            dir.display()
        )));
    }
    let sm = std::fs::symlink_metadata(path).map_err(Error::Connect)?;
    if sm.file_type().is_symlink() {
        return Err(refuse(
            "the socket path is a symbolic link; it is not followed".into(),
        ));
    }
    if !sm.file_type().is_socket() {
        return Err(refuse("the path is not a socket".into()));
    }
    if sm.uid() != euid {
        return Err(refuse(format!(
            "the socket is owned by another user (uid {}, you are {euid})",
            sm.uid()
        )));
    }
    Ok(())
}

/// Run `f` on its own thread and wait at most `budget` for it. `UnixStream`
/// has no connect timeout of its own and `connect` blocks on Linux while the
/// listener's backlog is full; a thread that outlives the budget finishes the
/// connect on its own and drops the stream.
fn within<T: Send + 'static>(
    budget: Duration,
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("tole-client-connect".into())
        .spawn(move || {
            let _ = tx.send(f());
        })?;
    rx.recv_timeout(budget).unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the connect budget is spent",
        ))
    })
}

impl Transport for UnixTransport {
    /// Checks the path (see [`UnixTransport::new`]), then connects within a
    /// 10 s budget, as the TCP transport does. Failures to connect are
    /// [`Error::Connect`]: nothing was sent.
    fn connect(&self) -> Result<Box<dyn Connection>> {
        check(&self.path, euid())?;
        let path = self.path.clone();
        let stream = within(self.connect_timeout, move || UnixStream::connect(path))
            .map_err(Error::Connect)?;
        Ok(Box::new(stream))
    }

    /// The server does not look at it on a socket; any valid host will do.
    fn host_header(&self) -> String {
        "localhost".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    /// A fresh directory with the given mode, removed on drop.
    struct Dir(PathBuf);
    impl Dir {
        fn new(mode: u32) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("tcu-{}-{n}", std::process::id()));
            std::fs::create_dir(&p).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            Dir(p)
        }
        fn sock(&self) -> PathBuf {
            self.0.join("s.sock")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn reason(e: Error) -> String {
        match e {
            Error::InsecureSocket { reason, .. } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_private_directory_and_an_own_socket_pass() {
        let d = Dir::new(0o700);
        let _l = UnixListener::bind(d.sock()).unwrap();
        assert!(UnixTransport::new(d.sock()).is_ok());
        assert!(check(&d.sock(), euid()).is_ok());
    }

    #[test]
    fn a_directory_with_group_or_other_access_is_refused_and_says_why() {
        for mode in [0o770, 0o755, 0o707, 0o750, 0o701] {
            let d = Dir::new(mode);
            let _l = UnixListener::bind(d.sock()).unwrap();
            let why = reason(UnixTransport::new(d.sock()).unwrap_err());
            assert!(
                why.contains("must be 0700") && why.contains(&format!("{mode:04o}")),
                "{why}"
            );
            // Not only at construction: connect says it too.
            let t = UnixTransport {
                path: d.sock(),
                connect_timeout: Duration::from_secs(1),
            };
            let err = t.connect().err().unwrap();
            assert!(reason(err).contains("must be 0700"));
        }
    }

    #[test]
    fn a_directory_widened_after_construction_is_caught_at_connect() {
        let d = Dir::new(0o700);
        let _l = UnixListener::bind(d.sock()).unwrap();
        let t = UnixTransport::new(d.sock()).unwrap();
        assert!(t.connect().is_ok());
        std::fs::set_permissions(&d.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = t.connect().err().unwrap();
        assert!(!err.is_transient(), "a policy refusal is never retried");
        assert!(reason(err).contains("0755"));
    }

    #[test]
    fn a_directory_or_socket_of_another_user_is_refused() {
        // Another effective uid, through the policy function: no chown and no
        // second user needed, so it runs the same as root and as not root.
        let d = Dir::new(0o700);
        let _l = UnixListener::bind(d.sock()).unwrap();
        let other = euid().wrapping_add(1);
        let why = reason(check(&d.sock(), other).unwrap_err());
        assert!(
            why.contains("directory") && why.contains("owned by another user"),
            "{why}"
        );
        // The directory passes for its owner. A socket someone else owns in
        // such a directory (root only: chown needs it) does not.
        if euid() == 0 {
            let c = std::ffi::CString::new(d.sock().as_os_str().as_bytes()).unwrap();
            // SAFETY: `c` is a valid NUL-terminated path for the call.
            assert_eq!(unsafe { libc::chown(c.as_ptr(), 65534, 65534) }, 0);
            let why = reason(check(&d.sock(), 0).unwrap_err());
            assert!(why.contains("socket is owned by another user"), "{why}");
        } else {
            assert!(check(&d.sock(), euid()).is_ok());
        }
    }

    #[test]
    fn links_and_other_files_are_refused() {
        let d = Dir::new(0o700);
        let real = d.0.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _l = UnixListener::bind(real.join("s.sock")).unwrap();
        // The directory is a link to a good directory.
        let linked = d.0.join("link");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let why = reason(UnixTransport::new(linked.join("s.sock")).unwrap_err());
        assert!(why.contains("symbolic link"), "{why}");
        // The socket path is a link to a socket.
        let link_sock = d.0.join("l.sock");
        std::os::unix::fs::symlink(real.join("s.sock"), &link_sock).unwrap();
        let why = reason(UnixTransport::new(&link_sock).unwrap_err());
        assert!(why.contains("symbolic link"), "{why}");
        // A regular file, and a directory that is a file.
        let file = d.0.join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(reason(UnixTransport::new(&file).unwrap_err()).contains("not a socket"));
        let why = reason(UnixTransport::new(file.join("s.sock")).unwrap_err());
        assert!(why.contains("not a directory"), "{why}");
    }

    #[test]
    fn a_missing_socket_or_directory_is_a_transient_connect_error() {
        let d = Dir::new(0o700);
        // Constructing is fine (serve may still be starting); connecting is not yet.
        let t = UnixTransport::new(d.sock()).unwrap();
        let err = t.connect().err().expect("nothing listens");
        assert!(matches!(err, Error::Connect(_)), "{err:?}");
        assert!(err.is_transient());
        let t = UnixTransport::new(d.0.join("absent/s.sock")).unwrap();
        assert!(matches!(t.connect().err().unwrap(), Error::Connect(_)));
        // A socket file nobody listens on (a killed serve): refused connection.
        drop(UnixListener::bind(d.sock()).unwrap());
        let t = UnixTransport::new(d.sock()).unwrap();
        let err = t.connect().err().unwrap();
        assert!(
            matches!(err, Error::Connect(ref e) if e.kind() == io::ErrorKind::ConnectionRefused),
            "{err:?}"
        );
        assert!(err.is_transient());
    }

    #[test]
    fn unusable_paths_are_refused_up_front() {
        let long = std::env::temp_dir()
            .join("x".repeat(SOCKET_PATH_MAX))
            .join("s.sock");
        assert!(matches!(
            UnixTransport::new(long),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            UnixTransport::new("/"),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn the_connection_honours_read_timeouts_and_the_host_header_is_fixed() {
        let d = Dir::new(0o700);
        let l = UnixListener::bind(d.sock()).unwrap();
        let t = UnixTransport::new(d.sock()).unwrap();
        assert_eq!(t.host_header(), "localhost");
        assert_eq!(t.path(), d.sock());
        let mut c = t.connect().unwrap();
        let _peer = l.accept().unwrap();
        c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut b = [0u8; 1];
        let e = io::Read::read(&mut c, &mut b).unwrap_err();
        assert!(matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        // A zero timeout is invalid, as for TCP: the client reports it as a setting.
        let e = c.set_read_timeout(Some(Duration::ZERO)).unwrap_err();
        assert!(matches!(Error::from(e), Error::InvalidConfig(_)));
    }

    #[test]
    fn a_connect_that_outlives_its_budget_is_a_timeout() {
        let started = std::time::Instant::now();
        let err = within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_millis(1500));
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(1200));
        assert!(Error::Connect(err).is_transient());
        assert_eq!(within(Duration::from_secs(5), || Ok(7)).unwrap(), 7);
    }
}
