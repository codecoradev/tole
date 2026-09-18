//! Shared subprocess helper: spawn, drain stdout/stderr on reader threads,
//! kill after a timeout. Used by the CLI-shelling tools (`cora_search`,
//! `uteke_search`, `gh`). The turn loop is synchronous — a hung child
//! would freeze the whole agent, so every subprocess gets a hard ceiling.

use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

/// Default ceiling for tool subprocesses (mirrors cora_search's E4 value).
pub const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-stream capture ceiling: a hostile or chatty child must not be able
/// to exhaust host memory before the timeout fires (CodeCora scan
/// 2026-09-18). Generous on purpose — real tool output (diffs, logs) sits
/// far below it.
const MAX_CAPTURE: u64 = 32 * 1024 * 1024;
const CAPTURE_TRUNCATED_MARK: &[u8] = b"\n\xe2\x80\xa6[tole: output truncated at 32 MiB]\n";

/// How long to wait for the pipe readers after the child is gone. A
/// grandchild that inherited the pipe can hold it open far past the
/// child's exit; joining the reader unconditionally blocked the agent for
/// that whole time (CodeCora scan 2026-09-18). Trade-off: output that has
/// not arrived within the grace window is dropped when a descendant keeps
/// the pipe open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Read a pipe into a buffer under [`MAX_CAPTURE`], then hand it to the
/// waiter. Marks truncation when the cap was hit; the caller-side
/// `take(N + 1)` trick makes cap-hit detection exact.
fn drain_capped(pipe: impl std::io::Read, tx: mpsc::Sender<Vec<u8>>) {
    use std::io::Read;
    let mut pipe = pipe.take(MAX_CAPTURE + 1);
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf);
    if buf.len() > MAX_CAPTURE as usize {
        buf.truncate(MAX_CAPTURE as usize);
        buf.extend_from_slice(CAPTURE_TRUNCATED_MARK);
    }
    let _ = tx.send(buf);
}

/// Bounded wait for one drained stream. On grace expiry the reader thread
/// is left behind on purpose: it terminates by itself once every pipe
/// holder closes (a leaked-but-doomed thread beats an unbounded agent
/// hang).
fn recv_with_grace<T>(rx: &mpsc::Receiver<T>) -> Option<T> {
    rx.recv_timeout(DRAIN_GRACE).ok()
}

/// Environment variable names that must NOT reach child processes
/// (threat-model ENV-1): anything secret-shaped, where "secret-shaped"
/// means the name contains API_KEY, _SECRET, _TOKEN, or PASSWORD
/// (case-insensitive). Exception: GITHUB_TOKEN is kept — gh/git push
/// auth needs it, and its scope is documented in the threat model.
pub fn scrub_env_for_child(cmd: &mut Command) -> &mut Command {
    const KEEP: [&str; 1] = ["GITHUB_TOKEN"];
    for (k, _) in std::env::vars_os() {
        let Some(name) = k.to_str() else { continue };
        let upper = name.to_ascii_uppercase();
        let secret_shaped = upper.contains("API_KEY")
            || upper.contains("_SECRET")
            || upper.contains("_TOKEN")
            || upper.contains("PASSWORD");
        if secret_shaped && !KEEP.contains(&upper.as_str()) {
            cmd.env_remove(name);
        }
    }
    cmd
}

