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

#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
