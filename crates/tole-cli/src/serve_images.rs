//! Image input for `tole serve` (#558): uploads, prompt references and the
//! recorded-image read route.
//!
//! - `POST /sessions/{id}/images` stores a raw body as an image of that
//!   session. The type is sniffed from the bytes (the request's
//!   `Content-Type` is never trusted) and the file is resolved through the
//!   same core path the CLI `--image` uses (`tole_core::image::resolve`:
//!   jail, size cap, sniff, FNV-1a 64). Files are content-addressed under
//!   `<spill dir>/serve/<session id>/<fnv1a64>.<ext>`, so the provider (whose
//!   image roots already hold the spill dir) re-loads them like any inline
//!   `--image`, also after a restart.
//! - `POST /sessions/{id}/prompt {"images": [<id>]}` resolves each id back to
//!   a reference of THAT session; the turn records only the reference.
//! - `GET /sessions/{id}/images/{fnv1a64}` serves an image only when that
//!   session's log records it, re-checking the jail and the recorded size
//!   and hash. A mismatch is an error, never a silent serve.
//!
//! Quotas bound the upload directory per session and in total (env
//! [`SESSION_QUOTA_ENV`], [`TOTAL_QUOTA_ENV`], in bytes). Unused uploads are
//! not garbage-collected (known limitation, see `docs/mobile-control.md`).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tole_core::image::{ImageRef, MAX_IMAGE_BYTES};

/// Per-session upload quota override (bytes).
pub const SESSION_QUOTA_ENV: &str = "TOLE_SERVE_IMAGE_SESSION_QUOTA";
/// Total upload quota override across all serve sessions (bytes).
pub const TOTAL_QUOTA_ENV: &str = "TOLE_SERVE_IMAGE_TOTAL_QUOTA";
/// Default per-session quota: 64 MiB (six full-size images).
pub const DEFAULT_SESSION_QUOTA: u64 = 64 * 1024 * 1024;
/// Default total quota: 512 MiB.
pub const DEFAULT_TOTAL_QUOTA: u64 = 512 * 1024 * 1024;

/// The extensions the core sniffer can produce (`image/<ext>`).
const EXTENSIONS: [&str; 4] = ["png", "jpeg", "gif", "webp"];

/// Why an image request failed; [`status`](Self::status) is the HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageApiError {
    /// 400: malformed id or empty body.
    BadRequest(String),
    /// 403: a recorded image resolves outside the image jail.
    Forbidden(String),
    /// 404: unknown upload id / not recorded in this session / file gone.
    NotFound(String),
    /// 409: the file no longer matches the recorded size or hash.
    Mismatch(String),
    /// 413: over the per-image cap.
    TooLarge(String),
    /// 415: not a png/jpeg/gif/webp image.
    Unsupported(String),
    /// 503: no data directory to store uploads in.
    Unavailable(String),
    /// 507: a quota would be exceeded.
    Quota(String),
    /// 500: anything else (I/O).
    Internal(String),
}

impl ImageApiError {
    pub fn status(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Forbidden(_) => 403,
            Self::NotFound(_) => 404,
            Self::Mismatch(_) => 409,
            Self::TooLarge(_) => 413,
            Self::Unsupported(_) => 415,
            Self::Unavailable(_) => 503,
            Self::Quota(_) => 507,
            Self::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::BadRequest(m)
            | Self::Forbidden(m)
            | Self::NotFound(m)
            | Self::Mismatch(m)
            | Self::TooLarge(m)
            | Self::Unsupported(m)
            | Self::Unavailable(m)
            | Self::Quota(m)
            | Self::Internal(m) => m,
        }
    }
}

/// Upload quotas in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quotas {
    pub per_session: u64,
    pub total: u64,
}

