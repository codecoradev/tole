//! `run_command` tool (B4): generic dynamic command execution.
//!
//! Design (owner-approved, TOML DSL rejected for UX):
//! - Input is a single command line string; it is **split to argv** with a
//!   small shlex-compatible splitter — there is NO shell involved. The
//!   approval prompt shows the exact argv, so what the user approves is
//!   what runs.
//! - Runs with cwd jailed to the session workdir and a hard timeout.
//! - Risk ceiling is `Write` — structurally below Destructive; it can
//!   never be auto-allowed via `--yes` in a way that bypasses the gate.
//!
//! The splitter supports: whitespace separation, single quotes (no
//! escapes inside), double quotes (backslash-escapes `"` `\` `$` and
//! backtick), and backslash-escaping of the next char outside quotes.

use crate::subprocess::{run_with_timeout, SUBPROCESS_TIMEOUT};
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

/// Generic command runner. `cwd` is the jail: the command runs with this
/// working directory and cannot be re-pointed via the input.
#[derive(Debug, Clone)]
pub struct RunCommandTool {
    /// Jail directory (the session cwd).
    pub cwd: PathBuf,
    /// Child-agent mode (#171): additionally refuse spawning the tole
    /// binary itself — a child session bypassing the depth cap via
    /// run_command is the verified escape hatch (2026-10-05).
    pub child_agent_mode: bool,
    /// The secret release for its children (#598); `None`: the process's.
    pub release: Option<crate::secret_release::Release>,
}

impl RunCommandTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            child_agent_mode: false,
            release: None,
        }
    }

    pub fn in_child_agent_mode(mut self) -> Self {
        self.child_agent_mode = true;
        self
    }

    /// Split a command line into argv (shlex-compatible subset). Public
    /// for tests. Returns an error string for unterminated quotes.
    pub fn split_argv(line: &str) -> Result<Vec<String>, String> {
        let mut argv: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut chars = line.chars().peekable();
        let mut started = false; // seen a non-space char for the current token
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' | '\n' => {
                    if started {
                        argv.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                '\'' => {
                    started = true;
                    loop {
                        match chars.next() {
                            Some('\'') => break,
                            Some(ch) => cur.push(ch),
                            None => return Err("unterminated single quote".into()),
                        }
                    }
                }
                '"' => {
                    started = true;
                    loop {
                        match chars.next() {
                            Some('"') => break,
                            Some('\\') => match chars.next() {
                                Some(e @ ('"' | '\\' | '$' | '`')) => cur.push(e),
                                Some(other) => {
                                    cur.push('\\');
                                    cur.push(other);
                                }
                                None => return Err("unterminated escape".into()),
                            },
                            Some(ch) => cur.push(ch),
                            None => return Err("unterminated double quote".into()),
                        }
                    }
                }
                '\\' => {
                    started = true;
                    match chars.next() {
                        Some(e) => cur.push(e),
                        None => return Err("trailing backslash".into()),
                    }
                }
                other => {
                    started = true;
                    cur.push(other);
                }
            }
        }
        if started {
            argv.push(cur);
        }
        Ok(argv)
    }
}

impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn risk(&self) -> Risk {
        // Generic execution is at least Write. The ceiling is structural:
        // Destructive is reserved for explicit delete-like tools with
        // their own gates; run_command never classifies itself higher.
        Risk::Write
    }

    fn summary(&self) -> String {
        "Run one command (program plus arguments, no shell) in the workspace and return its output."
            .into()
    }

    fn describe(&self, input: &Value) -> String {
        let line = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("<missing command>");
        match Self::split_argv(line) {
            // Re-quote tokens with whitespace/quotes/metacharacters so
            // the approval prompt preserves token boundaries (cora
            // full-scan #16): `printf a b` and `printf a b` (two args)
            // must render differently.
            Ok(argv) => format!(
                "run: {}",
                argv.iter()
                    .map(|a| quote_for_display(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Err(e) => format!("run: <unparsable command: {e}>"),
        }
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command line to run. Split to argv with shell-like quoting (single/double quotes, backslash escapes); no shell features (pipes, &&, redirects) — those are literal argv characters. Runs with cwd jailed to the session directory."
                }
            },
            "required": ["command"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let line = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must be {\"command\": \"...\"}".to_string())?;
        if line.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        let argv = Self::split_argv(line)?;
        let program = argv
            .first()
            .ok_or_else(|| "command produced an empty argv".to_owned())?;
        if program.is_empty() {
            return Err("program name must not be empty".into());
        }
        // No shell → no PATH trickery beyond what exec itself resolves;
        // still refuse obvious absolute/relative path forms so the audit
        // line always names a program, not a path traversal.
        // RC-1 (threat model): destructive argv refused pre-spawn, via
        // the shared helper (also applied by job_start).
        crate::subprocess::check_destructive_argv(&argv)?;
        if self.child_agent_mode {
            crate::agents::check_child_agent_argv(&argv)?;
        }
        // #684, every session kind: never the tole binary, never the
        // session marker.
        crate::session_guard::check_session_argv(&argv, &self.cwd)?;
        let mut cmd = Command::new(program);
        // Secret env never reaches children (threat-model ENV-1) unless a
        // `[secrets]` entry grants it to `run_command` (#598).
        let release = self
            .release
            .clone()
            .unwrap_or_else(crate::secret_release::release);
        crate::subprocess::scrub_env_with(&mut cmd, "run_command", &release);
        crate::session_guard::mark(&mut cmd);
        cmd.args(&argv[1..]).current_dir(&self.cwd);
        let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let status = out.status.code().unwrap_or(-1);
        let (stdout, stdout_truncated) = cap_output(&stdout, 4000);
        let (stderr, stderr_truncated) = cap_output(&stderr, 2000);
        Ok(json!({
            "status": status,
            "stdout": stdout,
            "stderr": stderr,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        }))
    }
}

/// Cap `s` to `budget` chars. Over budget, keep head and tail (build and
/// test failures sit at the END) joined by a marker stating the omitted
/// count; the marker counts against the budget. Returns (text, truncated).
fn cap_output(s: &str, budget: usize) -> (String, bool) {
    let total = s.chars().count();
    if total <= budget {
        return (s.to_owned(), false);
    }
    // Reserve room for the widest possible count so the result never
    // exceeds the budget.
    let overhead = "\n[...  characters omitted ...]\n".chars().count() + total.to_string().len();
    let Some(kept) = budget.checked_sub(overhead).filter(|k| *k > 0) else {
        // Budget too small for a marker: plain head truncation.
        return (s.chars().take(budget).collect(), true);
    };
    let (head, tail) = (kept / 2, kept - kept / 2);
    let mut out: String = s.chars().take(head).collect();
    out.push_str(&format!(
        "\n[... {} characters omitted ...]\n",
        total - kept
    ));
    out.extend(s.chars().skip(total - tail));
    (out, true)
}

/// Re-quote a single argv token for DISPLAY (cora full-scan #16):
/// tokens containing whitespace, quotes, or shell metacharacters get
/// single-quoted (with embedded single quotes widened to `'\''`) so an
/// approval prompt shows the true token boundaries — `printf 'a b'`
/// (one arg with a space) renders differently from `printf a b` (two
/// args).
fn quote_for_display(tok: &str) -> String {
    let needs_quoting = tok.is_empty()
        || tok.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\'' | '"' | '$' | '`' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '\\'
                )
        });
    if !needs_quoting {
        return tok.to_string();
    }
    format!("'{}'", tok.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catastrophic_argv_refused() {
        let t = RunCommandTool::new(PathBuf::from("/tmp"));
        for line in [
            "rm -rf /",
            "rm --recursive /",
            "rm -r ../outside",
            "rm -rf /etc",
            "rm -rf ~",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            "shutdown -h now",
            "/bin/rm -rf /",
            "sh -c 'rm -rf /etc'",
            "bash -lc 'dd if=/dev/zero of=/dev/sda'",
            "env FOO=1 rm -rf /",
        ] {
            let err = t
                .execute(json!({ "command": line }))
                .err()
                .unwrap_or_else(|| panic!("must refuse: {line}"));
            assert!(
                err.contains("destructive beyond the Write risk tier"),
                "{line}"
            );
        }
    }

    /// Token matching must not over-refuse benign commands whose ARGUMENT
    /// TEXT merely contains destructive keywords (CodeCora #3), and
    /// path-qualified PROGRAMS are refused (wrapper/PATH bypass).
    #[test]
    fn benign_commands_with_destructive_looking_args_pass() {
        // ISOLATED jail (cora full-scan #18): this test EXECUTES
        // `rm -rf ./build` — running it against the SHARED temp dir
        // deleted any peer test's files living at <temp>/build. Each
        // run gets its own scratch jail with a `build` dir inside.
        let jail = std::env::temp_dir().join(format!(
            "tole-run-benign-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(jail.join("build")).unwrap();
        std::fs::write(jail.join("middleware.ts"), "x").unwrap();
        std::fs::write(jail.join("runbook.md"), "x").unwrap();
        let t = RunCommandTool::new(jail.clone());
        for line in [
            "cat middleware.ts",
            "grep shutdown runbook.md",
            "git add file",
            "cargo add serde",
            "echo useradd",
        ] {
            let out = t.execute(json!({ "command": line }));
            assert!(out.is_ok(), "must NOT refuse: {line}");
        }
        // Scoped recursive rm inside the workspace stays allowed (Write
        // tier territory), including dot-segment noise that normalizes
        // inside.
        assert!(t.execute(json!({ "command": "rm -rf ./build" })).is_ok());
        assert!(t
            .execute(json!({ "command": "rm -rf ./a/../build" }))
            .is_ok());
        let _ = std::fs::remove_dir_all(&jail);
        // Path-qualified program: refused (PATH resolution is the way).
        let err = t
            .execute(json!({ "command": "/bin/echo hi" }))
            .expect_err("path-qualified program must be refused");
        assert!(err.contains("destructive beyond the Write risk tier"));
    }

    #[test]
    fn classified_write_never_destructive() {
        let t = RunCommandTool::new(PathBuf::from("/tmp"));
        assert_eq!(t.risk(), Risk::Write);
    }

    #[test]
    fn split_basic_and_quoted() {
        let argv = RunCommandTool::split_argv("echo 'hello world' \"a \\\"b\\\"\" x\\ y").unwrap();
        assert_eq!(argv, vec!["echo", "hello world", "a \"b\"", "x y"]);
    }

    #[test]
    fn split_empty_line_is_empty_argv() {
        assert!(RunCommandTool::split_argv("   ").unwrap().is_empty());
    }

    #[test]
    fn split_unterminated_quotes_error() {
        assert!(RunCommandTool::split_argv("echo 'oops").is_err());
        assert!(RunCommandTool::split_argv("echo \"oops").is_err());
    }

    #[test]
    fn describe_shows_exact_argv() {
        let t = RunCommandTool::new(PathBuf::from("/tmp"));
        let d = t.describe(&json!({"command": "ls -la 'my dir'"}));
        // Token boundaries preserved: the quoted arg renders re-quoted.
        assert_eq!(d, "run: ls -la 'my dir'");
    }

    #[test]
    fn describe_quotes_ambiguous_tokens() {
        let t = RunCommandTool::new(PathBuf::from("/tmp"));
        // One arg WITH a space renders quoted...
        let one = t.describe(&json!({"command": "printf 'a b'"}));
        assert_eq!(one, "run: printf 'a b'");
        // ...differently from two args (the cora full-scan #16 point).
        let two = t.describe(&json!({"command": "printf a b"}));
        assert_eq!(two, "run: printf a b");
        assert_ne!(one, two);
        // Embedded single quotes widen to '\''; metacharacters quoted.
        let semi = t.describe(&json!({"command": "echo 'x; y'"}));
        assert_eq!(semi, r"run: echo 'x; y'");
        let meta = t.describe(&json!({"command": "echo a;b"}));
        assert_eq!(meta, r"run: echo 'a;b'");
    }

    #[test]
    fn describe_unparsable_is_flagged() {
        let t = RunCommandTool::new(PathBuf::from("/tmp"));
        let d = t.describe(&json!({"command": "echo 'oops"}));
        assert!(d.contains("unparsable"), "{d}");
    }

    #[test]
    fn execute_rejects_missing_and_empty() {
        let t = RunCommandTool::new(PathBuf::from("/tmp"));
        assert!(t.execute(json!({})).is_err());
        assert!(t.execute(json!({"command": "   "})).is_err());
    }

    #[test]
    fn execute_runs_and_reports_status() {
        let t = RunCommandTool::new(std::env::temp_dir());
        let out = t.execute(json!({"command": "sh -c 'exit 3'"})).unwrap();
        assert_eq!(out["status"], 3);
    }

    #[test]
    fn execute_cwd_is_jailed() {
        // The command sees the jail as its working directory.
        let jail = std::env::temp_dir().join("tole-runcommand-jail");
        std::fs::create_dir_all(&jail).unwrap();
        let t = RunCommandTool::new(jail.clone());
        let out = t.execute(json!({"command": "pwd"})).unwrap();
        // POSIX pwd prints the logical path; canonicalize both sides.
        let got = std::path::Path::new(out["stdout"].as_str().unwrap().trim())
            .canonicalize()
            .unwrap_or_default();
        let want = jail.canonicalize().unwrap_or_default();
        assert_eq!(got, want);
    }

    fn n(c: char, k: usize) -> String {
        std::iter::repeat_n(c, k).collect()
    }

    #[test]
    fn cap_exact_budget_unchanged_one_over_flagged() {
        assert_eq!(cap_output(&n('a', 4000), 4000), (n('a', 4000), false));
        assert!(cap_output(&n('a', 4001), 4000).1);
    }

    #[test]
    fn cap_keeps_head_tail_and_exact_omitted_count() {
        let s = format!("HEAD_MARK{}TAIL_MARK", n('y', 40_000));
        let total = s.chars().count();
        let (out, cut) = cap_output(&s, 4000);
        assert!(cut);
        assert!(out.starts_with("HEAD_MARK") && out.ends_with("TAIL_MARK"));
        assert!(out.chars().count() <= 4000);
        let kept = out.chars().count() - out.split('\n').nth(1).unwrap().chars().count() - 2;
        let want = format!("[... {} characters omitted ...]", total - kept);
        assert_eq!(out.split('\n').nth(1).unwrap(), want);
    }

    #[test]
    fn cap_multibyte_cuts_on_char_boundaries() {
        for c in ['é', '😀'] {
            let (out, cut) = cap_output(&n(c, 10_000), 2000);
            assert!(cut && out.chars().count() <= 2000);
            assert!(out.starts_with(c) && out.ends_with(c));
        }
    }

    #[test]
    fn cap_tiny_budget_falls_back_to_head() {
        assert_eq!(cap_output("abcdef", 3), ("abc".to_owned(), true));
        assert_eq!(cap_output("abcdef", 0), (String::new(), true));
    }

    #[test]
    fn execute_caps_streams_independently_and_flags() {
        let t = RunCommandTool::new(std::env::temp_dir());
        let out = t
            .execute(json!({"command": "sh -c 'echo ok; echo ok >&2'"}))
            .unwrap();
        assert_eq!(out["stdout_truncated"], false);
        assert_eq!(out["stderr_truncated"], false);
        assert_eq!(out["stdout"], "ok\n");
        // 3000 chars: over the stderr budget (2000), under stdout's (4000).
        let out = t
            .execute(json!({"command": "sh -c 'head -c 3000 /dev/zero | tr \"\\0\" x | tee /dev/stderr'"}))
            .unwrap();
        assert_eq!(out["stdout_truncated"], false);
        assert_eq!(out["stderr_truncated"], true);
        assert!(out["stderr"].as_str().unwrap().chars().count() <= 2000);
    }

    #[test]
    fn execute_long_output_keeps_head_and_tail() {
        let t = RunCommandTool::new(std::env::temp_dir());
        let cmd =
            r#"sh -c 'printf HEAD_MARK; head -c 40000 /dev/zero | tr "\0" y; printf TAIL_MARK'"#;
        let out = t.execute(json!({ "command": cmd })).unwrap();
        let s = out["stdout"].as_str().unwrap();
        assert!(
            s.starts_with("HEAD_MARK") && s.ends_with("TAIL_MARK"),
            "{s}"
        );
        assert!(s.contains("characters omitted"));
        assert_eq!(out["stdout_truncated"], true);
    }

    /// #598: a command gets no secret by default, `GITHUB_TOKEN` included;
    /// an explicit grant to `run_command` restores that one token.
    #[cfg(unix)]
    #[test]
    fn a_command_gets_no_secret_unless_one_is_granted_to_run_command() {
        use crate::secret_release::Release;
        std::env::set_var("GITHUB_TOKEN", "t598-gh-token-value");
        std::env::set_var("TOLE_T598_OTHER_TOKEN", "t598-other-value");
        let out = |t: &RunCommandTool, name: &str| {
            t.execute(json!({"command": format!("printenv {name}")}))
                .unwrap()["stdout"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // The process release (the default here): nothing for commands.
        let mut t = RunCommandTool::new(std::env::temp_dir());
        assert_eq!(out(&t, "GITHUB_TOKEN"), "");
        t.release = Some(Release::default().with(
            "GITHUB_TOKEN",
            ["gh", "git", "run_command"].map(String::from),
        ));
        assert_eq!(out(&t, "GITHUB_TOKEN").trim(), "t598-gh-token-value");
        assert_eq!(out(&t, "TOLE_T598_OTHER_TOKEN"), "", "only the granted one");
    }

    /// #684: the tole binary is refused in EVERY session kind, not only
    /// child agents (the default tool is a root session's).
    #[cfg(unix)]
    #[test]
    fn root_session_refuses_the_tole_binary() {
        let dir = std::env::temp_dir().join(format!("tole-rc-684-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = RunCommandTool::new(dir.clone());
        assert!(!t.child_agent_mode);
        for line in [
            "tole cron resume job-1",
            "env tole cron resume job-1",
            "sh -c 'tole plugin approve x --hash h'",
            "env -u TOLE_UNDER_SESSION printenv",
        ] {
            let err = t
                .execute(json!({ "command": line }))
                .err()
                .unwrap_or_else(|| panic!("must refuse: {line}"));
            assert!(err.contains("#684"), "{line}: {err}");
        }
        // Ordinary commands that mention the name still run.
        let ok = t.execute(json!({ "command": "echo tole" })).unwrap();
        assert_eq!(ok["status"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #684: what a session starts carries the marker the owner-only
    /// subcommands refuse on.
    #[cfg(unix)]
    #[test]
    fn spawned_commands_carry_the_session_marker() {
        let dir = std::env::temp_dir().join(format!("tole-rc-684m-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = RunCommandTool::new(dir.clone());
        let out = t
            .execute(json!({ "command": "sh -c 'printenv | grep ^TOLE_UNDER'" }))
            .unwrap();
        assert_eq!(out["status"], 0, "{out}");
        let env = out["stdout"].as_str().unwrap();
        assert!(env.lines().any(|l| l == "TOLE_UNDER_SESSION=1"), "{env}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
