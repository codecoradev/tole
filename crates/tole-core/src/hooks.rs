//! Opt-in process hooks at the tool boundary (issue #110) — pattern
//! reference: ZCode's hook surface (Apache-2.0), simplified to the v0
//! deny-only contract.
//!
//! Contract: a hook is an external process. It receives ONE JSON object
//! on stdin (`{"event":"pretool"|"posttool","tool":...,"input":...,
//! "ok":...}`) and may print one JSON object to stdout. **Exit code 2**
//! is a DENY (pre-hooks only): the pending call is settled as a durable
//! error and the loop replans. Any other non-zero exit, non-JSON
//! stdout, a timeout, or a spawn failure is a HOOK FAILURE: recorded to
//! stderr, never blocks the call. argv execution only (no shell
//! string). Hooks fire for Write/Destructive tools only — ReadOnly
//! tools stay zero-overhead.

use crate::subprocess::run_with_timeout_stdin;
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

/// One configured hook process: program + fixed argv (split at the CLI
/// boundary; no quoting support in v0 — wrap complex commands in a
/// script).
#[derive(Debug, Clone)]
pub struct ProcessHook {
    program: String,
    args: Vec<String>,
    timeout: Duration,
}

/// The deny reason a pre-hook returned (exit 2), truncated for the
/// durable error entry.
pub type DenyReason = String;

impl ProcessHook {
    /// Five seconds per hook: a hook slower than that is a broken
    /// policy endpoint, and the loop must not stall behind it.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    pub fn new(command_line: &str) -> Self {
        let mut parts = command_line.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_default();
        Self {
            program,
            args: parts.collect(),
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Run the hook. `Ok(None)` = allow/no-op; `Ok(Some(reason))` =
    /// deny (exit 2, pre-hooks only); `Err` = hook failure
    /// (non-blocking by contract — callers log and continue).
    pub fn run(
        &self,
        event: &str,
        tool: &str,
        input: &Value,
        ok: Option<bool>,
    ) -> Result<Option<DenyReason>, String> {
        let mut payload = json!({ "event": event, "tool": tool, "input": input });
        if let Some(ok) = ok {
            payload["ok"] = json!(ok);
        }
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        // Hook processes are external programs: strip secret-shaped env
        // (API keys etc.) exactly like every other child spawn (cora
        // scan-3 #23 — same contract as run_command/memory).
        crate::subprocess::scrub_env_for_child(&mut cmd);
        let out = run_with_timeout_stdin(&mut cmd, self.timeout, payload.to_string().as_bytes())?;
        // Output cap: a runaway hook cannot flood the durable log.
        let mut stdout = out.stdout;
        stdout.truncate(32 * 1024);
        let stdout = String::from_utf8_lossy(&stdout).trim().to_string();
        match out.status.code() {
            Some(0) => Ok(None),
            Some(2) => {
                let reason = if stdout.is_empty() {
                    format!("denied by {event} hook")
                } else {
                    format!("denied by {event} hook: {stdout}")
                };
                Ok(Some(reason))
            }
            Some(code) => Err(format!("hook `{}` exited {code}", self.program)),
            None => Err(format!("hook `{}` killed by signal", self.program)),
        }
    }
}

/// Pre/post hook sets wired onto a registry. `None` (the default) means
/// hooks are off and the loop pays nothing.
#[derive(Debug, Clone, Default)]
pub struct ToolHooks {
    pub pre: Vec<ProcessHook>,
    pub post: Vec<ProcessHook>,
}

impl ToolHooks {
    /// Build from CLI flag lists (`--on-pretool`, `--on-posttool`).
    pub fn from_cli(pre: &[String], post: &[String]) -> Self {
        Self {
            pre: pre.iter().map(|s| ProcessHook::new(s)).collect(),
            post: post.iter().map(|s| ProcessHook::new(s)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn hook(cmd: &str) -> ProcessHook {
        let mut h = ProcessHook::new(cmd);
        h.timeout = Duration::from_secs(10);
        h
    }

    static SCRIPT_SEQ: AtomicU32 = AtomicU32::new(0);

    /// Write a shell script and hook it — whitespace splitting means
    /// complex hooks are SCRIPT FILES, which is the documented v0
    /// contract ("wrap complex commands in a script").
    fn script_hook(body: &str) -> (ProcessHook, std::path::PathBuf) {
        let n = SCRIPT_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tole-hook-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hook.sh");
        std::fs::write(&path, body).unwrap();
        (hook(&format!("/bin/sh {}", path.display())), dir)
    }

    #[test]
    fn exit_zero_is_a_noop() {
        let (h, dir) = script_hook("exit 0\n");
        assert_eq!(
            h.run("pretool", "write_file", &json!({}), None).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_two_denies_with_stdout_reason() {
        let (h, dir) = script_hook("echo no-writes-today\nexit 2\n");
        let r = h
            .run("pretool", "write_file", &json!({"path": "x"}), None)
            .unwrap();
        assert_eq!(r.unwrap(), "denied by pretool hook: no-writes-today");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_two_without_stdout_gets_a_default_reason() {
        let (h, dir) = script_hook("exit 2\n");
        let r = h.run("pretool", "run_command", &json!({}), None).unwrap();
        assert_eq!(r.unwrap(), "denied by pretool hook");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_nonzero_exit_is_a_hook_failure_not_a_deny() {
        let (h, dir) = script_hook("exit 3\n");
        assert!(h.run("pretool", "write_file", &json!({}), None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stdin_payload_reaches_the_hook() {
        let capture =
            std::env::temp_dir().join(format!("tole-hook-payload-{}.json", std::process::id()));
        let body = format!("cat > {}\necho -n ok\n", capture.display());
        let (h, dir) = script_hook(&body);
        assert_eq!(
            h.run("posttool", "write_file", &json!({"path": "x"}), Some(true))
                .unwrap(),
            None
        );
        let payload: Value = serde_json::from_str(&std::fs::read_to_string(&capture).unwrap())
            .expect("hook received valid JSON on stdin");
        assert_eq!(payload["event"], "posttool");
        assert_eq!(payload["tool"], "write_file");
        assert_eq!(payload["input"]["path"], "x");
        assert_eq!(payload["ok"], true);
        let _ = std::fs::remove_file(&capture);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_is_a_hook_failure() {
        let (mut h, dir) = script_hook("sleep 5\n");
        h.timeout = Duration::from_millis(300);
        assert!(h.run("pretool", "write_file", &json!({}), None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
