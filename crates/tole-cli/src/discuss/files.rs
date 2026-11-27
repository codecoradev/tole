//! Background files of a discussion (#1038): files inside the discussion's
//! working folder that the owner names with `--file`. They are passed by
//! reference: the contract record holds only their names (relative to the
//! working folder), and the host reads them when a turn needs them, through
//! the same sanitizer as any other input (configured secret values and token
//! shapes redacted, control and fence characters neutralized, capped at an
//! excerpt), so what an inline voice sees is data inside a fence, never an
//! instruction. Nothing else is redacted: a password written in prose or in a
//! config file goes as written to every inline participant's provider, so the
//! owner names only files they would paste into the question.
//!
//! What is refused, before anything starts and again on every resume (a name
//! read back from a ledger is checked like one just typed): a name
//! outside the working folder (absolute outside it, or with `..`), a symlink
//! (the file or any folder on the way to it), a FIFO or any other file that is
//! not a regular file, a file with more than one hard link (it may be a
//! sensitive file under another name, as `read_file` withholds it), a file over
//! [`MAX_BACKGROUND_FILE_BYTES`] and a file that is not UTF-8 text. A file the
//! sensitive-name rules cover (`.env`, keys) is the only one redacted line by
//! line, as `read_file` shows it.
//!
//! The contract also records the SHA-256 of each file's bytes as the
//! discussion started ([`verify`]): a resume refuses a file that is gone or
//! has changed, so every round reads the same evidence.

use std::path::{Component, Path, PathBuf};

use tole_core::discussion::{
    background_name_ok, sanitize, sha256_hex, Background, MAX_BACKGROUND_EXCERPT_CHARS,
};
use tole_core::secrets::SecretSet;
use tole_core::sensitive;

/// Largest background file read, in bytes (the `read_file` cap). A larger file
/// is refused, not cut: only the excerpt a voice sees is capped.
pub const MAX_BACKGROUND_FILE_BYTES: u64 = 1_048_576;

fn outside(typed: &str) -> String {
    format!("background file {typed:?} is outside the working folder")
}

/// The names of `typed` relative to `workdir`, `/`-separated, as the contract
/// holds them. A relative name may start with `./`; an absolute one must lie
/// under the working folder; a name with `..` or a root is outside it. Only
/// the text of the names is looked at here: [`load`] is what opens the files.
pub fn relative(workdir: &Path, typed: &[String]) -> Result<Vec<String>, String> {
    let root = workdir.canonicalize().map_err(|e| {
        format!(
            "cannot resolve the working folder {}: {e}",
            workdir.display()
        )
    })?;
    typed
        .iter()
        .map(|t| {
            let p = Path::new(t);
            let rel: &Path = if p.is_absolute() {
                p.strip_prefix(&root)
                    .or_else(|_| p.strip_prefix(workdir))
                    .map_err(|_| outside(t))?
            } else {
                p
            };
            let mut parts = Vec::new();
            for c in rel.components() {
                match c {
                    Component::Normal(n) => parts.push(
                        n.to_str()
                            .ok_or_else(|| format!("background file {t:?} is not valid text"))?,
                    ),
                    Component::CurDir => {}
                    _ => return Err(outside(t)),
                }
            }
            if parts.is_empty() {
                return Err(format!("background file {t:?} names no file"));
            }
            Ok(parts.join("/"))
        })
        .collect()
}

/// Reads `names` (as [`relative`] returns them) from `workdir`: each file's
/// sanitized excerpt, in order. The first file that is refused fails the lot,
/// naming it and why.
pub fn load(
    workdir: &Path,
    names: &[String],
    secrets: &SecretSet,
) -> Result<Vec<Background>, String> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let root = workdir.canonicalize().map_err(|e| {
        format!(
            "cannot resolve the working folder {}: {e}",
            workdir.display()
        )
    })?;
    names.iter().map(|n| one(&root, n, secrets)).collect()
}

