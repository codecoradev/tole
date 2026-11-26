//! The listener and the connection of `tole serve` (#1043).
//!
//! A serve process listens on loopback TCP (the default) or, with
//! `--socket <path>`, on a Unix domain socket in a private directory. Both
//! hand the request loop the same [`Conn`], so the routes, the auth checks and
//! the event stream are one code path; only where the bytes come from differs
//! (the faces rule: a face may differ in identity, transport format and
//! approval delivery).
//!
//! The local socket is meant for a client on the same machine (the desktop).
//! Its boundary is the filesystem: the directory holding the socket is created
//! `0700` and serve refuses to use one that is wider, owned by someone else or
//! a link, so another local user cannot even connect, and a process that is
//! not the owner cannot bind the path first (the squatter of a loopback port).
//! The bearer token still gates every route: the socket narrows who can reach
//! serve, it does not replace the credential. The per-launch token travels in
//! an inherited file descriptor ([`read_token_fd`]) or a `0600` file
//! ([`check_private_token_file`]), never in the arguments or the environment.
//!
//! Windows: a named pipe is not implemented yet ([`bind_socket`] says so).
#![allow(unsafe_code)] // SAFETY: socket, flock and descriptor FFI of the serve transport; each block states its invariant.

use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

/// One accepted connection, TCP or local socket.
pub enum Conn {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Conn {
    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => s.set_read_timeout(d),
            #[cfg(unix)]
            Conn::Unix(s) => s.set_read_timeout(d),
        }
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => s.set_write_timeout(d),
            #[cfg(unix)]
            Conn::Unix(s) => s.set_write_timeout(d),
        }
    }

    /// True for a connection on the local socket.
    pub fn is_local_socket(&self) -> bool {
        match self {
            Conn::Tcp(_) => false,
            #[cfg(unix)]
            Conn::Unix(_) => true,
        }
    }

    /// The key of the failed-auth limiter and the "is this client local" test of
    /// an unsigned webhook. A socket peer has no address: it is a process of the
    /// owner (the directory is `0700`), so it counts as loopback and all of them
    /// share one limiter bucket.
    pub fn peer_ip(&self) -> Option<IpAddr> {
        match self {
            Conn::Tcp(s) => s.peer_addr().ok().map(|a| a.ip()),
            #[cfg(unix)]
            Conn::Unix(_) => Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Read for &Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => (&*s).read(buf),
            #[cfg(unix)]
            Conn::Unix(s) => (&*s).read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

impl Write for &Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => (&*s).write(buf),
            #[cfg(unix)]
            Conn::Unix(s) => (&*s).write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => (&*s).flush(),
            #[cfg(unix)]
            Conn::Unix(s) => (&*s).flush(),
        }
    }
}

/// Where serve accepts connections.
pub enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixSocket),
}

impl Listener {
    /// Wait up to `wait` for one connection: `Ok(None)` when none came (also when
    /// a signal interrupted the wait), so the accept loop can look at its flags
    /// between connections (#1014). The listener is non-blocking from the first
    /// call on, and the accepted connection is made blocking again (some systems
    /// let it inherit the flag).
    pub fn accept_within(&self, wait: Duration) -> io::Result<Option<Conn>> {
        self.set_nonblocking()?;
        if !self.readable_within(wait)? {
            return Ok(None);
        }
        let conn = match self {
            Listener::Tcp(l) => l.accept().map(|(s, _)| s).and_then(|s| {
                s.set_nonblocking(false)?;
                Ok(Conn::Tcp(s))
            }),
            #[cfg(unix)]
            Listener::Unix(u) => u.listener.accept().map(|(s, _)| s).and_then(|s| {
                s.set_nonblocking(false)?;
                Ok(Conn::Unix(s))
            }),
        };
        match conn {
            Ok(c) => Ok(Some(c)),
            // The peer left between the wake-up and the accept.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        match self {
            Listener::Tcp(l) => l.set_nonblocking(true),
            #[cfg(unix)]
            Listener::Unix(u) => u.listener.set_nonblocking(true),
        }
    }

