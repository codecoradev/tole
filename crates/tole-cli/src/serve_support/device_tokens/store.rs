//! The token file: the only code that opens it.
//!
//! `serve-tokens.jsonl` is append-only and `serve-tokens.lock` is an advisory
//! lock (the OS drops it with the process, so none goes stale). `add` and
//! `revoke` take it blocking, read the file and append one whole line under
//! it, so two writers cannot both add one name. Nothing here rewrites, renames
//! or truncates the file (append-only by convention, not enforced by the file
//! system). The last component of the file and of the lock is opened with
//! `O_NOFOLLOW` (unix); parent directories are followed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use super::{
    generate, validate_name, Line, Principal, Scope, Secret, Snapshot, StoreError, MAX_ACTIVE,
    MAX_FILE_BYTES, MAX_USED_FILE_BYTES, RETRY_AFTER_SECS, TOUCH_EVERY_SECS,
};

const FILE: &str = "serve-tokens.jsonl";
const LOCK: &str = "serve-tokens.lock";

/// A token that was just made. The secret leaves only through
/// [`Secret::expose`].
#[derive(Debug)]
pub struct Added {
    pub name: String,
    pub scope: Scope,
    pub created_at: u64,
    pub secret: Secret,
}

/// The token file of one data dir.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    file: PathBuf,
    lock: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Store {
        Store {
            dir: dir.to_path_buf(),
            file: dir.join(FILE),
            lock: dir.join(LOCK),
        }
    }

    /// The store of the tole data dir (`$CODECORA_HOME/tole`).
    pub fn in_data_dir() -> Result<Store, StoreError> {
        let dir = tole_core::paths::tole_data_dir().ok_or(StoreError::NoDataDir)?;
        Ok(Store::new(&dir))
    }

    /// The directory must not be writable by anyone else: they could swap the
    /// file. A missing one is created 0700 when `create`.
    fn check_dir(&self, create: bool) -> Result<(), StoreError> {
        match std::fs::metadata(&self.dir) {
            Ok(m) if !m.is_dir() => {
                Err(StoreError::Unsafe("the data dir is not a directory".into()))
            }
            Ok(m) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if m.permissions().mode() & 0o022 != 0 {
                        return Err(StoreError::Unsafe(
                            "the data dir is writable by group or others (chmod 700)".into(),
                        ));
                    }
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                let mut b = std::fs::DirBuilder::new();
                b.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    b.mode(0o700);
                }
                Ok(b.create(&self.dir)?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Regular file, owner-only: anything else is refused, not repaired.
    fn check_file(f: &File) -> Result<(), StoreError> {
        let m = f.metadata()?;
        if !m.is_file() {
            return Err(StoreError::Unsafe("not a regular file".into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if m.permissions().mode() & 0o077 != 0 {
                return Err(StoreError::Unsafe(
                    "readable or writable by others (chmod 600)".into(),
                ));
            }
        }
        Ok(())
    }

    fn options() -> OpenOptions {
        let mut o = OpenOptions::new();
        o.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // NOFOLLOW: a planted link must not redirect us. NONBLOCK: a
            // planted FIFO must not hang us (it fails the regular-file check).
            o.mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        o
    }

    /// The lock file; held only for the flock, nothing is written to it.
    fn open_lock(&self, create: bool) -> std::io::Result<File> {
        Self::options().append(true).create(create).open(&self.lock)
    }

    /// The token file for reading and appending (never truncating).
    fn open_file(&self, create: bool) -> std::io::Result<File> {
        Self::options().append(true).create(create).open(&self.file)
    }

    /// The file's bytes; a missing file is empty. Capped, never followed.
    fn read(&self) -> Result<Vec<u8>, StoreError> {
        // Read-only: a 0400 file is still readable.
        let f = match Self::options().open(&self.file) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        Self::check_file(&f)?;
        let mut v = Vec::new();
        f.take(MAX_FILE_BYTES + 1).read_to_end(&mut v)?;
        if v.len() as u64 > MAX_FILE_BYTES {
            return Err(StoreError::TooLarge);
        }
        Ok(v)
    }

    /// The fold of the file. Reading needs no lock: the file only grows.
    pub fn load(&self) -> Result<Snapshot, StoreError> {
        if self.dir.exists() {
            self.check_dir(false)?;
        }
        Snapshot::parse(&self.read()?)
    }

    /// Takes the lock, reads and folds, and lets `f` decide what to append.
    fn locked<T>(
        &self,
        f: impl FnOnce(&Snapshot) -> Result<(T, Line), StoreError>,
    ) -> Result<T, StoreError> {
        self.check_dir(true)?;
        let lock = self.open_lock(true)?;
        Self::check_file(&lock)?;
        lock.lock()?;
        let bytes = self.read()?;
        let snap = Snapshot::parse(&bytes)?;
        let (out, line) = f(&snap)?;
        let mut buf = Vec::new();
        // A file that does not end in a newline (a hand edit) must not get the
        // new line glued on.
        if bytes.last().is_some_and(|b| *b != b'\n') {
            buf.push(b'\n');
        }
        buf.extend(line.encode()?);
        let mut file = self.open_file(true)?;
        Self::check_file(&file)?;
        file.write_all(&buf)?;
        file.sync_data()?;
        drop(lock);
        Ok(out)
    }

    /// Makes a token for `name`; the secret is in the result and nowhere else.
    pub fn add(&self, name: &str, scope: Scope, now: u64) -> Result<Added, StoreError> {
        validate_name(name).map_err(StoreError::BadName)?;
        self.locked(|snap| {
            if snap.active(name).is_some() {
                return Err(StoreError::Duplicate(name.to_string()));
            }
            if snap.active_count() >= MAX_ACTIVE {
                return Err(StoreError::TooMany);
            }
            let (secret, hash) = generate()?;
            let line = Line::add(name, scope, &hash, now);
            Ok((
                Added {
                    name: name.to_string(),
                    scope,
                    created_at: now,
                    secret,
                },
                line,
            ))
        })
    }

    /// Appends a revoke for the active token `name`.
    pub fn revoke(&self, name: &str, now: u64) -> Result<(), StoreError> {
        validate_name(name).map_err(StoreError::BadName)?;
        self.locked(|snap| {
            snap.active(name)
                .ok_or_else(|| StoreError::NotFound(name.to_string()))?;
            Ok(((), Line::revoke(name, now)))
        })
    }

    /// Best effort: appends a `used` line unless another process holds the
    /// lock, the file is missing or it is past [`MAX_USED_FILE_BYTES`].
    /// `Ok(true)` when written. Never blocks.
    pub fn record_use(&self, name: &str, now: u64) -> Result<bool, StoreError> {
        let Ok(lock) = self.open_lock(false) else {
            return Ok(false);
        };
        Self::check_file(&lock)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let mut file = match self.open_file(false) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        Self::check_file(&file)?;
        if file.metadata()?.len() >= MAX_USED_FILE_BYTES {
            return Ok(false);
        }
        let mut buf = Vec::new();
        // The same rule as `locked`: never glue a line onto a fragment.
        if last_byte(&mut file)?.is_some_and(|b| b != b'\n') {
            buf.push(b'\n');
        }
        buf.extend(Line::used(name, now).encode()?);
        file.write_all(&buf)?;
        Ok(true)
    }
}

fn last_byte(f: &mut File) -> std::io::Result<Option<u8>> {
    use std::io::{Seek, SeekFrom};
    if f.metadata()?.len() == 0 {
        return Ok(None);
    }
    // An O_APPEND write still goes to the end, wherever this read left us.
    f.seek(SeekFrom::End(-1))?;
    let mut b = [0u8; 1];
    f.read_exact(&mut b)?;
    Ok(Some(b[0]))
}

/// What `Live` compares to see that the file changed: length, mtime, ctime,
/// inode and device. An append changes the length, a `chmod` changes the
/// ctime and a file put in place by a rename changes the inode, so a file
/// made unsafe, made safe again or replaced is looked at again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct FileStamp {
    pub(super) len: u64,
    pub(super) mtime: Option<SystemTime>,
    pub(super) ctime: i64,
    pub(super) ino: u64,
    pub(super) dev: u64,
}

type Stamp = Option<FileStamp>;

pub(super) type Loader = Box<dyn Fn() -> Result<Snapshot, StoreError> + Send + Sync>;

struct Cached {
    /// `None` until the first load.
    stamp: Option<Stamp>,
    /// Empty when the file is missing or unusable (fail closed).
    snap: Arc<Snapshot>,
    error: Option<String>,
    /// Set when the last load failed with an error that may pass (I/O): the
    /// load is tried again at this time even if the file did not change.
    retry_at: Option<Instant>,
}

/// The view `tole serve` holds: a cached fold that is re-read when
/// the file changes, so a revoke applies on the next request, and a coalesced
/// last-use writer. A file that cannot be used means no device token is
/// valid. A failure that belongs to the file (corrupt, unsafe, too large) is
/// cached with the stamp, so unauthenticated requests do not re-read it; an
/// I/O error is retried after [`RETRY_AFTER_SECS`] or when the file changes.
/// See the module doc for the cost model (mutex over file I/O, synchronous
/// `touch`).
pub struct Live {
    store: Store,
    loader: Loader,
    retry_after: Duration,
    cached: Mutex<Cached>,
    touched: Mutex<std::collections::HashMap<String, u64>>,
}

impl Live {
    pub fn new(store: Store) -> Live {
        let s = store.clone();
        Self::with_loader(
            store,
            Box::new(move || s.load()),
            Duration::from_secs(RETRY_AFTER_SECS),
        )
    }

    /// A view whose loads are made by `loader` (tests inject a failure).
    pub(super) fn with_loader(store: Store, loader: Loader, retry_after: Duration) -> Live {
        Live {
            store,
            loader,
            retry_after,
            cached: Mutex::new(Cached {
                stamp: None,
                snap: Arc::new(Snapshot::default()),
                error: None,
                retry_at: None,
            }),
            touched: Mutex::new(Default::default()),
        }
    }

    pub(super) fn stamp(&self) -> Stamp {
        match std::fs::symlink_metadata(&self.store.file) {
            Ok(m) => {
                #[allow(unused_mut)]
                let mut st = FileStamp {
                    len: m.len(),
                    mtime: m.modified().ok(),
                    ..FileStamp::default()
                };
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    st.ctime = m
                        .ctime()
                        .saturating_mul(1_000_000_000)
                        .saturating_add(m.ctime_nsec());
                    st.ino = m.ino();
                    st.dev = m.dev();
                }
                Some(st)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            // Unreadable now: a stamp no file has, so it is looked at again
            // when the state changes.
            Err(_) => Some(FileStamp {
                len: u64::MAX - 1,
                ..FileStamp::default()
            }),
        }
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        let mut c = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        // Stat before reading: a write between the two is seen next time.
        let stamp = self.stamp();
        let retry = c.retry_at.is_some_and(|t| Instant::now() >= t);
        if c.stamp != Some(stamp) || retry {
            c.retry_at = None;
            match (self.loader)() {
                Ok(s) => (c.snap, c.error) = (Arc::new(s), None),
                Err(e) => {
                    if !e.is_deterministic() {
                        c.retry_at = Some(Instant::now() + self.retry_after);
                    }
                    (c.snap, c.error) = (Arc::new(Snapshot::default()), Some(e.to_string()));
                }
            }
            c.stamp = Some(stamp);
        }
        c.snap.clone()
    }
    /// Has the file been looked at yet? (Tests: a token of the wrong shape
    /// must not cause a read.)
    #[cfg(test)]
    pub(super) fn loaded(&self) -> bool {
        self.cached
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stamp
            .is_some()
    }

    /// Whose token is `presented` (the token alone, as taken from the header
    /// and trimmed by the caller), if it is an active device token. A token
    /// of the wrong shape (a static bearer token, say) costs no file access.
    pub fn lookup(&self, presented: &str) -> Option<Principal> {
        if !super::shaped(presented) {
            return None;
        }
        self.snapshot().lookup(presented)
    }

    /// Why the store is unusable now (for one log line), `None` when it is
    /// fine or missing.
    pub fn error(&self) -> Option<String> {
        self.snapshot();
        self.cached
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
            .clone()
    }

    /// Records that `name` was used at `now`, at most once per
    /// [`TOUCH_EVERY_SECS`] per name and never blocking; a failure is dropped
    /// (last use is information, not authority). Returns whether it wrote.
    pub fn touch(&self, name: &str, now: u64) -> bool {
        {
            let mut t = self.touched.lock().unwrap_or_else(|p| p.into_inner());
            if t.get(name)
                .is_some_and(|last| now.saturating_sub(*last) < TOUCH_EVERY_SECS)
            {
                return false;
            }
            // Marked before the attempt: a failing disk is retried once per
            // interval, not once per request.
            if t.len() >= 256 {
                t.clear();
            }
            t.insert(name.to_string(), now);
        }
        self.store.record_use(name, now).unwrap_or(false)
    }
}
