//! Shared subprocess helper: spawn, drain stdout/stderr on reader threads,
//! kill after a timeout. Used by the CLI-shelling tools (`cora_search`,
//! `uteke_search`, `gh`). The turn loop is synchronous — a hung child
//! would freeze the whole agent, so every subprocess gets a hard ceiling.

use std::process::{Command, Output};
use std::time::Duration;

/// Default ceiling for tool subprocesses (mirrors cora_search's E4 value).
pub const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `cmd`, killing it after `timeout`. Stdout/stderr are drained on
/// dedicated reader threads: a child writing more than the OS pipe buffer
/// (~64KB) would otherwise block on write while we only poll its status,
/// and we'd time out on a perfectly valid run.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, String> {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn subprocess (is it on PATH?): {e}"))?;
    let mut stdout_pipe = child.stdout.take().expect("stdout piped above");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped above");
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "subprocess timed out after {}s and was killed",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Like [`run_with_timeout`], but feeds `stdin_data` to the child on a
/// dedicated writer thread. Used by tools that must pass large content
/// (e.g. document markdown) without argv limits; without the writer
/// thread the classic deadlock applies: child blocks writing stdout
/// while we block writing stdin.
pub fn run_with_timeout_stdin(
    cmd: &mut Command,
    timeout: Duration,
    stdin_data: &[u8],
) -> Result<Output, String> {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn subprocess (is it on PATH?): {e}"))?;
    let mut stdin_pipe = child.stdin.take().expect("stdin piped above");
    let data = stdin_data.to_vec();
    let t_in = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin_pipe.write_all(&data);
        // Drop closes the pipe → child sees EOF.
        drop(stdin_pipe);
    });
    let mut stdout_pipe = child.stdout.take().expect("stdout piped above");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped above");
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "subprocess timed out after {}s and was killed",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let _ = t_in.join(); // writer thread always terminates (write_all or EPIPE)
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod stdin_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn stdin_helper_feeds_data_and_drains_output() {
        // Large output + stdin data: proves no pipe deadlock and that
        // the child actually received the bytes.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("cat > /tmp/tole-stdin-test; head -c 200000 /dev/zero | tr '\\0' 'x'");
        let out = run_with_timeout_stdin(&mut cmd, SUBPROCESS_TIMEOUT, b"hello stdin").unwrap();
        assert!(out.status.success());
        assert_eq!(
            std::fs::read_to_string("/tmp/tole-stdin-test").unwrap(),
            "hello stdin"
        );
        assert_eq!(out.stdout.len(), 200_000);
    }
}
