//! Long-running job tools (issue #59): formalize the improvised
//! nohup-background + poll pattern that agents assembled unprompted for
//! CPU-bound work (30-60s renders) that exceeds the 30s subprocess
//! ceiling of `run_command`.
//!
//! - `job_start` — spawn a command DETACHED (own process group, stdin
//!   null, stdout+stderr appended to a log inside the workspace) and
//!   return immediately with a job id. No shell is involved: the input
//!   is argv-split exactly like `run_command` (a shell script can be
//!   run explicitly as `bash -c "..."`, which the argv split quotes
//!   correctly).
//! - `job_poll`  — report liveness (`ps -p <pid>`) and the last 2000
//!   chars of the log. ReadOnly: the poll loop must be frictionless.
//!
//! The log lives INSIDE the file-tools workspace so the agent can
//! `read_file` the full log through the normal jail.
//!
//! Out of scope (documented, not accidentally missing): killing a job,
//! job listing, and completion predicates — the poll loop plus
//! `read_file` covers the observed workflows; add only with a real use
//! case.

use crate::run_command::RunCommandTool;
use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const JOBS_DIR: &str = "tole-jobs";
const LOG_TAIL_CHARS: usize = 2000;

/// `[a-z0-9-]` — path-safe by construction; no `/`, no `..`.
fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn jobs_root(root: &Path) -> PathBuf {
    root.join(JOBS_DIR)
}

/// `j-<hex ms>-<hex pid>` — same time-ordered shape as session ids.
fn new_job_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("j-{ms:x}-{:x}", std::process::id())
}

/// Read-only introspection of a detached job: running? log tail?
pub struct JobPollTool {
    root: PathBuf,
}

impl JobPollTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// True when `ps` reports the pid alive AND not a zombie. Spawned
    /// children are never waited on (the host may outlive them), so an
    /// exited job stays a zombie of the CLI process; `stat=` starting
    /// with `Z` means dead, not running.
    fn pid_alive(pid: u32) -> Result<bool, String> {
        let out = crate::subprocess::run_with_timeout(
            Command::new("ps")
                .arg("-p")
                .arg(pid.to_string())
                .arg("-o")
                .arg("stat="),
            crate::subprocess::SUBPROCESS_TIMEOUT,
        )?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let state = stdout.trim();
        Ok(!state.is_empty() && !state.starts_with('Z'))
    }
}

