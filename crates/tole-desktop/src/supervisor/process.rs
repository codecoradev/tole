//! Starting `tole serve` as a child of the desktop.
//!
//! `tole serve --socket <socket> --token-fd 0 --workspace <folder>`: the
//! credential is written into a pipe whose read end becomes the child's
//! standard input, so it is in neither the arguments nor the environment of any
//! process (the environment of the child is also cleared of
//! `TOLE_SERVE_TOKEN`). The child gets its own process group, so a Ctrl-C in
//! the terminal that started the desktop, or the desktop's own exit, does not
//! stop it: closing the window leaves the serve running.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use super::token::Token;

/// The last lines of its standard error the desktop keeps.
const TAIL_LINES: usize = 20;
/// A line is cut at this many bytes.
const LINE_BYTES: usize = 512;

/// A running serve that this desktop started.
pub struct Serve {
    child: Child,
    tail: Arc<Mutex<VecDeque<String>>>,
}

impl Serve {
    /// Start it. The caller has already checked the directory and decided that
    /// no serve answers there.
    pub fn spawn(
        binary: &Path,
        socket: &Path,
        workspace: &Path,
        cron: bool,
        token: &Token,
    ) -> io::Result<Serve> {
        let (reader, mut writer) = io::pipe()?;
        // A token is far below the pipe buffer, so this never blocks; closing
        // the write end is what ends the child's read.
        writer.write_all(token.expose().as_bytes())?;
        drop(writer);

        let mut cmd = Command::new(binary);
        cmd.arg("serve")
            .arg("--socket")
            .arg(socket)
            .args(["--token-fd", "0", "--workspace"])
            .arg(workspace);
        if cron {
            cmd.arg("--cron");
        }
        cmd.current_dir(workspace)
            .env_remove("TOLE_SERVE_TOKEN")
            .stdin(Stdio::from(reader))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn()?;
        drop(cmd); // the read end of the pipe is closed in this process
        let stderr = child.stderr.take().expect("stderr is piped");
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let sink = Arc::clone(&tail);
        std::thread::Builder::new()
            .name("tole-serve-stderr".into())
            .spawn(move || drain(stderr, &sink))?;
        Ok(Serve { child, tail })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `Some` once it has exited (and is then reaped).
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// The last lines it wrote to standard error, newest last.
    pub fn last_lines(&self) -> String {
        let lines = self.tail.lock().unwrap_or_else(|p| p.into_inner());
        lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// Stop it now and reap it.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read standard error to the end, keeping the last lines. It never stops
/// reading early: a serve that cannot write to a full pipe would block.
fn drain(mut stderr: impl Read, tail: &Mutex<VecDeque<String>>) {
    let mut chunk = [0u8; 4096];
    let mut line: Vec<u8> = Vec::new();
    let push = |line: &mut Vec<u8>| {
        let text = String::from_utf8_lossy(line).trim_end().to_owned();
        line.clear();
        if text.is_empty() {
            return;
        }
        let mut lines = tail.lock().unwrap_or_else(|p| p.into_inner());
        if lines.len() == TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(text);
    };
    while let Ok(n) = stderr.read(&mut chunk) {
        if n == 0 {
            break;
        }
        for &b in &chunk[..n] {
            if b == b'\n' {
                push(&mut line);
            } else if line.len() < LINE_BYTES {
                line.push(b);
            }
        }
    }
    push(&mut line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_tail_keeps_the_last_lines_and_cuts_a_long_one() {
        let tail = Mutex::new(VecDeque::new());
        let mut input = String::new();
        for i in 0..30 {
            input.push_str(&format!("line {i}\n"));
        }
        input.push_str(&"x".repeat(2000));
        input.push('\n');
        drain(input.as_bytes(), &tail);
        let lines = tail.lock().unwrap();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[TAIL_LINES - 2], "line 29");
        assert_eq!(lines[TAIL_LINES - 1].len(), LINE_BYTES);
    }

    #[test]
    fn the_token_reaches_the_child_on_stdin_and_not_in_its_arguments_or_environment() {
        let _turn = crate::supervisor::fake::spawn_guard();
        let dir = std::env::temp_dir().join(format!("tdproc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("seen");
        // A stand-in for `tole`: records what it was given, then exits.
        let script = dir.join("tole");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n{{ cat; echo; echo \"ARGS: $*\"; env; }} > {}\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let token = Token::generate().unwrap();
        std::env::set_var("TOLE_SERVE_TOKEN", "from-the-parent-environment");
        let mut serve = Serve::spawn(&script, &dir.join("s.sock"), &dir, true, &token).unwrap();
        std::env::remove_var("TOLE_SERVE_TOKEN");
        for _ in 0..200 {
            if serve.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let seen = std::fs::read_to_string(&out).unwrap();
        let (stdin_part, rest) = seen.split_once("ARGS: ").unwrap();
        assert_eq!(
            stdin_part.trim(),
            token.expose(),
            "the token arrives on stdin"
        );
        assert!(
            !rest.contains(token.expose()),
            "not in the arguments or environment"
        );
        assert!(
            rest.contains("serve --socket") && rest.contains("--token-fd 0"),
            "{rest}"
        );
        assert!(rest.contains("--cron"));
        assert!(
            !rest.contains("TOLE_SERVE_TOKEN"),
            "the parent's token variable is cleared"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