/// RC-1 (threat model): argv TOKENS with unbounded destructive potential
/// are refused before spawn — shared by run_command AND job_start (a
/// detached refusal is worse, not better).
///
/// Design (hardened through four CodeCora review rounds):
/// - Per-TOKEN matching: benign text like `cat middleware.ts` or
///   `grep shutdown runbook.md` never matches destructive names.
/// - Wrapper programs are recursed into: `env dd ...`, `busybox mkfs...`,
///   `nohup shutdown...` (env assignments `VAR=value` and `-u NAME` are
///   skipped before choosing the inner program).
/// - `sh`/`bash` command strings (`-c` inside any short-flag cluster) are
///   scanned CONSERVATIVELY: any teardown-name token, or an `rm` token
///   plus a recursive-flag token anywhere in the payload, refuses the
///   command. A shell parser is out of scope; false positives inside
///   wrapper payloads are acceptable and the error says why.
/// - `rm` with a recursive flag AND a target that escapes the workspace
///   (absolute, tilde, or upward-normalizing relative) is refused; scoped
///   recursive rm stays Write-tier territory. Flag clusters containing
///   r/R count as recursive (`-rfv` included).
/// - Path-qualified programs (`/bin/rm`) are refused outright: bare
///   names resolve via PATH, which is the audited surface.
///
/// SCOPE (read before extending): this is a best-effort blocklist for
/// headline accidental cases, NOT a sandbox. Wrapper/interpreter
/// bypasses (sudo apt, timeout 5 dd, find -delete, python rmtree, ...)
/// are expected; the PRIMARY control for non-ReadOnly commands is the
/// per-call approval gate, and complete argv sandboxing is explicitly
/// out of scope (threat-model RC-1 row / Design Rules: adding a real
/// sandbox is a different product).
///
/// Returns Err(message) when the argv must be refused.
pub fn check_destructive_argv(argv: &[String]) -> Result<(), String> {
    const REFUSE: &str = "refused \u{2014} this command is destructive beyond the Write risk tier (disk/system teardown, or recursive deletion rooted outside the workspace). If you truly need it, ask the operator to run it manually; Destructive-tier classification is tracked in the threat model.";
    const MAX_WRAPPER_DEPTH: u8 = 4;

    fn teardown_name(t: &str) -> bool {
        matches!(
            t,
            "dd" | "shutdown" | "reboot" | "halt" | "poweroff" | "init" | "fdisk"
        ) || t.starts_with("mkfs")
    }

    fn recursive_flag(t: &str) -> bool {
        t == "--recursive"
            || (t.starts_with('-')
                && !t.starts_with("--")
                && t.len() > 1
                && t[1..].contains(['r', 'R']))
    }

    /// Conservative payload scan for shell command STRINGS: a shell
    /// parser is out of scope, so any teardown token anywhere — or an
    /// `rm` token anywhere plus a recursive-flag token anywhere — refuses.
    /// Shell quoting survives naive tokenization (`'rm` is not `rm`), so
    /// tokens are quote-stripped before matching — conservative matcher,
    /// false positives acceptable (CodeCora scan 2026-09-18).
    fn payload_destructive(tokens: &[String]) -> bool {
        let lc: Vec<String> = tokens
            .iter()
            .map(|t| strip_quotes(t).to_ascii_lowercase())
            .collect();
        lc.iter().any(|t| teardown_name(t))
            || (lc.iter().any(|t| t == "rm" || t.ends_with("/rm"))
                && lc.iter().any(|t| recursive_flag(t)))
    }

    /// One layer of shell-quote stripping for the conservative matcher:
    /// `'rm` and `rm'` inside a payload must be seen as `rm`.
    fn strip_quotes(t: &str) -> &str {
        t.trim_start_matches(['\'', '"'])
            .trim_end_matches(['\'', '"'])
    }

    fn scan(tokens: &[String], depth: u8) -> Result<(), String> {
        if depth > MAX_WRAPPER_DEPTH || tokens.is_empty() {
            return Ok(());
        }
        // Quote-strip every token BEFORE matching: single/double-quoted
        // payload words (`sh -c 'rm -rf /'`) must not hide behind their
        // quotes (CodeCora scan 2026-09-18).
        let tokens: Vec<String> = tokens.iter().map(|t| strip_quotes(t).to_string()).collect();
        let prog = tokens[0].to_ascii_lowercase();
        if prog.contains('/') {
            // Path-qualified program escapes PATH-resolution auditing.
            return Err(REFUSE.to_string());
        }
        if teardown_name(&prog) {
            return Err(REFUSE.to_string());
        }
        if prog == "rm" {
            let args = &tokens[1..];
            let recursive = args.iter().any(|a| recursive_flag(&a.to_ascii_lowercase()));
            let escapes = args.iter().any(|a| {
                let l = a.to_ascii_lowercase();
                if l.starts_with('-') {
                    return false; // flags are not targets
                }
                if l.starts_with('/') || l.starts_with('~') {
                    return true;
                }
                let mut d: i32 = 0;
                for comp in l.split('/') {
                    match comp {
                        "" | "." => {}
                        ".." => d -= 1,
                        _ => d += 1,
                    }
                    if d < 0 {
                        return true;
                    }
                }
                false
            });
            if recursive && escapes {
                return Err(REFUSE.to_string());
            }
            return Ok(()); // rm never wraps other programs
        }
        // sudo/doas: recursive (they wrap the real command). timeout and
        // friends are NOT wrapper-recursed — their first non-flag token
        // is a duration, not a program — the conservative full-token
        // scan below still catches teardown names among their args.
        const WRAPPERS: [&str; 8] = [
            "sh", "bash", "env", "busybox", "xargs", "nohup", "sudo", "doas",
        ];
        if WRAPPERS.contains(&prog.as_str()) {
            let args = &tokens[1..];
            // sh/bash command-string mode: any -c inside a short-flag
            // cluster selects the payload; scan EVERY non-flag argument
            // conservatively (compound commands, trailing args).
            if (prog == "sh" || prog == "bash")
                && args.iter().any(|a| {
                    a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                })
            {
                for tok in args.iter().filter(|a| !a.starts_with('-')) {
                    let inner: Vec<String> = tok.split_whitespace().map(str::to_string).collect();
                    // BOTH checks (CodeCora scan + review round-trip):
                    // payload_destructive keeps the conservative
                    // whole-payload match for COMPOUND statements
                    // (`echo hi; rm -rf /` — rm is not the first token),
                    // while full scan() recursion adds what it cannot see:
                    // path-qualified programs and nested shells.
                    if payload_destructive(&inner) {
                        return Err(REFUSE.to_string());
                    }
                    scan(&inner, depth + 1)?;
                }
                return Ok(());
            }
            // Conservative wrapper scan (CodeCora: `xargs -n 1 rm -rf /`
            // derails first-non-flag detection because `-n`'s VALUE is
            // picked as the program; `env -C . ...` breaks out the same
            // way). Wrapper option grammars are wrapper-specific, so
            // instead of guessing which token is the inner program, scan
            // ALL remaining tokens: any teardown name, or an `rm` token
            // plus a recursive-flag token, refuses.
            let lc_args: Vec<String> = args.iter().map(|a| a.to_ascii_lowercase()).collect();
            let has_teardown = lc_args.iter().any(|t| teardown_name(t));
            let has_rm = lc_args.iter().any(|t| t == "rm" || t.ends_with("/rm"));
            let has_recursive = lc_args.iter().any(|t| recursive_flag(t));
            if has_teardown || (has_rm && has_recursive) {
                return Err(REFUSE.to_string());
            }
            // Command strings may be nested in ANY wrapper, not just at
            // top level (`nohup sh -c '...'`, `env bash -c '...'`):
            // re-tokenize every argument that contains whitespace — those
            // are shell command strings — plus apply the conservative
            // token scan to shell wrapper args too (CodeCora: a nested
            // sh/bash previously escaped the payload scan entirely).
            for tok in args.iter().filter(|a| a.contains(char::is_whitespace)) {
                let inner: Vec<String> = tok.split_whitespace().map(str::to_string).collect();
                if payload_destructive(&inner) {
                    return Err(REFUSE.to_string());
                }
                scan(&inner, depth + 1)?;
            }
            let nested_shell = args.iter().any(|a| {
                let l = a.to_ascii_lowercase();
                l == "sh" || l == "bash"
            });
            if nested_shell {
                let has_c = args.iter().any(|a| {
                    a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                });
                if has_c {
                    // A shell command string follows somewhere after -c;
                    // conservative refusal rather than parser guessing.
                    return Err(REFUSE.to_string());
                }
            }
        }
        Ok(())
    }
    scan(argv, 0)
}