impl Tool for JobPollTool {
    fn name(&self) -> &str {
        "job_poll"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn describe(&self, input: &Value) -> String {
        let job = input
            .get("job")
            .and_then(Value::as_str)
            .unwrap_or("<missing job>");
        format!("poll job {job}")
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "job": { "type": "string", "description": "Job id returned by job_start." }
            },
            "required": ["job"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let job = input
            .get("job")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must be {\"job\": \"<id>\"}".to_string())?;
        if !valid_job_id(job) {
            return Err(format!(
                "job_poll: invalid job id {job:?} (allowed: [a-z0-9-], max 64)"
            ));
        }
        let dir = jobs_root(&self.root).join(job);
        let pid: u32 = std::fs::read_to_string(dir.join("pid"))
            .map_err(|_| format!("job_poll: unknown job {job}"))?
            .trim()
            .parse()
            .map_err(|_| format!("job_poll: corrupt pid file for {job}"))?;
        let running = Self::pid_alive(pid)?;
        // Tail the LAST LOG_TAIL_CHARS without loading the whole file:
        // detached job logs grow unbounded (render logs are chatty), so
        // stat → seek near the end → read only a bounded slice.
        const READ_WINDOW: u64 = 8 * 1024;
        let path = dir.join("log");
        let mut log_file =
            std::fs::File::open(&path).map_err(|_| format!("job_poll: no log for {job}"))?;
        let len = log_file
            .metadata()
            .map_err(|e| format!("job_poll: stat log: {e}"))?
            .len();
        // JOB-2 (threat model): a chatty/unpolled job can grow its log
        // without bound. Past 10 MiB, truncate to the LAST MAX_LOG_BYTES
        // of what was ACTUALLY read — the offset derives from the buffer,
        // not the earlier stat, so a concurrent poll that shrunk the file
        // in between can never cause a slice panic. Safe against the
        // append-mode child: its next write lands at the new EOF, no
        // sparse NUL holes.
        const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
        // window = cap + slack; guard on `window` so len - window cannot
        // underflow for sizes between cap and cap+slack (CodeCora #1).
        const SLACK: u64 = 1024;
        let window = MAX_LOG_BYTES + SLACK;
        if len > window {
            // Bounded window: read only the last ~MAX_LOG_BYTES (+ 1 KiB
            // line-boundary slack) instead of the whole file — a multi-GiB
            // log must not OOM the host on first poll (threat-model JOB-2).
            let mut bytes = Vec::new();
            log_file
                .seek(SeekFrom::Start(len - window))
                .map_err(|e| format!("job_poll: seek log: {e}"))?;
            log_file
                .by_ref()
                .take(window)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("job_poll: read log: {e}"))?;
            std::fs::write(&path, &bytes).map_err(|e| format!("job_poll: truncate log: {e}"))?;
        }
        let len = log_file
            .metadata()
            .map_err(|e| format!("job_poll: stat log: {e}"))?
            .len();
        let start = len.saturating_sub(READ_WINDOW);
        use std::io::{Read, Seek, SeekFrom};
        log_file
            .seek(SeekFrom::Start(start))
            .map_err(|e| format!("job_poll: seek log: {e}"))?;
        let mut bytes = Vec::new();
        log_file
            .take(READ_WINDOW)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("job_poll: read log: {e}"))?;
        // Lossy: the seek can land mid-multibyte-char; that one glyph
        // never matters for a tail.
        let buf = String::from_utf8_lossy(&bytes).into_owned();
        let tail: String = if buf.chars().count() > LOG_TAIL_CHARS {
            buf.chars()
                .skip(buf.chars().count() - LOG_TAIL_CHARS)
                .collect()
        } else {
            buf
        };
        Ok(json!({
            "job": job,
            "running": running,
            "log_tail": tail,
            "log_path": format!("{JOBS_DIR}/{job}/log"),
        }))
    }
}

/// Spawn a detached long-running command; returns the job id at once.
pub struct JobStartTool {
    root: PathBuf,
}

impl JobStartTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for JobStartTool {
    fn name(&self) -> &str {
        "job_start"
    }

    fn risk(&self) -> Risk {
        // Spawns arbitrary processes: at least Write, like run_command.
        // Destructive stays reserved for explicit delete-like tools.
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        let line = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("<missing command>");
        match RunCommandTool::split_argv(line) {
            Ok(argv) => format!("start job: {}", argv.join(" ")),
            Err(e) => format!("start job: <unparsable command: {e}>"),
        }
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command line for the long-running job. Split to argv like run_command (no shell); run shell scripts explicitly as 'bash -c \"...\"'. Runs detached in the workspace directory; stdout+stderr go to the job log. Returns the job id immediately — poll with job_poll."
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
            return Err("job_start: command must not be empty".into());
        }
        let argv = RunCommandTool::split_argv(line)?;
        let program = argv
            .first()
            .ok_or_else(|| "job_start: command produced an empty argv".to_owned())?;
        if program.is_empty() {
            return Err("job_start: program name must not be empty".into());
        }
        // RC-1 (threat model): the SAME destructive-argv refusal as
        // run_command — detached execution is not an exemption.
        crate::subprocess::check_destructive_argv(&argv)?;