    /// True when a connection is waiting to be accepted.
    #[cfg(unix)]
    fn readable_within(&self, wait: Duration) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        let fd = match self {
            Listener::Tcp(l) => l.as_raw_fd(),
            Listener::Unix(u) => u.listener.as_raw_fd(),
        };
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = wait.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid `pollfd` for a descriptor this listener owns.
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            };
        }
        Ok(r > 0)
    }

    /// No `poll` here: sleep in short steps until the deadline.
    #[cfg(not(unix))]
    fn readable_within(&self, wait: Duration) -> io::Result<bool> {
        std::thread::sleep(wait.min(Duration::from_millis(25)));
        Ok(true)
    }
}

/// A bound Unix socket. It holds the start-up lock (see [`bind_socket`]) for as
/// long as it lives. Dropping it removes the socket file, but only the one this
/// process bound (same device and inode): it never unlinks another serve's
/// socket. `run_serve` returns after a SIGTERM or SIGINT drain (#1014), which drops this
/// and so removes the file and releases the lock; a process that is killed leaves
/// the file behind, and the next start removes it (under the lock) once nothing
/// answers on it.
#[cfg(unix)]
pub struct UnixSocket {
    listener: std::os::unix::net::UnixListener,
    path: std::path::PathBuf,
    /// `(dev, ino)` of the socket file right after `bind`.
    identity: (u64, u64),
    /// The exclusive `flock` on `<socket>.lock`, released when the process ends.
    _lock: std::fs::File,
}

#[cfg(unix)]
impl Drop for UnixSocket {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let ours = std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.file_type().is_socket() && (m.dev(), m.ino()) == self.identity);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Take the start-up lock of `socket`: an exclusive, non-blocking `flock` on
/// `<socket>.lock` in the private directory, held for the whole life of the
/// serve process (the kernel drops it when the process ends, however it ends).
/// It serialises two starts of the owner's own processes, so the stale-socket
/// check and removal cannot delete a socket another start has just bound. The
/// lock file is never removed (removing it would reopen the race).
#[cfg(unix)]
fn lock_socket_dir(socket: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut name = socket.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    let lock_path = socket.with_file_name(name);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|e| {
            format!(
                "--socket {}: cannot open the lock file {}: {e}",
                socket.display(),
                lock_path.display()
            )
        })?;
    let meta = f
        .metadata()
        .map_err(|e| format!("--socket {}: {e}", socket.display()))?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "--socket {}: the lock file {} is not a regular file of this user",
            socket.display(),
            lock_path.display()
        ));
    }
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            let who = std::os::unix::net::UnixStream::connect(socket)
                .ok()
                .and_then(probe_stream)
                .and_then(|h| h["version"].as_str().map(|v| format!(" (tole {v})")))
                .unwrap_or_default();
            return Err(format!(
                "--socket {}: another tole serve already runs on it or is starting{who}; \
                 attach to it instead of starting another",
                socket.display()
            ));
        }
        return Err(format!(
            "--socket {}: cannot lock {}: {e}",
            socket.display(),
            lock_path.display()
        ));
    }
    Ok(f)
}

/// Longest socket path `bind` accepts (`sun_path`, one byte kept for the NUL).
#[cfg(all(unix, any(target_os = "macos", target_os = "ios")))]
const SOCKET_PATH_MAX: usize = 103;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
const SOCKET_PATH_MAX: usize = 107;

