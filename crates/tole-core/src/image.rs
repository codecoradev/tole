//! Image input (#212): resolve an image to a durable *reference* and
//! re-load it as a data URL when the provider request is built.
//!
//! The write-once log stores only the reference (`path`, `mime`, `bytes`,
//! `fnv1a64`), never the blob. Every load is jailed to the allowed roots
//! (the workspace, plus the host's spill dir for inline images) and checks
//! the recorded size/hash, so a file swapped after the turn is reported
//! instead of silently sent.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Per-image size cap: larger files would bloat every request of the
/// session (the image is re-sent on each step).
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Durable reference to an image; this is what the log records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Absolute, canonical path.
    pub path: String,
    /// `image/png|jpeg|gif|webp`, sniffed from the bytes.
    pub mime: String,
    /// File size in bytes.
    pub bytes: u64,
    /// FNV-1a 64 of the content (change detection, not security).
    pub fnv1a64: String,
}

/// Why an image could not be resolved or loaded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("image: {0}")]
pub struct ImageError(pub String);

fn err<T>(m: impl Into<String>) -> Result<T, ImageError> {
    Err(ImageError(m.into()))
}

impl ImageRef {
    /// Log-payload form (an element of the user message's `images`).
    pub fn to_value(&self) -> Value {
        json!({"path": self.path, "mime": self.mime, "bytes": self.bytes, "fnv1a64": self.fnv1a64})
    }

    /// Inverse of [`to_value`](Self::to_value); `None` on a malformed element.
    pub fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            path: v.get("path")?.as_str()?.to_string(),
            mime: v.get("mime")?.as_str()?.to_string(),
            bytes: v.get("bytes")?.as_u64()?,
            fnv1a64: v.get("fnv1a64")?.as_str()?.to_string(),
        })
    }
}

fn fnv1a64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Canonicalize `p` and require it to sit under one of `roots`
/// (a relative `p` is resolved against the first root).
fn jailed(p: &str, roots: &[PathBuf]) -> Result<PathBuf, ImageError> {
    let raw = Path::new(p);
    let joined = match (raw.is_absolute(), roots.first()) {
        (true, _) => raw.to_path_buf(),
        (false, Some(r)) => r.join(raw),
        (false, None) => return err("no workspace root to resolve a relative path"),
    };
    let canon = joined
        .canonicalize()
        .map_err(|e| ImageError(format!("{p}: {e}")))?;
    let inside = roots
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .any(|r| canon.starts_with(r));
    if !inside {
        return err(format!("{p}: outside the workspace jail — refused"));
    }
    Ok(canon)
}

fn read_checked(canon: &Path) -> Result<Vec<u8>, ImageError> {
    let meta = std::fs::metadata(canon).map_err(|e| ImageError(e.to_string()))?;
    if !meta.is_file() {
        return err(format!("{}: not a regular file", canon.display()));
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return err(format!(
            "{}: {} bytes exceeds the {MAX_IMAGE_BYTES}-byte cap",
            canon.display(),
            meta.len()
        ));
    }
    std::fs::read(canon).map_err(|e| ImageError(format!("{}: {e}", canon.display())))
}

/// Resolve `spec` — a file path inside `roots` (workspace first), or an
/// inline `data:image/…;base64,…` URL — to an [`ImageRef`]. Inline images
/// are written to `spill_dir` (content-addressed) so the log can keep a
/// reference; without a `spill_dir` they are refused.
pub fn resolve(
    spec: &str,
    roots: &[PathBuf],
    spill_dir: Option<&Path>,
) -> Result<ImageRef, ImageError> {
    let canon = if let Some(rest) = spec.strip_prefix("data:") {
        let (_, b64) = rest
            .split_once(";base64,")
            .ok_or_else(|| ImageError("inline image must be a data:…;base64, URL".into()))?;
        let data = STANDARD
            .decode(b64.trim())
            .map_err(|e| ImageError(format!("inline image: bad base64: {e}")))?;
        if data.len() as u64 > MAX_IMAGE_BYTES {
            return err(format!(
                "inline image exceeds the {MAX_IMAGE_BYTES}-byte cap"
            ));
        }
        let mime =
            sniff(&data).ok_or_else(|| ImageError("inline image: not png/jpeg/gif/webp".into()))?;
        let dir =
            spill_dir.ok_or_else(|| ImageError("inline images need a spill directory".into()))?;
        std::fs::create_dir_all(dir).map_err(|e| ImageError(e.to_string()))?;
        let file = dir.join(format!("{}.{}", fnv1a64(&data), &mime[6..]));
        std::fs::write(&file, &data).map_err(|e| ImageError(e.to_string()))?;
        file.canonicalize().map_err(|e| ImageError(e.to_string()))?
    } else {
        jailed(spec, roots)?
    };
    let data = read_checked(&canon)?;
    let mime = sniff(&data).ok_or_else(|| {
        ImageError(format!(
            "{}: not a png/jpeg/gif/webp image",
            canon.display()
        ))
    })?;
    Ok(ImageRef {
        path: canon.to_string_lossy().into_owned(),
        mime: mime.to_string(),
        bytes: data.len() as u64,
        fnv1a64: fnv1a64(&data),
    })
}