/// Checks that `loaded` (the files as read now) are the ones the contract
/// recorded: the same names, each with the digest it had when the discussion
/// started. A file that changed, or a contract that records no digest for a
/// file, is refused with the file named.
pub fn verify(names: &[String], recorded: &[String], loaded: &[Background]) -> Result<(), String> {
    for (i, b) in loaded.iter().enumerate() {
        match recorded.get(i) {
            Some(d) if names.get(i) == Some(&b.path) && *d == b.sha256 => {}
            Some(_) => {
                return Err(format!(
                    "background file {}: it changed since the discussion started (its SHA-256 \
                     differs from the one the contract recorded), and every round must read the \
                     same evidence. Start a new discussion with the file as it is now",
                    b.path
                ))
            }
            None => {
                return Err(format!(
                    "background file {}: the contract records no SHA-256 for it, so it cannot \
                     be checked; start a new discussion",
                    b.path
                ))
            }
        }
    }
    Ok(())
}

fn one(root: &Path, name: &str, secrets: &SecretSet) -> Result<Background, String> {
    let refuse = |why: &str| format!("background file {name}: {why}");
    // A name that comes out of a ledger on resume is as untrusted as one just
    // typed: the same rule as the contract's, before any path is built (a `..`
    // or a drive prefix would walk out of the working folder).
    if !background_name_ok(name) {
        return Err(refuse(
            "is not a relative name of plain components (no `..`, no backslash or colon)",
        ));
    }
    let mut path: PathBuf = root.to_path_buf();
    // Every step is looked at without following it, so a symlinked folder on
    // the way is refused like a symlinked file.
    for part in name.split('/') {
        path.push(part);
        let m = std::fs::symlink_metadata(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                refuse("not found in the working folder")
            } else {
                refuse(&format!("cannot be read: {e}"))
            }
        })?;
        if m.file_type().is_symlink() {
            return Err(refuse("a symlink (or inside one) is refused"));
        }
    }
    let bytes = read_checked(&path).map_err(|e| refuse(&e))?;
    let sha256 = sha256_hex(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| refuse("is not UTF-8 text"))?;
    let named = Path::new(name);
    let sensitive = sensitive::is_sensitive_path(named, &path);
    let text = if sensitive {
        sensitive::redact_content(named, &path, &text)
    } else {
        text
    };
    let s = sanitize(&text, secrets, MAX_BACKGROUND_EXCERPT_CHARS);
    Ok(Background {
        path: name.to_string(),
        text: s.text,
        sha256,
        truncated: s.truncated,
        redacted: s.redacted || sensitive,
    })
}

/// The bytes of the regular file at `path`, at most
/// [`MAX_BACKGROUND_FILE_BYTES`]. Opened without following a final symlink and
/// without waiting for a FIFO's writer; the kind, the hard-link count and the
/// size are checked on the OPEN descriptor, so a final component swapped for a
/// symlink, a FIFO or a hard-linked file after the path checks is refused.
/// Folders on the way are checked by path only (best effort): swapping one for
/// a symlink between the walk and the open needs a concurrent writer inside
/// the working folder and is not caught here.
fn read_checked(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "not found in the working folder".to_string()
        } else {
            format!("cannot be read: {e}")
        }
    })?;
    let m = f.metadata().map_err(|e| format!("cannot be read: {e}"))?;
    if !m.is_file() {
        return Err("not a regular file".into());
    }
    if sensitive::has_other_names(&m) {
        return Err(
            "has more than one hard link, so it may be a sensitive file under another name".into(),
        );
    }
    let too_big = || format!("larger than {} KiB", MAX_BACKGROUND_FILE_BYTES / 1024);
    if m.len() > MAX_BACKGROUND_FILE_BYTES {
        return Err(too_big());
    }
    let mut buf = Vec::new();
    f.take(MAX_BACKGROUND_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot be read: {e}"))?;
    if buf.len() as u64 > MAX_BACKGROUND_FILE_BYTES {
        return Err(too_big());
    }
    Ok(buf)
}
