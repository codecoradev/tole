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
