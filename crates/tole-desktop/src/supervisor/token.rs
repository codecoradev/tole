//! The per-launch credential of the local serve (owner decision D4).
//!
//! A fresh random token is made every time the desktop starts a serve. It
//! reaches the serve through an inherited descriptor (see `process`), never the
//! command line or the environment. A second launch of the desktop has to
//! attach to that serve without a new one, so a copy is kept in the private
//! directory in a `0600` file: the same boundary as the socket (only this user
//! can open either), removed when the serve is stopped from the desktop and
//! replaced at every start.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::sys;

/// File name of the credential in the private directory.
pub const TOKEN_FILE: &str = "token";
/// Longest token the desktop accepts from a file.
const MAX_LEN: u64 = 256;

/// A bearer token. `Debug` prints no text.
pub struct Token(String);

impl Token {
    /// 32 random bytes, hex encoded.
    pub fn generate() -> io::Result<Token> {
        let bytes = sys::random_bytes(32)?;
        Ok(Token(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// A token from text read back from the file: printable ASCII, no
    /// whitespace inside, not empty.
    pub fn from_text(text: &str) -> Option<Token> {
        let text = text.trim();
        let ok = !text.is_empty() && text.bytes().all(|b| b.is_ascii_graphic());
        ok.then(|| Token(text.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

pub fn token_path(dir: &Path) -> PathBuf {
    dir.join(TOKEN_FILE)
}

/// Write the token to `<dir>/token`: a new `0600` file, then renamed over the
/// old one, so a reader sees the old token or the new one, never half of it.
pub fn write(dir: &Path, token: &Token) -> io::Result<()> {
    let tmp = dir.join(format!("{TOKEN_FILE}.tmp"));
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    let written = file.write_all(token.expose().as_bytes());
    drop(file);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, token_path(dir))) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Read the token back. `Ok(None)`: there is none. An error: there is a file
/// that must not be trusted (a link, not a regular file, another user's, open
/// to group or other, too large or not a token).
pub fn read(dir: &Path) -> io::Result<Option<Token>> {
    let path = token_path(dir);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let bad = |why: &str| io::Error::new(io::ErrorKind::PermissionDenied, why.to_owned());
    if !meta.file_type().is_file() {
        return Err(bad("the credential file is not a regular file"));
    }
    if meta.uid() != sys::euid() {
        return Err(bad("the credential file belongs to another user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(bad("the credential file is open to other users"));
    }
    let mut text = String::new();
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?
        .take(MAX_LEN + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_LEN {
        return Err(bad("the credential file is too large"));
    }
    Token::from_text(&text)
        .map(Some)
        .ok_or_else(|| bad("the credential file does not hold a token"))
}

/// Remove the credential file; one that is already gone is fine.
pub fn remove(dir: &Path) {
    let _ = std::fs::remove_file(token_path(dir));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tdtok-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_token_is_long_random_hex_and_prints_no_text() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert!(a.expose().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.expose(), b.expose());
        assert!(!format!("{a:?}").contains(a.expose()));
    }

    #[test]
    fn the_file_is_0600_and_reads_back_and_is_replaced_in_one_step() {
        let dir = scratch("rw");
        assert!(read(&dir).unwrap().is_none());
        let first = Token::generate().unwrap();
        write(&dir, &first).unwrap();
        let mode = std::fs::metadata(token_path(&dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(read(&dir).unwrap().unwrap().expose(), first.expose());
        let second = Token::generate().unwrap();
        write(&dir, &second).unwrap();
        assert_eq!(read(&dir).unwrap().unwrap().expose(), second.expose());
        assert!(!dir.join("token.tmp").exists());
        remove(&dir);
        assert!(read(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_must_not_be_trusted_is_an_error_not_a_token() {
        let dir = scratch("bad");
        let path = token_path(&dir);
        std::fs::write(&path, "abc").unwrap();
        for mode in [0o644, 0o640, 0o604] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = read(&dir).unwrap_err();
            assert!(
                e.to_string().contains("open to other users"),
                "{mode:o}: {e}"
            );
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, "x".repeat(300)).unwrap();
        assert!(read(&dir).unwrap_err().to_string().contains("too large"));
        std::fs::write(&path, "two words").unwrap();
        assert!(read(&dir)
            .unwrap_err()
            .to_string()
            .contains("does not hold a token"));
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &path).unwrap();
        assert!(read(&dir)
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
