//! Shared subprocess helper: spawn, drain stdout/stderr on reader threads,
//! kill after a timeout. Used by the CLI-shelling tools (`cora_search`,
//! `uteke_search`, `gh`). The turn loop is synchronous — a hung child
//! would freeze the whole agent, so every subprocess gets a hard ceiling.

use std::process::{Command, Output};
use std::time::Duration;

/// Default ceiling for tool subprocesses (mirrors cora_search's E4 value).
pub const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(30);

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
    fn payload_destructive(tokens: &[String]) -> bool {
        let lc: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();
        lc.iter().any(|t| teardown_name(t))
            || (lc.iter().any(|t| t == "rm") && lc.iter().any(|t| recursive_flag(t)))
    }

    fn scan(tokens: &[String], depth: u8) -> Result<(), String> {
        if depth > MAX_WRAPPER_DEPTH || tokens.is_empty() {
            return Ok(());
        }
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
                    if payload_destructive(&inner) {
                        return Err(REFUSE.to_string());
                    }
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