/// Listen on the Unix socket `socket` (`--socket`). Everything that makes the
/// local boundary weaker than it looks is a refusal here, before anything
/// listens:
///
/// - the parent directory is created `0700` when missing; an existing one must
///   be a real directory (not a link), owned by the current user and with no
///   group or other permission;
/// - the socket path must not be a link or any other file; an existing socket is
///   removed only when nothing answers on it (a live serve is a refusal that
///   names its version when it answers `/health`).
///
/// A second start on the same path fails on the start-up lock (`flock` on
/// `<socket>.lock`, held until the process ends), so two starts cannot remove
/// each other's socket. The socket file itself is set to `0600`.
#[cfg(unix)]
pub fn bind_socket(socket: &Path) -> Result<Listener, String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    if socket.as_os_str().as_bytes().len() > SOCKET_PATH_MAX {
        return Err(format!(
            "--socket {}: the path is longer than {SOCKET_PATH_MAX} bytes, the most a socket \
             address holds on this system; use a shorter directory",
            socket.display()
        ));
    }
    if socket.file_name().is_none() {
        return Err(format!(
            "--socket {}: not a file path (a socket name is needed)",
            socket.display()
        ));
    }
    let dir = match socket.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    check_private_dir(dir).map_err(|e| format!("--socket {}: {e}", socket.display()))?;
    let lock = lock_socket_dir(socket)?;
    let me = unsafe { libc::geteuid() };

    match std::fs::symlink_metadata(socket) {
        Ok(m) => {
            let ft = m.file_type();
            if ft.is_symlink() {
                return Err(format!(
                    "--socket {}: the path is a symbolic link; refusing to follow it",
                    socket.display()
                ));
            }
            if !ft.is_socket() {
                return Err(format!(
                    "--socket {}: the path exists and is not a socket; not removing it",
                    socket.display()
                ));
            }
            if m.uid() != me {
                return Err(format!(
                    "--socket {}: the socket is owned by another user",
                    socket.display()
                ));
            }
            match UnixStream::connect(socket) {
                Ok(live) => {
                    let who = probe_stream(live)
                        .and_then(|h| h["version"].as_str().map(|v| format!(" (tole {v})")))
                        .unwrap_or_default();
                    return Err(format!(
                        "--socket {}: something already answers on it{who}; attach to it \
                         instead of starting another serve",
                        socket.display()
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    // A socket file nobody listens on: left by a killed serve.
                    std::fs::remove_file(socket).map_err(|e| {
                        format!(
                            "--socket {}: cannot remove the stale socket: {e}",
                            socket.display()
                        )
                    })?;
                }
                Err(e) => {
                    return Err(format!(
                        "--socket {}: cannot tell whether it is in use ({e}); not removing it",
                        socket.display()
                    ));
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("--socket {}: {e}", socket.display())),
    }

    let listener = UnixListener::bind(socket)
        .map_err(|e| format!("--socket {}: cannot bind: {e}", socket.display()))?;
    // The directory already keeps everyone else out; owner-only on the file too.
    if let Err(e) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(socket);
        return Err(format!(
            "--socket {}: cannot restrict the socket to its owner: {e}",
            socket.display()
        ));
    }
    let identity = std::fs::symlink_metadata(socket)
        .map(|m| (m.dev(), m.ino()))
        .map_err(|e| format!("--socket {}: {e}", socket.display()))?;
    Ok(Listener::Unix(UnixSocket {
        listener,
        path: socket.to_path_buf(),
        identity,
        _lock: lock,
    }))
}

/// Windows: a named pipe is the planned transport and is not built yet.
#[cfg(not(unix))]
pub fn bind_socket(socket: &Path) -> Result<Listener, String> {
    Err(format!(
        "--socket {}: local sockets are only available on Unix (macOS, Linux); a Windows named \
         pipe is not implemented yet (#1043 follow-up). Use the loopback TCP listener",
        socket.display()
    ))
}

/// Create `dir` `0700` when it does not exist, then require what a private
/// directory is: not a link, a directory, owned by the current user, with no
/// permission for group or other.
#[cfg(unix)]
fn check_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("cannot create the directory {}: {e}", dir.display()))?;
            std::fs::symlink_metadata(dir)
                .map_err(|e| format!("cannot read the directory {}: {e}", dir.display()))?
        }
        Err(e) => return Err(format!("cannot read the directory {}: {e}", dir.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(format!(
            "the directory {} is a symbolic link; refusing to follow it",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "the directory {} is owned by another user",
            dir.display()
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "the directory {} has mode {mode:04o}; it must be 0700 (owner only): chmod 700 {}",
            dir.display(),
            dir.display()
        ));
    }
    Ok(())
}

