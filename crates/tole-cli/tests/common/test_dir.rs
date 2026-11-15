//! A scratch directory that removes itself (#781).
//!
//! Tests used to build `std::env::temp_dir().join(format!(..))` by hand and
//! never delete it, so every run of the suites left directories behind.
//! `TestDir` creates a unique directory and removes it on drop, also when
//! the test panics.
//!
//! The same file is compiled into the integration tests (`mod support`) and,
//! through a `#[path]` module under `#[cfg(test)]`, into the unit tests of
//! the crate. Keep the copies in `tole-core` and `tole-cli` identical.
//!
//! * Where: `CARGO_TARGET_TMPDIR` for integration tests (cargo owns it and
//!   `cargo clean` removes it), the system temp dir for unit tests.
//! * Debugging: `TOLE_KEEP_TEST_DIRS=1` keeps every directory and prints its
//!   path; [`TestDir::keep`] keeps one.
//! * A child that may still use the directory must be killed and awaited
//!   before the guard drops (see [`ChildGuard`]).
//!
//! The condition waits of #812 live in the same file ([`wait_until`],
//! [`wait_for`]): a test that waits for something to happen polls for the
//! condition up to a generous bound instead of sleeping a fixed time, so a
//! loaded machine only makes it slower, never red, and a passing run does not
//! wait longer than the condition needs.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory, removed (best effort) when dropped.
#[derive(Debug)]
pub struct TestDir {
    path: PathBuf,
    keep: bool,
}

impl TestDir {
    /// Creates `<base>/<prefix>-<pid>-<n>-<clock>` and returns its guard.
    /// The path is canonical (no symlinked temp root, as on macOS).
    pub fn new(prefix: &str) -> Self {
        let base = match option_env!("CARGO_TARGET_TMPDIR") {
            Some(d) => PathBuf::from(d),
            None => std::env::temp_dir(),
        };
        Self::create_in(&base, prefix)
    }

    /// Like [`TestDir::new`], but always under the system temp dir. For a
    /// test whose subject must not sit inside this repository's checkout
    /// (cargo's target dir does, in CI), such as a git probe.
    pub fn in_system_temp(prefix: &str) -> Self {
        Self::create_in(&std::env::temp_dir(), prefix)
    }