impl Quotas {
    /// Quotas from the env values; an unparsable value keeps the default
    /// (reported on stderr).
    pub fn from_values(per_session: Option<&str>, total: Option<&str>) -> Self {
        fn parse(name: &str, v: Option<&str>, default: u64) -> u64 {
            match v.map(str::trim).filter(|v| !v.is_empty()) {
                None => default,
                Some(v) => v.parse().unwrap_or_else(|_| {
                    eprintln!("tole serve: {name}={v:?} is not a byte count; using {default}");
                    default
                }),
            }
        }
        Self {
            per_session: parse(SESSION_QUOTA_ENV, per_session, DEFAULT_SESSION_QUOTA),
            total: parse(TOTAL_QUOTA_ENV, total, DEFAULT_TOTAL_QUOTA),
        }
    }

    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(SESSION_QUOTA_ENV).ok().as_deref(),
            std::env::var(TOTAL_QUOTA_ENV).ok().as_deref(),
        )
    }
}

/// FNV-1a 64 as the core records it (`tole_core::image::ImageRef::fnv1a64`).
fn fnv1a64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// An image id is the content hash: exactly 16 lowercase hex digits. This
/// is also what keeps a client-supplied id from naming any other file.
fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The client-facing form of an uploaded reference (no server path).
pub fn upload_json(r: &ImageRef) -> Value {
    json!({"id": r.fnv1a64, "mime": r.mime, "bytes": r.bytes, "fnv1a64": r.fnv1a64})
}

/// Total size of the regular files under `dir` (recursive); a missing
/// directory is empty. Dot-files (in-flight temp files) are not counted.
fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// The serve upload store: `<root>/<session id>/<fnv1a64>.<ext>`.
pub struct UploadStore {
    root: PathBuf,
    quotas: Quotas,
    /// Serializes quota check + write, so two concurrent uploads cannot
    /// both pass a check that only one of them fits.
    lock: Mutex<()>,
}

impl UploadStore {
    pub fn new(root: PathBuf, quotas: Quotas) -> Self {
        Self {
            root,
            quotas,
            lock: Mutex::new(()),
        }
    }

    /// The store under the host spill dir (`<data dir>/images/serve`) with
    /// the env quotas; `None` when no data directory resolves.
    pub fn from_env() -> Option<Self> {
        crate::turn_runner::spill_dir().map(|d| Self::new(d.join("serve"), Quotas::from_env()))
    }

    pub fn quotas(&self) -> Quotas {
        self.quotas
    }

    fn session_dir(&self, session_id: &str) -> Result<PathBuf, ImageApiError> {
        crate::session_host::validate_session_id(session_id)
            .ok_or_else(|| ImageApiError::BadRequest("invalid session id".into()))?;
        Ok(self.root.join(session_id))
    }

    /// The stored file of `id` in `dir`, if any.
    fn existing(dir: &Path, id: &str) -> Option<PathBuf> {
        EXTENSIONS
            .iter()
            .map(|ext| dir.join(format!("{id}.{ext}")))
            .find(|p| p.is_file())
    }

    /// Resolve `file` through the core (jail = the session dir) and require
    /// it to still hash to `id`.
    fn resolve_as(file: &Path, dir: &Path, id: &str) -> Result<ImageRef, ImageApiError> {
        let r = tole_core::image::resolve(
            &file.to_string_lossy(),
            std::slice::from_ref(&dir.to_path_buf()),
            None,
        )
        .map_err(|e| ImageApiError::Mismatch(format!("stored image {id}: {}", e.0)))?;
        if r.fnv1a64 != id {
            return Err(ImageApiError::Mismatch(format!(
                "stored image {id} no longer matches its hash"
            )));
        }
        Ok(r)
    }

