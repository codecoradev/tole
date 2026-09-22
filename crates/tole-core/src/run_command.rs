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
}

impl RunCommandTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
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
        let mut cmd = Command::new(program);
        // Secret env never reaches children (threat-model ENV-1).
        crate::subprocess::scrub_env_for_child(&mut cmd);
        cmd.args(&argv[1..]).current_dir(&self.cwd);
        let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let status = out.status.code().unwrap_or(-1);
        Ok(json!({
            "status": status,
            "stdout": stdout.chars().take(4000).collect::<String>(),
            "stderr": stderr.chars().take(2000).collect::<String>(),
        }))
    }
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
}