        let id = new_job_id();
        let dir = jobs_root(&self.root).join(&id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("job_start: creating {}: {e}", dir.display()))?;
        // Job logs can contain arbitrary child output — keep the dir
        // owner-only (threat model: tole-jobs exposure).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let log_path = dir.join("log");
        // Append mode (threat-model JOB-2): the child's writes always
        // land at current EOF, so poll-side truncation cannot create
        // sparse NUL holes behind the child's file offset.
        let log_out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("job_start: log: {e}"))?;
        let log_err = log_out
            .try_clone()
            .map_err(|e| format!("job_start: log: {e}"))?;

        let mut cmd = Command::new(program);
        // Secret env never reaches children (threat-model ENV-1).
        crate::subprocess::scrub_env_for_child(&mut cmd);
        cmd.args(&argv[1..])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err));
        // Detach into its own process group/session: survives the host
        // CLI exiting and terminal hangup (the nohup property, without
        // a shell). Non-unix hosts keep the plain spawn (best effort).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("job_start: failed to spawn (is it on PATH?): {e}"))?;
        let pid = child.id();
        std::fs::write(dir.join("pid"), pid.to_string())
            .map_err(|e| format!("job_start: pid file: {e}"))?;
        Ok(json!({
            "job": id,
            "pid": pid,
            "log_path": format!("{JOBS_DIR}/{id}/log"),
            "note": "running detached; poll with job_poll, read the full log with read_file",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-jobs-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn poll_truncates_runaway_log() {
        let dir = tmpdir("logcap");
        // Craft an oversized log + pid for a live process (this test
        // process itself is alive, so running=true is deterministic).
        let job_dir = dir.join(JOBS_DIR).join("j-logcap-test");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::write(job_dir.join("pid"), std::process::id().to_string()).unwrap();
        let blob = "x".repeat(10 * 1024 * 1024 + 4096);
        std::fs::write(job_dir.join("log"), blob).unwrap();

        let poll = JobPollTool::new(dir.clone());
        let out = poll.execute(json!({ "job": "j-logcap-test" })).unwrap();
        assert_eq!(out["running"], json!(true));
        let len = std::fs::metadata(job_dir.join("log")).unwrap().len();
        // Cap = MAX_LOG_BYTES + the 1 KiB line-boundary slack.
        assert!(
            len <= 10 * 1024 * 1024 + 1024,
            "log must be capped, got {len}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_then_poll_completes_with_log() {
        let dir = tmpdir("cycle");
        let start = JobStartTool::new(dir.clone());
        let out = start
            .execute(json!({ "command": "sh -c 'echo job-done; sleep 0.4'" }))
            .unwrap();
        let job = out["job"].as_str().unwrap().to_string();
        assert!(out["pid"].is_u64(), "spawn must return the pid");
        // Poll immediately: whatever the state, the shape must hold.
        let poll = JobPollTool::new(dir.clone());
        let first = poll.execute(json!({ "job": job })).unwrap();
        assert_eq!(first["job"], json!(job));
        // Give the job time to finish, then: not running, log captured.
        std::thread::sleep(std::time::Duration::from_millis(600));
        let second = poll.execute(json!({ "job": job })).unwrap();
        assert_eq!(second["running"], json!(false));
        assert!(second["log_tail"].as_str().unwrap().contains("job-done"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_rejects_unspawnable_program() {
        let dir = tmpdir("badprog");
        let start = JobStartTool::new(dir);
        let err = start
            .execute(json!({ "command": "definitely-not-a-program-xyz --flag" }))
            .unwrap_err();
        assert!(err.contains("failed to spawn"));
    }

    #[test]
    fn poll_rejects_unknown_job() {
        let dir = tmpdir("unknown");
        let poll = JobPollTool::new(dir);
        let err = poll.execute(json!({ "job": "j-nope" })).unwrap_err();
        assert!(err.contains("unknown job"));
    }

    #[test]
    fn poll_rejects_path_tricks() {
        let dir = tmpdir("tricks");
        let poll = JobPollTool::new(dir);
        assert!(poll.execute(json!({ "job": "../escape" })).is_err());
        assert!(poll.execute(json!({ "job": "j/BAD" })).is_err());
        assert!(poll.execute(json!({ "job": "" })).is_err());
    }

    #[test]
    fn start_rejects_empty_command() {
        let dir = tmpdir("empty");
        let start = JobStartTool::new(dir);
        assert!(start.execute(json!({ "command": "   " })).is_err());
    }

    #[test]
    fn start_is_write_and_poll_is_read_only() {
        let dir = tmpdir("risk");
        assert_eq!(JobStartTool::new(dir.clone()).risk(), Risk::Write);
        assert_eq!(JobPollTool::new(dir).risk(), Risk::ReadOnly);
    }
}