/// A token file used with `--socket` must be the owner's alone: a regular file
/// owned by the current user with no group or other permission (`0600`). The
/// plain `--token-file` rules (no link, no FIFO, 4 KiB) still apply on top.
#[cfg(unix)]
pub fn check_private_token_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        // The reader reports a missing file in its own words.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("--token-file {}: {e}", path.display())),
    };
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "--token-file {}: owned by another user",
            path.display()
        ));
    }
    let mode = meta.mode() & 0o7777;
    if meta.file_type().is_file() && mode & 0o077 != 0 {
        return Err(format!(
            "--token-file {}: has mode {mode:04o}; with --socket it must be 0600: chmod 600 {}",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn check_private_token_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// How long `--token-fd` waits for the writer to finish and close its end.
#[cfg(unix)]
const TOKEN_FD_WAIT: Duration = Duration::from_secs(5);

/// The bearer token from an inherited file descriptor (`--token-fd`): read to
/// the end (at most `cap` bytes), then closed. The parent writes the token into
/// a pipe and passes the read end, so the token is in neither the arguments nor
/// the environment of this process. A parent that leaves the write end open
/// gets an error after a few seconds, not a hang. Errors never carry the token.
#[cfg(unix)]
pub fn read_token_fd(fd: i32, cap: u64) -> Result<String, String> {
    read_token_fd_within(fd, cap, TOKEN_FD_WAIT)
}

#[cfg(unix)]
fn read_token_fd_within(fd: i32, cap: u64, wait: Duration) -> Result<String, String> {
    use std::os::fd::FromRawFd;
    if fd < 0 || fd == 1 || fd == 2 {
        return Err(format!("--token-fd {fd}: not a usable descriptor"));
    }
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return Err(format!("--token-fd {fd}: not an open file descriptor"));
    }
    if unsafe { libc::isatty(fd) } == 1 {
        return Err(format!(
            "--token-fd {fd}: is a terminal; pass a pipe or file with the token"
        ));
    }
    // Ownership of the descriptor moves here: it is closed when `f` drops.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let deadline = std::time::Instant::now() + wait;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!(
                "--token-fd {fd}: cannot be read: {}",
                io::Error::last_os_error()
            ));
        }
        if r == 0 {
            return Err(format!(
                "--token-fd {fd}: no end of input within {} s (does the launcher close its \
                 write end after writing the token?)",
                wait.as_secs().max(1)
            ));
        }
        // Readable (data, or end of input): this read does not block.
        let n = f
            .read(&mut chunk)
            .map_err(|e| format!("--token-fd {fd}: cannot be read: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() as u64 > cap {
            return Err(format!("--token-fd {fd}: larger than {} KiB", cap / 1024));
        }
    }
    let text =
        String::from_utf8(buf).map_err(|_| format!("--token-fd {fd}: the token is not UTF-8"))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(format!("--token-fd {fd}: is empty"));
    }
    Ok(token.to_string())
}

#[cfg(not(unix))]
pub fn read_token_fd(fd: i32, _cap: u64) -> Result<String, String> {
    Err(format!(
        "--token-fd {fd}: inherited descriptors are only supported on Unix; use --token-file"
    ))
}