    /// Store `data` as an image of `session_id` and return its reference.
    /// Re-uploading the same bytes returns the existing reference without
    /// consuming quota.
    pub fn store(&self, session_id: &str, data: &[u8]) -> Result<ImageRef, ImageApiError> {
        if data.is_empty() {
            return Err(ImageApiError::BadRequest("empty image body".into()));
        }
        if data.len() as u64 > MAX_IMAGE_BYTES {
            return Err(ImageApiError::TooLarge(format!(
                "image exceeds the {MAX_IMAGE_BYTES}-byte cap"
            )));
        }
        let dir = self.session_dir(session_id)?;
        let id = fnv1a64(data);
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(file) = Self::existing(&dir, &id) {
            return Self::resolve_as(&file, &dir, &id);
        }
        let len = data.len() as u64;
        let used = dir_size(&dir);
        if used + len > self.quotas.per_session {
            return Err(ImageApiError::Quota(format!(
                "per-session image quota exceeded: {used} of {} bytes used, upload is {len} bytes \
                 ({SESSION_QUOTA_ENV})",
                self.quotas.per_session
            )));
        }
        let total = dir_size(&self.root);
        if total + len > self.quotas.total {
            return Err(ImageApiError::Quota(format!(
                "total image quota exceeded: {total} of {} bytes used, upload is {len} bytes \
                 ({TOTAL_QUOTA_ENV})",
                self.quotas.total
            )));
        }
        std::fs::create_dir_all(&dir).map_err(|e| ImageApiError::Internal(e.to_string()))?;
        // The temp name is a dot-file (not counted, never an id) and unique
        // per upload; the core then sniffs, caps and hashes it in place.
        let tmp = dir.join(format!(".upload-{id}-{}.tmp", std::process::id()));
        std::fs::write(&tmp, data).map_err(|e| ImageApiError::Internal(e.to_string()))?;
        let sniffed =
            tole_core::image::resolve(&tmp.to_string_lossy(), std::slice::from_ref(&dir), None);
        let r = match sniffed {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(if e.0.contains("not a png/jpeg/gif/webp") {
                    ImageApiError::Unsupported("not a png/jpeg/gif/webp image".into())
                } else {
                    ImageApiError::Internal(e.0)
                });
            }
        };
        let ext = r.mime.trim_start_matches("image/");
        let file = dir.join(format!("{id}.{ext}"));
        if let Err(e) = std::fs::rename(&tmp, &file) {
            let _ = std::fs::remove_file(&tmp);
            return Err(ImageApiError::Internal(e.to_string()));
        }
        Self::resolve_as(&file, &dir, &id)
    }

    /// The reference of upload `id` of `session_id` (for a prompt's
    /// `images`). Only that session's uploads are visible.
    pub fn lookup(&self, session_id: &str, id: &str) -> Result<ImageRef, ImageApiError> {
        if !valid_id(id) {
            return Err(ImageApiError::BadRequest(format!(
                "invalid image id {id:?} (16 hex digits)"
            )));
        }
        let dir = self.session_dir(session_id)?;
        let file = Self::existing(&dir, id).ok_or_else(|| {
            ImageApiError::NotFound(format!(
                "unknown image id {id}: upload it to this session first"
            ))
        })?;
        Self::resolve_as(&file, &dir, id)
    }

    /// [`lookup`](Self::lookup) for every id of a prompt's `images` value
    /// (absent or `null` = none).
    pub fn lookup_all(
        &self,
        session_id: &str,
        images: Option<&Value>,
    ) -> Result<Vec<ImageRef>, ImageApiError> {
        let ids = match images {
            None | Some(Value::Null) => return Ok(vec![]),
            Some(Value::Array(a)) => a,
            Some(_) => {
                return Err(ImageApiError::BadRequest(
                    "images must be an array of image ids".into(),
                ))
            }
        };
        ids.iter()
            .map(|v| {
                let id = v.as_str().ok_or_else(|| {
                    ImageApiError::BadRequest("images must be an array of image ids".into())
                })?;
                self.lookup(session_id, id)
            })
            .collect()
    }
}