/// Runs `cmd`, killing it after `timeout`. Stdout/stderr are drained on
/// dedicated reader threads: a child writing more than the OS pipe buffer
/// (~64KB) would otherwise block on write while we only poll its status,
/// and we'd time out on a perfectly valid run.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, String> {
    use std::process::Stdio;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn subprocess (is it on PATH?): {e}"))?;
    let stdout_pipe = child.stdout.take().expect("stdout piped above");
    let stderr_pipe = child.stderr.take().expect("stderr piped above");
    let (tx_out, rx_out) = mpsc::channel();
    let (tx_err, rx_err) = mpsc::channel();
    std::thread::spawn(move || drain_capped(stdout_pipe, tx_out));
    std::thread::spawn(move || drain_capped(stderr_pipe, tx_err));
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
    let stdout = recv_with_grace(&rx_out).unwrap_or_default();
    let stderr = recv_with_grace(&rx_err).unwrap_or_default();
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
    use std::process::Stdio;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn subprocess (is it on PATH?): {e}"))?;
    let mut stdin_pipe = child.stdin.take().expect("stdin piped above");
    let data = stdin_data.to_vec();
    let (tx_in, rx_in) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin_pipe.write_all(&data);
        // Drop closes the pipe → child sees EOF.
        drop(stdin_pipe);
        let _ = tx_in.send(());
    });
    let stdout_pipe = child.stdout.take().expect("stdout piped above");
    let stderr_pipe = child.stderr.take().expect("stderr piped above");
    let (tx_out, rx_out) = mpsc::channel();
    let (tx_err, rx_err) = mpsc::channel();
    std::thread::spawn(move || drain_capped(stdout_pipe, tx_out));
    std::thread::spawn(move || drain_capped(stderr_pipe, tx_err));
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
    // Writer: bounded grace like the readers — the old unconditional join
    // hung forever when a child (or its descendants) never read stdin
    // (CodeCora review on the scan-triage PR).
    let _ = recv_with_grace(&rx_in);
    let stdout = recv_with_grace(&rx_out).unwrap_or_default();
    let stderr = recv_with_grace(&rx_err).unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod env_scrub_tests {
    use super::*;

    /// The scrubber mutates the Command; verify via a child that prints
    /// its own env, run through run_with_timeout.
    fn child_env(cmd: &mut Command) -> String {
        let out = run_with_timeout(cmd, SUBPROCESS_TIMEOUT).unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    fn secret_shaped_env_removed_but_normal_kept() {
        std::env::set_var("TOLE_TEST_API_KEY", "x");
        std::env::set_var("TOLE_TEST_SECRET_VALUE", "x");
        std::env::set_var("MY_PASSWORD", "x");
        std::env::set_var("TOLE_TEST_KEEP_ME", "visible");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("env");
        let env = child_env(scrub_env_for_child(&mut cmd));
        assert!(!env.contains("TOLE_TEST_API_KEY"));
        assert!(!env.contains("TOLE_TEST_SECRET_VALUE"));
        assert!(!env.contains("MY_PASSWORD"));
        assert!(env.contains("TOLE_TEST_KEEP_ME=visible"));
        assert!(env.contains("PATH="));
        std::env::remove_var("TOLE_TEST_API_KEY");
        std::env::remove_var("TOLE_TEST_SECRET_VALUE");
        std::env::remove_var("MY_PASSWORD");
        std::env::remove_var("TOLE_TEST_KEEP_ME");
    }
}

#[cfg(test)]
mod destructive_argv_tests {
    use super::check_destructive_argv;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn refused_cases() {
        for argv in [
            s(&["rm", "-rf", "/"]),
            s(&["rm", "--recursive", "/"]),
            s(&["rm", "-rfv", "/etc"]), // flag cluster with extra letter
            s(&["rm", "-r", "../outside"]),
            s(&["rm", "-rf", "/home/*"]),
            s(&["rm", "-rf", "./.."]),
            s(&["rm", "-rf", "~"]),
            s(&["/bin/rm", "-rf", "/"]), // path-qualified program
            s(&["dd", "if=/dev/zero", "of=/dev/sda"]),
            s(&["mkfs.ext4", "/dev/sda1"]),
            s(&["shutdown", "-h", "now"]),
            s(&["env", "dd", "if=/dev/zero", "of=/dev/sda"]), // wrapper bypass
            s(&["busybox", "mkfs.ext4", "/dev/sda"]),         // wrapper bypass
            s(&["nohup", "shutdown", "-h", "now"]),           // wrapper bypass
            s(&["sh", "-c", "rm -rf /etc"]),                  // quoted payload
            s(&["bash", "-lc", "dd if=/dev/zero of=/dev/sda"]), // quoted payload
            s(&["xargs", "-n", "1", "rm", "-rf", "/"]),       // option value derails naive scan
            s(&["env", "-C", ".", "rm", "-rf", "/"]),         // env -C dir bypass
            s(&["nohup", "sh", "-c", "dd if=/dev/zero of=/dev/sda"]), // nested sh
            s(&["env", "bash", "-c", "rm -rf /etc"]),         // nested bash
            s(&["sudo", "rm", "-rf", "/"]),                   // sudo wrapper
            s(&["doas", "rm", "-rf", "/"]),                   // doas wrapper
        ] {
            let err = check_destructive_argv(&argv)
                .err()
                .unwrap_or_else(|| panic!("must refuse: {argv:?}"));
            assert!(
                err.contains("destructive beyond the Write risk tier"),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn benign_cases_pass() {
        for argv in [
            s(&["cat", "middleware.ts"]),
            s(&["grep", "shutdown", "runbook.md"]),
            s(&["git", "add", "file"]),
            s(&["cargo", "add", "serde"]),
            s(&["echo", "useradd"]),
            s(&["which", "rm"]), // rm as ARGUMENT TEXT
            s(&["man", "rm"]),
            s(&["echo", "rm -rf is dangerous"]), // rm inside text
            s(&["rm", "-rf", "./build"]),        // scoped, inside ws
            s(&["rm", "-rf", "./a/../build"]),   // normalizes inside
            s(&["rm", "notes.txt"]),             // non-recursive anywhere
            s(&["sh", "-c", "echo hello"]),      // benign payload
            s(&["env", "FOO=1", "echo", "hi"]),  // env with assignment
            s(&["sh", "-c", "cat middleware.ts"]), // benign payload w/ dd-like text
        ] {
            assert!(
                check_destructive_argv(&argv).is_ok(),
                "must NOT refuse: {argv:?}"
            );
        }
    }
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

#[cfg(test)]
mod capture_hardening_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn oversized_output_is_capped_with_marker() {
        // 40 MB of output vs the 32 MiB per-stream capture ceiling.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("head -c 40000000 /dev/zero | tr '\\0' 'x'");
        let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT).unwrap();
        let cap = MAX_CAPTURE as usize;
        assert!(out.stdout.len() > cap - 1024, "cap must be reached");
        assert!(
            out.stdout.len() <= cap + CAPTURE_TRUNCATED_MARK.len(),
            "capture must not exceed cap + marker"
        );
        assert!(out.stdout.ends_with(CAPTURE_TRUNCATED_MARK));
    }

    #[test]
    fn grandchild_holding_pipe_does_not_hang() {
        // `sh` exits at once; the backgrounded sleep inherits the pipe and
        // holds it for 30s. The old join-based drain blocked the agent for
        // that entire window; the drain grace returns in ~2s. Output that
        // has not fully arrived within the grace window is dropped — the
        // documented trade-off.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo started; sleep 30 &");
        let t0 = std::time::Instant::now();
        let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT).unwrap();
        assert!(out.status.success());
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "drain must not wait for the pipe-holding grandchild"
        );
        assert!(out.stdout.is_empty(), "unterminated stream is dropped");
    }
}