    fn create_in(base: &Path, prefix: &str) -> Self {
        std::fs::create_dir_all(base).expect("create test temp base");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        loop {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = base.join(format!("{prefix}-{}-{n}-{stamp:x}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    let path = path.canonicalize().expect("canonicalize test dir");
                    return TestDir { path, keep: false };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create test dir {}: {e}", path.display()),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keeps the directory after the test and returns its path.
    pub fn keep(mut self) -> PathBuf {
        self.keep = true;
        std::mem::take(&mut self.path)
    }
}

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for TestDir {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if self.keep || self.path.as_os_str().is_empty() {
            return;
        }
        if std::env::var("TOLE_KEEP_TEST_DIRS").is_ok_and(|v| v == "1") {
            eprintln!("TOLE_KEEP_TEST_DIRS: kept {}", self.path.display());
            return;
        }
        remove_tree(&self.path);
    }
}

/// The bound a condition wait uses unless a test has a reason for another:
/// long enough for a machine stuck on one busy CPU, short enough that a
/// condition that never holds still fails the test in a minute. A run that
/// passes never waits for it.
pub const WAIT: Duration = Duration::from_secs(60);

/// Polls `probe` until it returns `Some` and returns that value, or returns
/// `None` once `timeout` has passed. For a wait whose expiry is an answer
/// (a child that must be killed when it does not exit), not a failure; use
/// [`wait_for`] when expiry fails the test.
///
/// The probe runs at once, then after 2 ms, doubling up to 50 ms, so a
/// condition that is already true costs nothing and a slow one is not
/// hammered. It runs one last time at the deadline, so a long pause of this
/// thread (a loaded machine) cannot turn a condition that holds into a
/// timeout. A fixed `thread::sleep` before an assertion is a guess about the
/// machine; this waits for the fact (#812).
pub fn poll_until<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    let mut nap = Duration::from_millis(2);
    loop {
        if let Some(v) = probe() {
            return Some(v);
        }
        let spent = start.elapsed();
        if spent >= timeout {
            // The pause above may have been the machine's, not the
            // condition's: look once more before giving up.
            return probe();
        }
        std::thread::sleep(nap.min(timeout - spent));
        nap = (nap * 2).min(Duration::from_millis(50));
    }
}

/// [`poll_until`] that panics with `what` and the bound when the probe is
/// still `None` after `timeout`, so a failure says what never happened.
#[track_caller]
pub fn wait_for<T>(timeout: Duration, what: &str, probe: impl FnMut() -> Option<T>) -> T {
    match poll_until(timeout, probe) {
        Some(v) => v,
        None => panic!("timed out after {timeout:?} waiting for {what}"),
    }
}

/// Waits up to `timeout` for `child` to exit and returns its status, or
/// `None` when it is still running (the caller kills it and fails the test
/// with what it knows). The child is not reaped on `None`.
pub fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    poll_until(timeout, || child.try_wait().ok().flatten())
}

/// [`wait_for`] for a plain condition: returns once `cond` holds.
///
/// ```ignore
/// wait_until(WAIT, "the child to write its file", || file.exists());
/// ```
#[track_caller]
pub fn wait_until(timeout: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    wait_for(timeout, what, || cond().then_some(()));
}

/// Writes `contents` to `path` as an executable (mode 0o755) and returns only
/// when the script can be exec'd (#840).
///
/// The race it removes: the writing thread closes its fd, but a concurrent
/// `fork()` of another test thread (any `Command::spawn`) may already have
/// copied it. The child keeps that write fd until its own `exec` (the fd is
/// `O_CLOEXEC`), and for that moment an `exec` of the script fails with
/// `ETXTBSY` ("Text file busy"). Renaming or re-chmod-ing does not help (the
/// inode is the same), and a fixed sleep is a guess. So after writing and
/// closing, this waits for the fact: no process holds a writable fd to the
/// file any more. On Linux the kernel answers that exactly (a read lease is
/// refused while any open file description of the inode can write, including
/// the copy of a child that is still between `fork` and `exec`, which no
/// `/proc` scan can see yet); elsewhere the wait is a no-op, those kernels
/// are not known to hit this.
///
/// Use it for every script a test runs; do not combine `std::fs::write` with
/// `set_permissions(.., 0o755)` by hand.
#[cfg(unix)]
#[track_caller]
pub fn write_executable(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = path.as_ref();
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(path)
            .unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
        f.write_all(contents.as_ref())
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        f.flush().unwrap();
    } // closed here
      // `mode` is masked by the umask and ignored for an existing file.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
    settle_writes(path);
}

/// Copies the executable `src` to `dst` (keeping its mode) and returns when
/// `dst` can be exec'd: the same race as [`write_executable`], for a test
/// that runs a renamed copy of a binary (#840).
#[cfg(unix)]
#[track_caller]
pub fn copy_executable(src: impl AsRef<Path>, dst: impl AsRef<Path>) {
    let (src, dst) = (src.as_ref(), dst.as_ref());
    std::fs::copy(src, dst)
        .unwrap_or_else(|e| panic!("copy {} to {}: {e}", src.display(), dst.display()));
    settle_writes(dst);
}

/// Waits until no open file description of `path` can write (see
/// [`write_executable`]).
#[cfg(unix)]
#[track_caller]
fn settle_writes(path: &Path) {
    wait_until(
        WAIT,
        &format!("writers of {} to close", path.display()),
        || writers_gone(path),
    );
}

/// True once no open file description of `path` can write. Takes (and at
/// once drops) a read lease, which Linux refuses with `EAGAIN` while a writer
/// exists. Where leases are unavailable (`leases-enable=0`, a filesystem
/// without them) it falls back to scanning `/proc/*/fdinfo`, which is
/// right except for a child caught mid-`fork`.
#[cfg(target_os = "linux")]
fn writers_gone(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(f) = std::fs::File::open(path) else {
        return true;
    };
    let fd = f.as_raw_fd();
    // SAFETY: plain fcntl calls on an fd this function owns.
    unsafe {
        if libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) == 0 {
            libc::fcntl(fd, libc::F_SETLEASE, libc::F_UNLCK);
            return true;
        }
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EAGAIN) | Some(libc::EBUSY) => false,
        _ => !has_writer(path),
    }
}