/// The reference recorded under `hash` in the session log at `log` (the
/// latest user message carrying it).
fn recorded(log: &Path, hash: &str) -> Result<ImageRef, ImageApiError> {
    use tole_core::storage::Storage;
    let storage = tole_core::storage::JsonlStorage::open_read_only(log)
        .map_err(|e| ImageApiError::Internal(e.to_string()))?;
    storage
        .entries()
        .iter()
        .rev()
        .filter(|e| e.kind == tole_core::entry::EntryType::MESSAGE)
        .filter_map(|e| e.payload.get("images").and_then(Value::as_array))
        .flatten()
        .filter_map(ImageRef::from_value)
        .find(|r| r.fnv1a64 == hash)
        .ok_or_else(|| ImageApiError::NotFound("image not recorded in this session".into()))
}

/// The bytes of the image recorded under `hash` in the session log at
/// `log`, with its recorded mime. Re-checks the jail (`roots`), the
/// recorded size and the hash of the very bytes returned.
pub fn read_recorded(
    log: &Path,
    hash: &str,
    roots: &[PathBuf],
) -> Result<(String, Vec<u8>), ImageApiError> {
    if !valid_id(hash) {
        return Err(ImageApiError::NotFound(
            "image not recorded in this session".into(),
        ));
    }
    let r = recorded(log, hash)?;
    if !EXTENSIONS
        .iter()
        .any(|ext| r.mime == format!("image/{ext}"))
    {
        return Err(ImageApiError::Mismatch(format!(
            "recorded image {hash} has an unexpected type {:?}",
            r.mime
        )));
    }
    // The core's jailed resolve: canonical path inside the roots, a regular
    // file under the cap, an image by its bytes.
    let checked = tole_core::image::resolve(&r.path, roots, None).map_err(|e| {
        if e.0.contains("outside the workspace jail") {
            ImageApiError::Forbidden(format!("recorded image {hash} is outside the image jail"))
        } else if e.0.contains("No such file") || e.0.contains("cannot find") {
            ImageApiError::NotFound(format!("recorded image {hash} is no longer on disk"))
        } else {
            ImageApiError::Mismatch(format!("recorded image {hash}: {}", e.0))
        }
    })?;
    let data = std::fs::read(&checked.path).map_err(|e| ImageApiError::Internal(e.to_string()))?;
    if data.len() as u64 != r.bytes || fnv1a64(&data) != r.fnv1a64 || checked.mime != r.mime {
        return Err(ImageApiError::Mismatch(format!(
            "recorded image {hash} changed since it was attached"
        )));
    }
    Ok((r.mime, data))
}