/// `GET /health` on an open socket stream: the parsed answer of a serve, or
/// `None` when the peer does not answer like one. Used to name the version of
/// the serve that already runs on a socket.
#[cfg(unix)]
fn probe_stream(mut s: std::os::unix::net::UnixStream) -> Option<serde_json::Value> {
    let limit = Some(Duration::from_secs(2));
    s.set_read_timeout(limit).ok()?;
    s.set_write_timeout(limit).ok()?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: tole\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut out = Vec::new();
    s.take(64 * 1024).read_to_end(&mut out).ok()?;
    let text = String::from_utf8(out).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200").then_some(())?;
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    (v["status"] == "ok").then_some(v)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-sock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    fn err(r: Result<Listener, String>) -> String {
        match r {
            Ok(_) => panic!("expected a refusal"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_missing_directory_is_created_owner_only_and_the_socket_is_0600() {
        let root = scratch("create");
        let sock = root.join("run/serve.sock");
        let l = bind_socket(&sock).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(sock.parent().unwrap()), 0o700);
        assert_eq!(mode(&sock), 0o600);
        drop(l);
        assert!(
            !sock.exists(),
            "the socket file goes away with the listener"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `O_NONBLOCK` of a connection, read back from the descriptor.
    fn is_nonblocking(c: &Conn) -> bool {
        use std::os::fd::AsRawFd;
        let fd = match c {
            Conn::Tcp(s) => s.as_raw_fd(),
            Conn::Unix(s) => s.as_raw_fd(),
        };
        (unsafe { libc::fcntl(fd, libc::F_GETFL) }) & libc::O_NONBLOCK != 0
    }

    /// The wait of the accept loop (#1014): nothing waiting is `None` after the
    /// wait, a waiting connection is returned at once, and the connection is a
    /// plain blocking one.
    fn exercise_accept_within(l: &Listener, connect: impl Fn()) {
        let wait = Duration::from_millis(60);
        let started = std::time::Instant::now();
        assert!(l.accept_within(wait).unwrap().is_none());
        assert!(started.elapsed() >= wait, "it returned before the wait");
        connect();
        let conn = crate::test_dir::wait_for(Duration::from_secs(30), "the connection", || {
            l.accept_within(wait).unwrap()
        });
        assert!(!is_nonblocking(&conn), "an accepted connection blocks");
    }

    #[test]
    fn accept_within_waits_then_returns_a_blocking_connection_on_tcp() {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = tcp.local_addr().unwrap();
        let l = Listener::Tcp(tcp);
        let held = std::cell::RefCell::new(Vec::new());
        exercise_accept_within(&l, || {
            held.borrow_mut().push(TcpStream::connect(addr).unwrap())
        });
    }

    #[test]
    fn accept_within_waits_then_returns_a_blocking_connection_on_a_socket() {
        let root = scratch("accept");
        let sock = root.join("s.sock");
        let l = bind_socket(&sock).unwrap();
        let held = std::cell::RefCell::new(Vec::new());
        exercise_accept_within(&l, || {
            held.borrow_mut()
                .push(std::os::unix::net::UnixStream::connect(&sock).unwrap())
        });
        drop(l);
        assert!(!sock.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_with_group_or_other_access_is_refused() {
        let root = scratch("wide");
        for mode in [0o770, 0o750, 0o701, 0o777] {
            let dir = root.join(format!("d{mode:o}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = err(bind_socket(&dir.join("s.sock")));
            assert!(e.contains("must be 0700"), "{mode:o}: {e}");
            assert!(!dir.join("s.sock").exists());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_directory_or_socket_path_is_refused() {
        let root = scratch("link");
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let linked = root.join("linked");
        symlink(&real, &linked).unwrap();
        let e = err(bind_socket(&linked.join("s.sock")));
        assert!(e.contains("symbolic link"), "{e}");

        // The socket path itself a link, to a file and to a live socket.
        let target = root.join("elsewhere");
        std::fs::write(&target, "keep").unwrap();
        symlink(&target, real.join("s.sock")).unwrap();
        let e = err(bind_socket(&real.join("s.sock")));
        assert!(e.contains("symbolic link"), "{e}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_regular_file_at_the_socket_path_is_never_removed() {
        let root = scratch("file");
        let sock = root.join("s.sock");
        std::fs::write(&sock, "not a socket").unwrap();
        let e = err(bind_socket(&sock));
        assert!(e.contains("not a socket"), "{e}");
        assert_eq!(std::fs::read_to_string(&sock).unwrap(), "not a socket");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_of_another_user_is_refused() {
        let me = unsafe { libc::geteuid() };
        let dir = if me == 0 {
            // Root can give a 0700 directory to someone else.
            let root = scratch("owner");
            let c = std::ffi::CString::new(root.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::chown(c.as_ptr(), 65534, 65534) }, 0);
            root
        } else {
            // `/` is root's.
            PathBuf::from("/")
        };
        let e = err(bind_socket(&dir.join("tole-test.sock")));
        assert!(e.contains("owned by another user"), "{e}");
        if me == 0 {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_not() {
        let root = scratch("stale");
        let sock = root.join("s.sock");
        // A socket file nobody listens on: bind, then forget the listener
        // without unlinking (what a killed process leaves).
        let dead = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(dead);
        assert!(sock.exists());
        let live = bind_socket(&sock).unwrap();
        // The same path again while it is live: refused, and still there.
        let e = err(bind_socket(&sock));
        assert!(e.contains("already runs on it"), "{e}");
        assert!(sock.exists());
        drop(live);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dropping_a_listener_never_unlinks_a_socket_it_did_not_bind() {
        let root = scratch("ino");
        let sock = root.join("s.sock");
        let first = bind_socket(&sock).unwrap();
        // Someone replaces the file at the path with another live socket.
        std::fs::remove_file(&sock).unwrap();
        let other = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(first);
        assert!(sock.exists(), "a socket this listener did not bind stays");
        drop(other);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_starts_leave_exactly_one_serving_socket() {
        let root = scratch("race");
        let sock = root.join("s.sock");
        // A stale socket, then many starts at once: one wins, the rest are
        // refused on the lock, and the winner's socket is never unlinked.
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        let results: Vec<Result<Listener, String>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8).map(|_| s.spawn(|| bind_socket(&sock))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let refusals: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        assert_eq!(results.len() - refusals.len(), 1, "{refusals:?}");
        for e in refusals {
            assert!(e.contains("already runs on it"), "{e}");
        }
        assert!(
            std::os::unix::net::UnixStream::connect(&sock).is_ok(),
            "the winner's socket is reachable"
        );
        drop(results);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_path_too_long_for_a_socket_address_is_refused_by_name() {
        let long = Path::new("/tmp").join("x".repeat(200)).join("s.sock");
        let e = err(bind_socket(&long));
        assert!(e.contains("longer than"), "{e}");
    }

    #[test]
    fn the_token_file_must_be_the_owners_alone() {
        let root = scratch("tokfile");
        let f = root.join("token");
        std::fs::write(&f, "t\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        let e = check_private_token_file(&f).unwrap_err();
        assert!(e.contains("must be 0600") && !e.contains("t\n"), "{e}");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_private_token_file(&f).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn pipe_with(bytes: &[u8]) -> i32 {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let n = unsafe { libc::write(fds[1], bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n as usize, bytes.len());
        unsafe { libc::close(fds[1]) };
        fds[0]
    }

    #[test]
    fn the_token_is_read_from_a_descriptor_and_the_descriptor_is_closed() {
        let fd = pipe_with(b"  per-launch-token\n");
        assert_eq!(read_token_fd(fd, 4096).unwrap(), "per-launch-token");
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) },
            -1,
            "the descriptor is closed after the read"
        );
    }

    #[test]
    fn a_writer_that_never_closes_gets_an_error_not_a_hang() {
        // Data written, but the write end stays open: no end of input ever comes.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let secret = b"never-echoed-token";
        let n = unsafe { libc::write(fds[1], secret.as_ptr().cast(), secret.len()) };
        assert_eq!(n as usize, secret.len());
        let started = std::time::Instant::now();
        let e = read_token_fd_within(fds[0], 4096, Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(e.contains("no end of input"), "{e}");
        assert!(!e.contains("never-echoed-token"), "{e}");
        unsafe { libc::close(fds[1]) };
    }

    #[test]
    fn an_unusable_descriptor_is_an_error_that_never_carries_the_token() {
        let blank = pipe_with(b" \n");
        assert!(read_token_fd(blank, 4096).unwrap_err().contains("is empty"));
        let big = pipe_with(&[b'a'; 5000]);
        assert!(read_token_fd(big, 4096).unwrap_err().contains("larger"));
        assert!(read_token_fd(-1, 4096).is_err());
        assert!(read_token_fd(2, 4096).is_err());
        assert!(read_token_fd(987_654, 4096)
            .unwrap_err()
            .contains("not an open"));
    }
}
