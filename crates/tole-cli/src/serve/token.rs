//! The serve bearer token (#682, `--token-file`, `--token`, `TOLE_SERVE_TOKEN`) and the
//! password prompt of `tole serve-hash` (#742).
#![allow(unsafe_code)] // SAFETY: termios echo guard of the token prompt; each block states its invariant.

use anyhow::{Context, Result};
use std::io::{BufRead, Read};

/// Largest `--token-file`.
const MAX_TOKEN_FILE: u64 = 4096;

/// The serve bearer token (#682): `--token-file` (a regular file, no link
/// or FIFO, at most 4 KiB, surrounding whitespace dropped; an empty file is
/// an error), else `--token`, else `TOLE_SERVE_TOKEN` (blank counts as
/// unset). Errors never carry the token.
///
/// The token becomes a secret held by role, whatever carried it: redacted
/// on egress, and removed from any child's environment.
pub fn serve_token(
    flag: Option<String>,
    file: Option<&std::path::Path>,
    env: Option<String>,
) -> std::result::Result<String, String> {
    serve_token_optional(flag, file, env)?.ok_or_else(|| {
        "refusing to start an unauthenticated server: set TOLE_SERVE_TOKEN, --token-file \
         or --token"
            .to_string()
    })
}

/// [`serve_token`] where having no token at all is not an error: `--auth
/// basic` may run with the Basic user alone (#742). A file or value that is
/// given but unusable is still an error.
pub fn serve_token_optional(
    flag: Option<String>,
    file: Option<&std::path::Path>,
    env: Option<String>,
) -> std::result::Result<Option<String>, String> {
    let from_file = match file {
        Some(p) => {
            let bytes = tole_core::delegation_report::read_plain_file(p, MAX_TOKEN_FILE)
                .map_err(|e| format!("--token-file {}: {e}", p.display()))?
                .ok_or_else(|| format!("--token-file {}: does not exist", p.display()))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| format!("--token-file {}: not UTF-8", p.display()))?;
            if text.trim().is_empty() {
                return Err(format!("--token-file {}: is empty", p.display()));
            }
            Some(text)
        }
        None => None,
    };
    let token = [from_file, flag, env]
        .into_iter()
        .flatten()
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty());
    if let Some(t) = &token {
        tole_core::secrets::add_role_secret("TOLE_SERVE_TOKEN", t);
    }
    Ok(token)
}

/// `tole serve-hash` (#742): print the password hash `--auth basic` takes.
/// The password comes from stdin: a pipe gives one line, a terminal is asked
/// twice without echo. It is never an argument (visible to every local
/// process) and never printed; only the hash goes to stdout.
pub fn hash_command(iterations: u32) -> Result<()> {
    use std::io::IsTerminal;
    let password = if std::io::stdin().is_terminal() {
        let first = read_hidden("Password: ")?;
        let again = read_hidden("Repeat: ")?;
        if !tole_cli::auth::ct_eq(first.as_bytes(), again.as_bytes()) {
            anyhow::bail!("the two passwords differ");
        }
        first
    } else {
        let mut line = String::new();
        // One line at most: a runaway pipe must not be slurped.
        std::io::stdin()
            .lock()
            .take(4096)
            .read_line(&mut line)
            .context("reading the password from stdin")?;
        line.trim_end_matches(['\n', '\r']).to_string()
    };
    let hash = tole_cli::auth::PasswordHash::create(password.as_bytes(), iterations)
        .map_err(anyhow::Error::msg)?;
    println!("{hash}");
    Ok(())
}

/// Terminal attributes to put back: restores echo when dropped, so an early
/// return or a panic cannot leave the terminal silent. (A signal that kills the
/// process, Ctrl-C included, is not handled: `stty echo` repairs the terminal.)
#[cfg(unix)]
struct EchoGuard {
    fd: std::os::fd::RawFd,
    saved: libc::termios,
}

#[cfg(unix)]
impl EchoGuard {
    /// Turn echo off on `fd`; `None` when it is not a terminal.
    fn off(fd: std::os::fd::RawFd) -> Option<Self> {
        // SAFETY: plain termios calls on a descriptor with a zeroed struct the
        // kernel fills in; nothing is retained but the saved copy.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return None;
            }
            let mut quiet = t;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(fd, libc::TCSANOW, &quiet);
            Some(Self { fd, saved: t })
        }
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `off`.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// Read one line from the terminal with echo off (restored on every exit).
fn read_hidden(prompt: &str) -> Result<String> {
    #[cfg(unix)]
    let _echo = {
        use std::os::fd::AsRawFd;
        EchoGuard::off(std::io::stdin().as_raw_fd())
    };
    #[cfg(not(unix))]
    eprintln!("(this platform cannot hide the typed password; pipe it on stdin instead)");
    eprint!("{prompt}");
    let mut line = String::new();
    let read = std::io::stdin().lock().take(4096).read_line(&mut line);
    eprintln!();
    read.context("reading the password")?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}