/// [`read_recorded`] with the host's image roots (the same jail the provider
/// re-loads images from).
pub fn read_recorded_in_jail(log: &Path, hash: &str) -> Result<(String, Vec<u8>), ImageApiError> {
    read_recorded(log, hash, &crate::turn_runner::image_roots())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::storage::{Commit, JsonlStorage, Storage};

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89,
    ];

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-serve-img-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn store(root: &Path, per_session: u64, total: u64) -> UploadStore {
        UploadStore::new(root.join("serve"), Quotas { per_session, total })
    }

    #[test]
    fn quotas_parse_bytes_and_fall_back_to_the_defaults() {
        let q = Quotas::from_values(None, None);
        assert_eq!(q.per_session, DEFAULT_SESSION_QUOTA);
        assert_eq!(q.total, DEFAULT_TOTAL_QUOTA);
        let q = Quotas::from_values(Some(" 100 "), Some("junk"));
        assert_eq!(q.per_session, 100);
        assert_eq!(q.total, DEFAULT_TOTAL_QUOTA);
    }

    #[test]
    fn store_sniffs_dedupes_and_lookup_is_session_scoped() {
        let d = tmp("store");
        let s = store(&d, 1 << 20, 1 << 20);
        let r = s.store("s1", PNG).unwrap();
        assert_eq!(r.mime, "image/png");
        assert_eq!(r.fnv1a64, fnv1a64(PNG));
        assert_eq!(upload_json(&r)["id"], json!(r.fnv1a64));
        assert!(
            upload_json(&r).get("path").is_none(),
            "no server path leaks"
        );
        // Same bytes again: the same reference, no second file.
        assert_eq!(s.store("s1", PNG).unwrap(), r);
        assert_eq!(std::fs::read_dir(d.join("serve/s1")).unwrap().count(), 1);
        assert_eq!(s.lookup("s1", &r.fnv1a64).unwrap(), r);
        assert_eq!(s.lookup("s2", &r.fnv1a64).unwrap_err().status(), 404);
        assert_eq!(s.lookup("s1", "../../etc").unwrap_err().status(), 400);
        assert_eq!(s.store("../x", PNG).unwrap_err().status(), 400);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn store_refuses_non_images_empty_and_over_quota() {
        let d = tmp("refuse");
        let s = store(&d, PNG.len() as u64 + 10, 3 * PNG.len() as u64);
        assert_eq!(s.store("s1", b"plain text").unwrap_err().status(), 415);
        assert_eq!(s.store("s1", b"").unwrap_err().status(), 400);
        s.store("s1", PNG).unwrap();
        let mut other = PNG.to_vec();
        other.extend_from_slice(b"more");
        let e = s.store("s1", &other).unwrap_err();
        assert_eq!(e.status(), 507, "{e:?}");
        assert!(e.message().contains(SESSION_QUOTA_ENV), "{e:?}");
        s.store("s2", &other).unwrap();
        let e = s.store("s3", &other).unwrap_err();
        assert_eq!(e.status(), 507, "{e:?}");
        assert!(e.message().contains(TOTAL_QUOTA_ENV), "{e:?}");
        // No temp file is left behind by a refused upload.
        assert!(std::fs::read_dir(d.join("serve/s1"))
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with('.')));
        let _ = std::fs::remove_dir_all(d);
    }

    fn log_with(dir: &Path, refs: &[ImageRef]) -> PathBuf {
        let mut st = JsonlStorage::create(dir, "s", None).unwrap();
        let images: Vec<Value> = refs.iter().map(ImageRef::to_value).collect();
        st.commit(Commit::new().entry(NewEntry::root(
            EntryType::new(EntryType::MESSAGE),
            json!({"role": "user", "text": "look", "images": images}),
        )))
        .unwrap();
        dir.join("s.jsonl")
    }

    #[test]
    fn read_serves_only_recorded_images_and_reports_mismatches() {
        let d = tmp("read");
        let s = store(&d, 1 << 20, 1 << 20);
        let r = s.store("s1", PNG).unwrap();
        let roots = vec![d.join("serve")];
        let log = log_with(&d, std::slice::from_ref(&r));
        let (mime, data) = read_recorded(&log, &r.fnv1a64, &roots).unwrap();
        assert_eq!((mime.as_str(), data.as_slice()), ("image/png", PNG));
        // Uploaded but not recorded, or a junk hash: 404.
        let mut other = PNG.to_vec();
        other.push(1);
        let unrecorded = s.store("s1", &other).unwrap();
        assert_eq!(
            read_recorded(&log, &unrecorded.fnv1a64, &roots)
                .unwrap_err()
                .status(),
            404
        );
        assert_eq!(
            read_recorded(&log, "../x", &roots).unwrap_err().status(),
            404
        );
        // Outside the jail: 403, never served.
        assert_eq!(
            read_recorded(&log, &r.fnv1a64, &[d.join("elsewhere")])
                .unwrap_err()
                .status(),
            403
        );
        // Swapped bytes: 409.
        std::fs::write(&r.path, &other).unwrap();
        let e = read_recorded(&log, &r.fnv1a64, &roots).unwrap_err();
        assert_eq!(e.status(), 409, "{e:?}");
        assert!(e.message().contains("changed"), "{e:?}");
        // Gone: 404.
        std::fs::remove_file(&r.path).unwrap();
        assert_eq!(
            read_recorded(&log, &r.fnv1a64, &roots)
                .unwrap_err()
                .status(),
            404
        );
        let _ = std::fs::remove_dir_all(d);
    }
}