/// Re-load a recorded reference as a `data:` URL for the wire. Re-checks
/// the jail and that the file still matches what the log recorded.
pub fn data_url(r: &ImageRef, roots: &[PathBuf]) -> Result<String, ImageError> {
    let canon = jailed(&r.path, roots)?;
    let data = read_checked(&canon)?;
    if data.len() as u64 != r.bytes || fnv1a64(&data) != r.fnv1a64 {
        return err(format!("{}: changed since it was attached", r.path));
    }
    Ok(format!("data:{};base64,{}", r.mime, STANDARD.encode(data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1x1 PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89,
    ];

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-image-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn resolves_a_jailed_path_and_roundtrips_to_a_data_url() {
        let ws = tmp("ok");
        std::fs::write(ws.join("a.png"), PNG).unwrap();
        let r = resolve("a.png", std::slice::from_ref(&ws), None).unwrap();
        assert_eq!(r.mime, "image/png");
        assert_eq!(r.bytes, PNG.len() as u64);
        assert_eq!(ImageRef::from_value(&r.to_value()), Some(r.clone()));
        let url = data_url(&r, &[ws]).unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    #[test]
    fn refuses_a_path_outside_the_jail() {
        let ws = tmp("jail");
        let outside = tmp("jail-out");
        std::fs::write(outside.join("x.png"), PNG).unwrap();
        let e = resolve(
            outside.join("x.png").to_str().unwrap(),
            std::slice::from_ref(&ws),
            None,
        )
        .unwrap_err();
        assert!(e.0.contains("outside the workspace jail"), "{e}");
        // `..` escape and a symlink escape are both caught by canonicalize.
        let e = resolve("../x.png", std::slice::from_ref(&ws), None).unwrap_err();
        assert!(e.0.contains("outside") || e.0.contains("No such"), "{e}");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("x.png"), ws.join("l.png")).unwrap();
            let e = resolve("l.png", &[ws], None).unwrap_err();
            assert!(e.0.contains("outside the workspace jail"), "{e}");
        }
    }

    #[test]
    fn refuses_non_images_and_oversize() {
        let ws = tmp("bad");
        std::fs::write(ws.join("t.png"), b"just text").unwrap();
        let e = resolve("t.png", std::slice::from_ref(&ws), None).unwrap_err();
        assert!(e.0.contains("not a png/jpeg/gif/webp"), "{e}");
        let big = vec![0u8; MAX_IMAGE_BYTES as usize + 1];
        std::fs::write(ws.join("b.png"), big).unwrap();
        assert!(resolve("b.png", &[ws], None).unwrap_err().0.contains("cap"));
    }

    #[test]
    fn inline_data_url_is_spilled_and_referenced() {
        let ws = tmp("inl");
        let spill = ws.join("spill");
        let spec = format!("data:image/png;base64,{}", STANDARD.encode(PNG));
        let r = resolve(&spec, &[], Some(&spill)).unwrap();
        assert!(r
            .path
            .starts_with(spill.canonicalize().unwrap().to_str().unwrap()));
        assert!(data_url(&r, std::slice::from_ref(&spill))
            .unwrap()
            .starts_with("data:image/png"));
        assert!(resolve(&spec, &[], None).is_err(), "no spill dir → refused");
        assert!(resolve("data:image/png;base64,!!!", &[], Some(&spill)).is_err());
    }

    #[test]
    fn a_swapped_file_is_reported_not_sent() {
        let ws = tmp("swap");
        std::fs::write(ws.join("a.png"), PNG).unwrap();
        let r = resolve("a.png", std::slice::from_ref(&ws), None).unwrap();
        let mut other = PNG.to_vec();
        other.push(0);
        std::fs::write(ws.join("a.png"), other).unwrap();
        assert!(data_url(&r, &[ws]).unwrap_err().0.contains("changed"));
    }
}