/// True when some process still has `path` open for writing, as far as
/// `/proc` shows.
#[cfg(target_os = "linux")]
fn has_writer(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(want) = std::fs::metadata(path) else {
        return false;
    };
    let (dev, ino) = (want.dev(), want.ino());
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    for proc in procs.flatten() {
        if !proc
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else {
            continue; // gone, or not ours
        };
        for fd in fds.flatten() {
            // `metadata` follows the link: same device and inode, whatever
            // the path the other process opened it by.
            let Ok(m) = std::fs::metadata(fd.path()) else {
                continue;
            };
            if m.dev() != dev || m.ino() != ino {
                continue;
            }
            let info = proc.path().join("fdinfo").join(fd.file_name());
            let writable = std::fs::read_to_string(info)
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find_map(|l| l.strip_prefix("flags:"))
                        .and_then(|f| u32::from_str_radix(f.trim(), 8).ok())
                })
                // O_ACCMODE: 0 read-only, 1 write-only, 2 read-write.
                .is_none_or(|f| f & 0o3 != 0);
            if writable {
                return true;
            }
        }
    }
    false
}

#[cfg(all(unix, not(target_os = "linux")))]
fn writers_gone(_path: &Path) -> bool {
    true
}

/// Windows has no `ETXTBSY` and no exec bit: just copy the file.
#[cfg(not(unix))]
#[track_caller]
pub fn copy_executable(src: impl AsRef<Path>, dst: impl AsRef<Path>) {
    std::fs::copy(src.as_ref(), dst.as_ref()).expect("copy executable");
}

/// Windows has no `ETXTBSY` and no exec bit: just write the file.
#[cfg(not(unix))]
#[track_caller]
pub fn write_executable(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    std::fs::write(path.as_ref(), contents).expect("write script");
}

/// Removes `dir`, retrying after making everything writable (read-only
/// files and directories) and after a short wait (a child or a scanner that
/// still holds a file open, mostly on Windows). Never panics.
fn remove_tree(dir: &Path) {
    for attempt in 0..5u32 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            return;
        }
        make_writable(dir);
        std::thread::sleep(Duration::from_millis(40 * u64::from(attempt + 1)));
    }
    eprintln!("could not remove test dir {}", dir.display());
}

/// Gives the owner write (and, for directories, search) permission on the
/// whole tree. Symlinks are not followed.
fn make_writable(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut perms = meta.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() | if meta.is_dir() { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
    }
    let _ = std::fs::set_permissions(path, perms);
    if meta.is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                make_writable(&entry.path());
            }
        }
    }
}

/// A working directory `<root>/w` inside a [`TestDir`]. Its siblings
/// (`work.with_extension("home")`, a `CODECORA_HOME` outside the workspace)
/// are created inside the same root, so they go away with it. Derefs to the
/// working directory.
#[derive(Debug)]
pub struct WorkDir {
    _root: TestDir,
    work: PathBuf,
}

impl WorkDir {
    pub fn new(prefix: &str) -> Self {
        let root = TestDir::new(prefix);
        let work = root.path().join("w");
        std::fs::create_dir_all(&work).expect("create work dir");
        WorkDir { _root: root, work }
    }

    pub fn path(&self) -> &Path {
        &self.work
    }
}

impl std::ops::Deref for WorkDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.work
    }
}

impl AsRef<Path> for WorkDir {
    fn as_ref(&self) -> &Path {
        &self.work
    }
}

impl AsRef<OsStr> for WorkDir {
    fn as_ref(&self) -> &OsStr {
        self.work.as_os_str()
    }
}

/// Kills and reaps a spawned child when dropped. Declare it after the
/// [`TestDir`] the child works in: locals drop in reverse order, so the
/// child is gone before the directory is removed.
pub struct ChildGuard(pub Child);

impl ChildGuard {
    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
