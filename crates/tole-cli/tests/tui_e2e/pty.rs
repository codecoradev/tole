//! `tole` on a pty: the harness of the TUI e2e tests.

use crate::common::{self, wait_for, WorkDir, WAIT};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
/// `text` without its CSI sequences (`ESC [ ... final byte`), plus, for each
/// byte of the result, the byte offset in `text` it came from.
fn strip_csi(text: &str) -> (String, Vec<usize>) {
    let (mut flat, mut at) = (String::new(), Vec::new());
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\x1b' && chars.peek().is_some_and(|(_, n)| *n == '[') {
            chars.next();
            for (_, n) in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&n) {
                    break;
                }
            }
            continue;
        }
        flat.push(c);
        at.extend(std::iter::repeat_n(i, c.len_utf8()));
    }
    (flat, at)
}

/// `tole` on a fresh pty; everything it prints lands in `out`.
pub struct Pty {
    child: Child,
    master: std::fs::File,
    pub out: Arc<Mutex<Vec<u8>>>,
    /// Set by the reader thread once the pty reports end of output (every
    /// slave end closed and the buffer drained).
    eof: Arc<AtomicBool>,
    seen: usize,
}

impl Pty {
    pub fn spawn(cwd: &Path, url: &str) -> Pty {
        Pty::spawn_with(cwd, url, &[])
    }

    pub fn spawn_with(cwd: &Path, url: &str, args: &[String]) -> Pty {
        Pty::spawn_full(cwd, url, args, &[])
    }

    #[allow(unsafe_code)] // SAFETY: test-only libc call on a path or descriptor the test owns.
    pub fn spawn_full(cwd: &Path, url: &str, args: &[String], envs: &[(&str, &str)]) -> Pty {
        let (mut m, mut sl) = (0, 0);
        let mut ws = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // macOS declares the termios and winsize arguments `*mut`, Linux `*const`;
        // `*mut` coerces to both.
        let rc = unsafe {
            libc::openpty(
                &mut m,
                &mut sl,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(ws),
            )
        };
        assert_eq!(rc, 0, "openpty");
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(sl)) };
        let mut cmd =
            common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &cwd.with_extension("home"), &[]);
        cmd.args(args)
            .current_dir(cwd)
            .env("TOLE_BASE_URL", url)
            .env("TOLE_MODEL", "m")
            .env("TOLE_API_KEY", "k")
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("TERM", "xterm-256color")
            .env("PATH", "/usr/bin:/bin")
            .env_remove("TOLE_MEMORY")
            .env_remove("TOLE_TRUST")
            .env_remove("TOLE_VISION")
            .env_remove("TOLE_THEME")
            .env_remove("NO_COLOR")
            .envs(envs.iter().copied())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn tole");
        drop(slave);
        let master = std::fs::File::from(master);
        let mut reader = master.try_clone().unwrap();
        // The inline viewport asks the terminal for the cursor (CSI 6n); a
        // real terminal answers, so the fixture must too.
        let mut answer = master.try_clone().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        let eof = Arc::new(AtomicBool::new(false));
        let done = eof.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 4096];
            while let Ok(n) = reader.read(&mut b) {
                if n == 0 {
                    break;
                }
                if b[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = answer.write_all(b"\x1b[1;1R");
                }
                sink.lock().unwrap().extend_from_slice(&b[..n]);
            }
            done.store(true, Ordering::SeqCst);
        });
        Pty {
            child,
            master,
            out,
            eof,
            seen: 0,
        }
    }

    pub fn send(&mut self, s: &str) {
        self.master.write_all(s.as_bytes()).unwrap();
        self.master.flush().unwrap();
    }

    /// Wait until `needle` appears in output not yet consumed; consumes up to it.
    pub fn expect(&mut self, needle: &str) {
        let (out, seen) = (self.out.clone(), self.seen);
        let upto = wait_for(WAIT, &format!("{needle:?} in the screen output"), || {
            let o = out.lock().unwrap();
            let text = String::from_utf8_lossy(&o[seen..]).to_string();
            text.find(needle).map(|i| text[..i + needle.len()].len())
        });
        self.seen += upto;
    }

    /// Like [`Pty::expect`], but matches across the terminal's own line
    /// wrapping (#835). The TUI hard-wraps a long status line at the screen
    /// width, so a message that embeds a path (whose length depends on where
    /// the test directory lives) can break inside the needle. Wrapping only
    /// inserts cursor-movement sequences, so the needle is searched in the
    /// output with every CSI sequence removed.
    pub fn expect_unwrapped(&mut self, needle: &str) {
        let (out, seen) = (self.out.clone(), self.seen);
        let what = format!("{needle:?} in the unwrapped screen output");
        let upto = wait_for(WAIT, &what, || {
            let o = out.lock().unwrap();
            let text = String::from_utf8_lossy(&o[seen..]).to_string();
            let (flat, at) = strip_csi(&text);
            flat.find(needle).map(|i| {
                let last = at[i + needle.len() - 1];
                last + text[last..].chars().next().map_or(0, char::len_utf8)
            })
        });
        self.seen += upto;
    }

    /// The turn is over: the idle hint (redrawn when `working…` goes away)
    /// is back, so hotkeys and commands are accepted again.
    pub fn idle(&mut self) {
        self.expect("Shift+Tab thinking");
    }

    pub fn wait_exit(&mut self) -> Option<i32> {
        let child = &mut self.child;
        let mut code = None;
        let exited = common::poll_until(WAIT, || {
            child.try_wait().ok().flatten().map(|st| code = st.code())
        });
        if exited.is_none() {
            let _ = self.child.kill();
        } else {
            // The child is gone, but the reader thread may not have copied
            // its last output (the terminal-restore sequences) yet: an
            // assertion on the final screen must wait for the pty's end of
            // output, not race it (#877). Expiry is not a failure here, a
            // grandchild may still hold the slave end; the caller's own
            // assertion then reports what is missing.
            let eof = self.eof.clone();
            let _ = common::poll_until(WAIT, || eof.load(Ordering::SeqCst).then_some(()));
        }
        code
    }
}

pub fn workdir() -> WorkDir {
    WorkDir::new("tole-tui-e2e")
}

pub fn printed(t: &Pty) -> String {
    String::from_utf8_lossy(&t.out.lock().unwrap()).into_owned()
}
