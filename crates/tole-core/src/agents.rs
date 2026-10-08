//! Child agents (issue #171): a parent session spawns N durable child
//! `tole run` sessions — depth capped at ONE (no grandchildren), the
//! mailbox is a per-child uteke namespace, ephemeral by default.
//!
//! Architecture = the jobs pattern (detached spawn + on-disk state +
//! poll), composed with the memory loop's namespace override:
//!
//! - `agent_start` — spawn `<bin> run "<prompt>"` DETACHED with
//!   `TOLE_AGENT_DEPTH=1`, `TOLE_MEMORY_NAMESPACE=agent-<id>`,
//!   `TOLE_MEMORY=uteke`. The child is a full tole session: own JSONL
//!   (inside the agent dir), own registry (BUILT WITHOUT the agent
//!   tools — the structural depth cap), own usage ledger. Its settle
//!   summary lands in its mailbox namespace automatically (the memory
//!   loop), which is the ONLY result channel back up.
//! - `agent_poll`  — liveness + log tail +, once settled, the mailbox
//!   summary; consuming a settled result CLEANS the mailbox (soft,
//!   best-effort) unless the agent was started with `keep_mailbox`.
//!
//! Worktree mode (parent-only operator flag, default OFF): the child
//! gets its own `git worktree` + branch; merge-back stays human.
//!
//! Security invariants:
//! - depth: registration skips these tools entirely when
//!   TOLE_AGENT_DEPTH >= 1 (see the CLI wiring) AND the tool itself
//!   refuses — belt and braces.
//! - the run_command/job_start bypass (spawning `tole` by name) is
//!   closed in child mode by `check_child_agent_argv` (wired via
//!   `in_child_mode` on those tools).
//! - env: the child INHERITS the parent env (it needs the provider
//!   key) — the normal secret scrub is deliberately NOT applied here;
//!   isolation comes from depth + jail + policy visibility instead.

use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

const AGENTS_DIR: &str = "tole-agents";
const LOG_TAIL_CHARS: usize = 2000;
const READ_WINDOW: u64 = 8 * 1024;
/// Hard default for concurrent LIVE children per parent (issue #171).
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
/// A child running longer than this is reported `timed_out` (killing
/// is deliberately out of scope — same contract as jobs).
const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;

/// Refuse any argv that would spawn a depth-0 tole from a child
/// session: a program element that IS the tole binary (any path form),
/// a tole invocation smuggled behind wrappers (`env -u … tole`,
/// `bash -c '… tole …'` — matched as a WORD anywhere in argv), or any
/// manipulation of the TOLE_AGENT_DEPTH marker itself. Checked in
/// addition to the destructive-payload scan.
///
/// Documented residual (CodeCora PR #174 round 1): a RENAMED COPY of
/// the binary (`cp $(which tole) ./t && ./t run`) is not nameably
/// distinguishable — but that is the same authority the child already
/// has to spawn arbitrary subprocesses via run_command (which stays
/// Write-tier + approved/allowlisted by the parent's policy); the
/// depth cap prevents silent budget multiplication, not overt hostile
/// action that is fully visible in the audit log.
pub fn check_child_agent_argv(argv: &[String]) -> Result<(), String> {
    const ERR: &str = "child agents may not spawn or reconfigure the tole binary — the agent \
         tree is capped at one level by design (issue #171)";
    if argv
        .iter()
        // Normalize the same way the token scan below does (CI cora
        // round on #245: `TOLE_AGENT_DEPT""H` split the marker across
        // quote removal; round 5: `${V}` splits it via expansion —
        // both normalize to the literal marker here).
        .any(|a| {
            let stripped: String = a
                .chars()
                .filter(|c| !matches!(c, '"' | '\'' | '\\' | '$' | '{' | '}'))
                .collect();
            stripped.contains("TOLE_AGENT_DEPTH")
        })
    {
        return Err(ERR.to_string());
    }
    // Issue #228 (scan #123): ONE normalized segmentation pass. Every
    // argument is split on whitespace and shell/path/assignment
    // separators (`; & | / =`), then quote/substitution punctuation is
    // stripped — the SAME normalization for the env-clear detection and
    // the binary detection (cora review round 1: matching bare `env`
    // while the binary check normalized paths/quotes let
    // `/usr/bin/env -i …` or `"env" -i` revive the depth-marker wipe).
    // - `env` invocation flags `-i` / `-u` / `--ignore-environment`
    //   (incl. compound shorts like `-iu`) are refused outright: only
    //   an environment-clearing env can strip TOLE_AGENT_DEPTH.
    // - The tole binary is matched AFTER stripping, so `tole"`,
    //   `"tole"`, `t=tole`, `$(... tole)` no longer slip the exact
    //   match; compound fleet names (`tole-agents`, `tole-jobs`) stay
    //   allowed (the hyphen is not a separator).
    {
        // Contexts CROSS argv elements and are NEVER cleared mid-argv
        // (cora review rounds 3+6): `$VAR` indirection and chained
        // assignments (`e=env; f=-i; $e $f …`) can smuggle both the
        // env/exec token and its flag through bare-name tokens, so any
        // clearing rule was evadable. The cost is a small false-refusal
        // window (`grep -i` after an env mention in the SAME argv),
        // accepted for the depth invariant; the structural fix (depth
        // via a marker file the child cannot env-clear) closes the
        // class entirely.
        // Substitution punctuation widens the ENV context (the env
        // keyword itself may be assembled at runtime:
        // `$(printf e)$(printf nv) -i …`, CI cora round on #245) — but
        // ONLY the env side. The earlier version also widened exec and
        // refused every `bash -c "echo $HOME"` (the `-c` flag matched
        // exec's 'c' rule). The exec-keyword-via-substitution case is
        // undecidable at the token level; it is covered by the guard
        // boundary below (overt, auditable) and the structural
        // marker-file follow-up.
        let any_subst = argv
            .iter()
            .any(|a| a.contains('$') || a.contains('`') || a.contains('('));
        let mut saw_env = false;
        let mut saw_exec = false;
        for arg in argv {
            let segments =
                arg.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '/' | '='));
            for seg in segments {
                // Track whether the RAW segment carried substitution
                // punctuation: a wipe flag or keyword can be assembled
                // at runtime (`env -$(echo i)`, `env $(printf '\55')`
                // — rounds 7+10) and cannot be statically cleared.
                let raw_subst = seg.contains('$')
                    || seg.contains('`')
                    || seg.contains('(')
                    || seg.contains(')');
                // Strip QUOTES/backslashes/substitution FIRST (CI cora
                // round 2 on #245: `exec "tole.exe"` — trimming `.exe`
                // before quote-stripping was a no-op while the trailing
                // quote hid it), THEN the `.exe` suffix.
                let tok: String = seg
                    .chars()
                    .filter(|c| !matches!(c, '"' | '\'' | '`' | '$' | '(' | ')' | '{' | '}' | '\\'))
                    .collect();
                let tok = tok.trim_end_matches(".exe").to_string();
                if tok == "env" {
                    saw_env = true;
                    saw_exec = false;
                    continue;
                }
                if tok == "exec" {
                    saw_exec = true;
                    saw_env = false;
                    continue;
                }
                if tok.is_empty() {
                    // Artifact of adjacent separators (`env | grep`):
                    // carries no command semantics — must NOT clear the
                    // wipe context (cora round-4 benign case).
                    continue;
                }
                if tok == "tole" || tok == "tole-cli" {
                    return Err(ERR.to_string());
                }
                if (saw_env || saw_exec) && raw_subst {
                    // ANY substitution-adjacent segment in a wipe
                    // context is refused (cora review round 10): the
                    // flag itself may be built without a dash prefix
                    // (`env $(printf '\55') sh` = `env -i sh`).
                    return Err(ERR.to_string());
                }
                if let Some(body) = tok.strip_prefix('-') {
                    // A dash-token whose raw form carried substitution
                    // punctuation is refused outright in env/exec
                    // context (round 7): `env -$(echo i)` builds the
                    // wipe flag at runtime. Long flag after env: GNU
                    // getopt_long accepts unambiguous abbreviations
                    // (`--ignore-env` wipes exactly like
                    // `--ignore-environment`, cora review round 5), so
                    // ANY long flag in env context is treated as
                    // env-clearing. Short clusters use containment
                    // (`-vi`, `-iu` …), not exclusivity — same for
                    // exec's `-cl` (round 5 #2).
                    // env context: literal `env` seen OR substitution
                    // anywhere (the keyword may be assembled at
                    // runtime). Under a literal env: any long flag
                    // (GNU abbreviation, round 5) or an i/u-containing
                    // short cluster. Under any_subst alone: the named
                    // env-clearing long flags, any i/u-containing
                    // short cluster, or a substitution-built segment
                    // (CI cora round 4: `--ignore-environment` built
                    // next to an assembled keyword; bare `--` in
                    // `rm -- "$f"` stays benign — its body has no
                    // i/u).
                    let env_clear = if saw_env {
                        raw_subst
                            || tok.starts_with("--")
                            || body.chars().any(|c| c == 'i' || c == 'u')
                    } else if any_subst {
                        raw_subst
                            // Long-form env-clearing flags (incl. GNU
                            // abbreviations: `--i`, `--ignore-env` —
                            // CI cora round 5 on #245). Any `--x` flag
                            // next to an assembled env keyword is
                            // refused (undecidable which abbreviation)
                            // EXCEPT the bare `--` end-of-options
                            // marker (`rm -- "$f"`): its strip-prefix
                            // body is the literal "-", not empty.
                            || (tok.starts_with("--") && tok != "--")
                            || (!tok.starts_with("--")
                                && body.chars().any(|c| c == 'i' || c == 'u'))
                    } else {
                        false
                    };
                    let exec_clear = saw_exec
                        && !tok.starts_with("--")
                        && (raw_subst || body.chars().any(|c| c == 'c'));
                    if env_clear || exec_clear {
                        return Err(ERR.to_string());
                    }
                }
            }
        }
    }
    // Guard boundary (documented, issue #228): this is a TOKEN-level
    // check. Two classes are explicitly OUT of scope here:
    // 1. An interpreter script that clears the environment itself
    //    (`python -c 'os.environ.clear(); os.execv(…)'`) — by the
    //    module contract that is OVERT hostile action, fully visible
    //    in the audit log, not silent budget multiplication.
    // 2. Runtime-ASSEMBLED identifiers (`unset TOLE_AGE${V}NT_DEPTH`,
    //    `e=e; f=xec; $e$f …`) — undecidable at the token level: any
    //    static rule either misses a construction or refuses ordinary
    //    variable use. The structural closure is the tracked
    //    follow-up: propagate depth via a marker FILE the child
    //    cannot env-clear, making every one of these constructions
    //    inert.
    Ok(())
}

/// `a-<hex ms>-<hex pid>-<hex n>` — path-safe, collision-free (same
/// shape as job ids).
fn new_agent_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("a-{ms:x}-{:x}-{n:x}", std::process::id())
}

fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn agents_root(root: &std::path::Path) -> PathBuf {
    root.join(AGENTS_DIR)
}

/// Result<bool> like jobs.rs (CodeCora PR #174 round 2): a ps failure
/// must NOT be read as "process dead" — on ps-less hosts that would
/// consume-and-wipe a live child's mailbox and undercount the cap.
fn pid_alive(pid: u32) -> Result<bool, String> {
    let out = crate::subprocess::run_with_timeout(
        Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("stat="),
        crate::subprocess::SUBPROCESS_TIMEOUT,
    )?;
    if !out.status.success() {
        // ps exits non-zero for an unknown pid — that IS "dead".
        return Ok(false);
    }
    let state = String::from_utf8_lossy(&out.stdout);
    let state = state.trim();
    Ok(!state.is_empty() && !state.starts_with('Z'))
}

/// One agent's durable state (meta.json in its dir).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct AgentMeta {
    pub prompt: String,
    pub mailbox_ns: String,
    pub keep_mailbox: bool,
    pub spawned_at_ms: u128,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Consume-once flag, PERSISTED (issue #234): the in-process Mutex
    /// alone lost the fact on every new parent process, so a second
    /// poll re-recalled the mailbox after cleanup — flapping summaries.
    #[serde(default)]
    pub mailbox_consumed: bool,
}

fn read_meta(dir: &std::path::Path) -> Result<AgentMeta, String> {
    let raw =
        std::fs::read_to_string(dir.join("meta.json")).map_err(|e| format!("agent meta: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("agent meta parse: {e}"))
}

fn write_meta(dir: &std::path::Path, meta: &AgentMeta) -> Result<(), String> {
    let raw = serde_json::to_string(meta).map_err(|e| format!("agent meta serialize: {e}"))?;
    std::fs::write(dir.join("meta.json"), raw).map_err(|e| format!("agent meta write: {e}"))
}

/// Count LIVE agents (used to enforce MAX concurrent). Dead-but-
/// unconsumed agents do not block new slots. A liveness-check FAILURE
/// is an error, not zero (CodeCora PR #174 round 2). Callers racing to
/// spawn MUST hold [`agents_root_lock`] around count+register (issue
/// #234: count-then-spawn without a lock lets two parents exceed the
/// cap).
fn live_agents(root: &std::path::Path) -> Result<usize, String> {
    let mut n = 0;
    let root_dir = match std::fs::read_dir(agents_root(root)) {
        Ok(entries) => entries,
        // No agents yet = zero live (the dir appears on first spawn).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("agents dir: {e}")),
    };
    for e in root_dir.flatten() {
        let dir = e.path();
        let Ok(pid) = std::fs::read_to_string(dir.join("pid")) else {
            continue;
        };
        if let Ok(pid) = pid.trim().parse::<u32>() {
            if pid_alive(pid)? {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Serialize the count-then-register critical section of
/// `agent_start` across processes (issue #234, scan #124 TOCTOU): an
/// exclusive flock on `<root>/tole-agents/LOCK`. Held from the
/// liveness count until the child's pid file is written, so two
/// concurrent parents cannot both observe "3 live" and exceed the cap.
/// The lock lives on a FIXED file (not per-agent dirs, which are
/// created after the check) and is advisory — cooperative by design,
/// matching every other tole on-host contract.
#[cfg(unix)]
fn agents_root_lock(root: &std::path::Path) -> Result<std::fs::File, String> {
    let lock = open_lock_file(root)?;
    use std::os::fd::AsRawFd;
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(format!("agents lock: {}", std::io::Error::last_os_error()));
    }
    Ok(lock)
}

/// Non-unix hosts run agents single-parent today; the flock is the unix
/// serialization mechanism. Still open the SAME regular LOCK file (a
/// directory open fails on Windows) so both platforms agree on layout.
#[cfg(not(unix))]
fn agents_root_lock(root: &std::path::Path) -> Result<std::fs::File, String> {
    open_lock_file(root)
}

fn open_lock_file(root: &std::path::Path) -> Result<std::fs::File, String> {
    let dir = agents_root(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("agents dir: {e}"))?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(dir.join("LOCK"))
        .map_err(|e| format!("agents lock: {e}"))
}

/// Best-effort mailbox cleanup (ephemeral default): `uteke forget`
/// each memory in the child's namespace. Soft failure — cleanup is an
/// optimization; the durable child JSONL is the real archive.
fn clean_mailbox(ns: &str) {
    let mut list = Command::new("uteke");
    list.args(["list", "--namespace", ns, "--json"])
        .stdin(Stdio::null());
    let Ok(out) =
        crate::subprocess::run_with_timeout(&mut list, crate::subprocess::SUBPROCESS_TIMEOUT)
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let Ok(Value::Array(items)) =
        serde_json::from_str::<Value>(&String::from_utf8_lossy(&out.stdout))
    else {
        return;
    };
    for id in items
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
    {
        let mut cmd = Command::new("uteke");
        cmd.args(["forget", id, "--namespace", ns]);
        // forget asks for confirmation — answer it.
        let _ = crate::subprocess::run_with_timeout_stdin(
            &mut cmd,
            crate::subprocess::SUBPROCESS_TIMEOUT,
            b"y\n",
        );
    }
}

/// Spawn a durable child agent session (Risk::Write: it runs a full
/// agent turn with tools — the approver sees the prompt and the
/// child's allowlist before anything runs).
pub struct AgentStartTool {
    /// The tole binary to spawn (defaults to the running executable;
    /// injectable for tests).
    pub bin: PathBuf,
    /// File-tools root: agent dirs live under `<root>/tole-agents/`,
    /// and worktrees under `<root>/.tole-worktrees/`.
    pub root: PathBuf,
    /// This session's depth (0 = parent). The tool refuses at >= 1;
    /// registration also skips it (structural belt and braces).
    pub depth: u32,
    /// Parent-only operator mode (default false): each child gets a
    /// git worktree + branch. The MODEL cannot flip this per call.
    pub worktree_mode: bool,
    pub max_concurrent: usize,
    pub timeout_secs: u64,
    /// The PARENT operator's approved Write-allow patterns. The child's
    /// allowlist can never exceed this set: requested patterns must be
    /// glob-covered by a parent pattern, and the default is exactly the
    /// parent's list (CodeCora PR #174 round 2 — a non-interactive
    /// parent must not let the model hand a child broader writes).
    pub parent_allows: Vec<String>,
}

impl AgentStartTool {
    pub fn new(bin: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            root: root.into(),
            depth: 0,
            worktree_mode: false,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            parent_allows: Vec::new(),
        }
    }

    pub fn with_parent_allows(mut self, patterns: Vec<String>) -> Self {
        self.parent_allows = patterns;
        self
    }

    pub fn at_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_worktrees(mut self, on: bool) -> Self {
        self.worktree_mode = on;
        self
    }
}

impl Tool for AgentStartTool {
    fn name(&self) -> &str {
        "agent_start"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn summary(&self) -> String {
        "Start a child tole agent session on one mission; collect its result with agent_poll."
            .into()
    }

    fn describe(&self, input: &Value) -> String {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("<missing prompt>");
        let preview: String = prompt.chars().take(160).collect();
        let allows: Vec<String> = input
            .get("allow")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        format!(
            "start child agent: {:?} (child allowlist: {:?}, worktree: {})",
            preview,
            if allows.is_empty() {
                self.parent_allows.clone()
            } else {
                allows
            },
            self.worktree_mode
        )
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "description": "Spawn a durable child tole session running one mission. The child cannot spawn children (depth cap 1). Its settle summary lands in a per-child uteke mailbox; read it with agent_poll (which also cleans the mailbox once consumed, unless keep_mailbox).",
            "properties": {
                "prompt": { "type": "string", "description": "The child's mission (its one user prompt)" },
                "allow": {
                    "type": "array", "items": { "type": "string" },
                    "description": "The child's Write-tool allow patterns (glob). Keep minimal — the human approves this policy here."
                },
                "keep_mailbox": { "type": "boolean", "description": "Keep the uteke mailbox after the result is consumed (default false — ephemeral). The child's session JSONL is always kept." }
            },
            "required": ["prompt"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        if self.depth >= 1 {
            return Err(
                "agent_start: child agents cannot spawn children — the agent tree is capped at \
                 one level (issue #171)"
                    .into(),
            );
        }
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|p| !p.trim().is_empty())
            .ok_or("agent_start: missing or empty 'prompt'")?;
        let keep_mailbox = input
            .get("keep_mailbox")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let requested: Vec<String> = input
            .get("allow")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        for pat in &requested {
            if pat.starts_with('-') || pat.is_empty() {
                return Err(format!("agent_start: bad allow pattern {pat:?}"));
            }
        }
        // Subset enforcement: every requested pattern must be glob-covered
        // by a PARENT pattern; the default is the parent's own list. The
        // child can never write wider than the operator approved.
        let allows = if requested.is_empty() {
            self.parent_allows.clone()
        } else {
            let uncovered: Vec<&String> = requested
                .iter()
                .filter(|p| {
                    !self
                        .parent_allows
                        .iter()
                        .any(|pp| crate::approval::glob_match(pp, p))
                })
                .collect();
            if !uncovered.is_empty() {
                return Err(format!(
                    "agent_start: child allow pattern(s) {uncovered:?} exceed the parent's \
                     approved scope {:?} — children can never be granted wider writes",
                    self.parent_allows
                ));
            }
            requested
        };
        // Issue #234: hold the flock across count + register — the
        // count-then-spawn window is the TOCTOU the scan flagged. The
        // guard releases when `_lock` drops (spawn errors included).
        let _lock = agents_root_lock(&self.root)?;
        let live = live_agents(&self.root)?;
        if live >= self.max_concurrent {
            return Err(format!(
                "agent_start: {live} child agent(s) already running (cap {}) — poll and \
                 consume their results first",
                self.max_concurrent
            ));
        }

        let id = new_agent_id();
        let dir = agents_root(&self.root).join(&id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("agent_start: creating {}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }

        // Worktree mode (parent-only flag): isolate the child's file
        // mutations in its own worktree + branch; merge stays human.
        let mut child_cwd = self.root.clone();
        let (worktree, branch) = if self.worktree_mode {
            let wt = self.root.join(".tole-worktrees").join(&id);
            let branch = format!("agent/{id}");
            let out = crate::subprocess::run_with_timeout(
                Command::new("git")
                    .args(["worktree", "add"])
                    .arg(&wt)
                    .arg("-b")
                    .arg(&branch)
                    .current_dir(&self.root),
                crate::subprocess::SUBPROCESS_TIMEOUT,
            );
            match out {
                Ok(o) if o.status.success() => {
                    child_cwd = wt.clone();
                    (Some(wt.display().to_string()), Some(branch))
                }
                Ok(o) => {
                    return Err(format!(
                        "agent_start: git worktree add failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                    ));
                }
                Err(e) => return Err(format!("agent_start: git worktree add: {e}")),
            }
        } else {
            (None, None)
        };

        let mailbox_ns = format!("agent-{id}");
        let meta = AgentMeta {
            prompt: prompt.clone(),
            mailbox_ns: mailbox_ns.clone(),
            keep_mailbox,
            spawned_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            worktree: worktree.clone(),
            branch: branch.clone(),
            mailbox_consumed: false,
        };
        write_meta(&dir, &meta)?;

        // The child's sessions live INSIDE its agent dir: the whole dir
        // is the unit of durability + cleanup.
        let sessions_dir = dir.join("sessions");
        let log_path = dir.join("log");
        let log_out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("agent_start: log: {e}"))?;
        let log_err = log_out
            .try_clone()
            .map_err(|e| format!("agent_start: log: {e}"))?;

        let mut cmd = Command::new(&self.bin);
        // Deliberately NO env scrub: the child IS a tole session and
        // needs the provider key. Isolation = depth cap + jail + the
        // approved child allowlist (see the module doc).
        cmd.env("TOLE_AGENT_DEPTH", "1")
            .env("TOLE_MEMORY_NAMESPACE", &mailbox_ns)
            .env("TOLE_MEMORY", "uteke")
            .arg("run")
            .arg("--sessions-dir")
            .arg(&sessions_dir)
            .arg("--workspace")
            .arg(&child_cwd);
        for pat in &allows {
            cmd.arg("--allow").arg(pat);
        }
        // `--` terminates the child's flag parsing: a model-supplied
        // prompt like "--yes real mission" must reach the child as the
        // POSITIONAL prompt, never as approver-escalating flags
        // (CodeCora PR #174 round 1).
        cmd.arg("--")
            .arg(&prompt)
            .current_dir(&child_cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("agent_start: spawning {}: {e}", self.bin.display()))?;
        let pid = child.id();
        // pid file BEFORE detaching: a spawn that reports failure must
        // not leave a live orphan (the jobs.rs invariant — CodeCora PR
        // #174 round 2). On write failure the child is killed.
        if let Err(e) = std::fs::write(dir.join("pid"), pid.to_string()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("agent_start: pid file: {e}"));
        }
        // pid written = the agent is registered — the count now sees it.
        // Release the #234 flock before the (slow) detached run.
        drop(_lock);
        // Detach: the child survives the parent CLI exiting; we never
        // wait on it (poll checks liveness via ps, like jobs).
        drop(child);

        Ok(json!({
            "agent": id,
            "pid": pid,
            "mailbox": mailbox_ns,
            "worktree": worktree,
            "branch": branch,
            "note": "running detached; poll with agent_poll (the result summary arrives via the mailbox)",
        }))
    }
}

/// Poll a child agent: liveness, log tail, and once settled the
/// mailbox summary (consuming it cleans the mailbox by default).
pub struct AgentPollTool {
    pub root: PathBuf,
    pub timeout_secs: u64,
    /// Serialize consume-once: a settled agent's mailbox cleanup must
    /// not race a second poll.
    pub consumed: Mutex<Vec<String>>,
}

impl AgentPollTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            consumed: Mutex::new(Vec::new()),
        }
    }
}

impl Tool for AgentPollTool {
    fn name(&self) -> &str {
        "agent_poll"
    }

    /// `Write`, not `ReadOnly` (issue #300): a settled poll CONSUMES the
    /// mailbox — it forgets the uteke mailbox namespace and persists
    /// `mailbox_consumed` in meta.json. The tier must match the behavior
    /// (a ReadOnly tool must never mutate; unlike `job_poll`, whose
    /// write-back was removed for exactly that reason). Consume-once is
    /// load-bearing (#243/#260/#287), so the tier moves rather than the
    /// side effect. Plan mode drops Write tools, but it also drops
    /// `agent_start`, so no child can exist to poll there.
    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn is_poll(&self) -> bool {
        true
    }

    fn summary(&self) -> String {
        "Poll a child agent started with agent_start for its status and result.".into()
    }

    fn describe(&self, input: &Value) -> String {
        let id = input
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        format!("poll child agent {id}")
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "description": "Agent id returned by agent_start" }
            },
            "required": ["agent"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let id = input
            .get("agent")
            .and_then(Value::as_str)
            .ok_or("agent_poll: input must be {\"agent\": \"<id>\"}")?;
        if !valid_agent_id(id) {
            return Err(format!("agent_poll: invalid agent id {id:?}"));
        }
        let dir = agents_root(&self.root).join(id);
        let pid: u32 = std::fs::read_to_string(dir.join("pid"))
            .map_err(|_| format!("agent_poll: unknown agent {id}"))?
            .trim()
            .parse()
            .map_err(|_| format!("agent_poll: corrupt pid file for {id}"))?;
        let mut meta = read_meta(&dir)?;
        let running =
            pid_alive(pid).map_err(|e| format!("agent_poll: liveness check failed: {e}"))?;
        let elapsed_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
            .saturating_sub(meta.spawned_at_ms);
        let timed_out = running && elapsed_ms > self.timeout_secs as u128 * 1000;

        // Bounded log tail (same read-window discipline as job_poll).
        let mut tail = String::new();
        if let Ok(mut f) = std::fs::File::open(dir.join("log")) {
            use std::io::{Read, Seek, SeekFrom};
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let start = len.saturating_sub(READ_WINDOW);
            let _ = f.seek(SeekFrom::Start(start));
            let mut bytes = Vec::new();
            let _ = f.take(READ_WINDOW).read_to_end(&mut bytes);
            let buf = String::from_utf8_lossy(&bytes).into_owned();
            tail = if buf.chars().count() > LOG_TAIL_CHARS {
                buf.chars()
                    .skip(buf.chars().count() - LOG_TAIL_CHARS)
                    .collect()
            } else {
                buf
            };
        }

        let mut out = json!({
            "agent": id,
            "running": running,
            "timed_out": timed_out,
            "elapsed_secs": elapsed_ms / 1000,
            "log_tail": tail,
            "branch": meta.branch,
        });

        if running {
            return Ok(out);
        }
        // Settled: pull the summary from the mailbox, then clean it
        // (consume-once, unless keep_mailbox). The consumed flag is
        // PERSISTED in meta.json (issue #234) so a later poll —
        // possibly from another parent process — does not resurrect a
        // cleaned mailbox. The in-process Mutex is held across the
        // ENTIRE consume section (issue #260, rescan #155: dropping it
        // after the read let two concurrent polls both recall+clean).
        self.consume_mailbox(&dir, &mut meta, &mut out, crate::memory::recall);
        Ok(out)
    }
}

impl AgentPollTool {
    /// Consume-once mailbox handling for a settled child. The mailbox is
    /// marked consumed and cleaned ONLY when the recall succeeded (hits or
    /// an explicit empty result); on a recall error the result is kept and
    /// the next poll retries (issue #287, regression of #260).
    fn consume_mailbox<F>(
        &self,
        dir: &std::path::Path,
        meta: &mut AgentMeta,
        out: &mut Value,
        recall: F,
    ) where
        F: Fn(&crate::memory::MemoryConfig, &str) -> Result<Vec<crate::memory::RecallHit>, String>,
    {
        let _g = self.consumed.lock();
        if meta.mailbox_consumed {
            return;
        }
        let cfg = crate::memory::MemoryConfig {
            bin: "uteke".into(),
            namespace: meta.mailbox_ns.clone(),
            limit: 3,
        };
        match recall(&cfg, &meta.prompt) {
            Ok(hits) => {
                let summary = hits
                    .iter()
                    .map(|h| h.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                if !summary.is_empty() {
                    out["summary"] = json!(summary);
                }
            }
            Err(e) => {
                out["recall_error"] = json!(format!(
                    "mailbox recall failed; result kept, poll again to retry: {e}"
                ));
                return;
            }
        }
        if !meta.keep_mailbox {
            clean_mailbox(&meta.mailbox_ns);
        }
        meta.mailbox_consumed = true;
        if let Err(e) = write_meta(dir, meta) {
            // Degrade loudly-but-softly: the summary WAS delivered; a
            // failed persist could let a future poll re-recall.
            out["consume_persist_warning"] = json!(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake_child(dir: &std::path::Path) -> PathBuf {
        // A stand-in "tole binary": prints a line, exits 0. Tests drive
        // spawn/poll mechanics without a real agent turn.
        #[cfg(unix)]
        {
            let p = dir.join("fake-tole");
            std::fs::write(&p, "#!/bin/sh\necho FAKE_AGENT_DONE\nexit 0\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
            p
        }
        #[cfg(not(unix))]
        {
            let _ = dir;
            PathBuf::from("true")
        }
    }

    fn sleeper_child(dir: &std::path::Path, secs: u64) -> PathBuf {
        #[cfg(unix)]
        {
            let p = dir.join("sleep-tole");
            std::fs::write(&p, format!("#!/bin/sh\nsleep {secs}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
            p
        }
        #[cfg(not(unix))]
        {
            let _ = (dir, secs);
            PathBuf::from("true")
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        // Unique PER CALL: pid alone is not enough — CI hit "Text file
        // busy" when a second exec of the same fixed dump path raced a
        // still-open write fd (PR #203 run). A monotonic suffix gives
        // every call its own directory; no cross-test exec/write races
        // are possible by construction.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("tole-agents-{tag}-{}-{n:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn child_argv_refuses_tole_binary() {
        assert!(check_child_agent_argv(&["tole".into(), "run".into(), "x".into()]).is_err());
        assert!(check_child_agent_argv(&["/usr/local/bin/tole".into(), "chat".into()]).is_err());
        assert!(check_child_agent_argv(&["tole-cli".into()]).is_err());
        assert!(check_child_agent_argv(&["tole.exe".into()]).is_err());
        assert!(check_child_agent_argv(&["git".into(), "status".into()]).is_ok());
        assert!(check_child_agent_argv(&[]).is_ok());
    }

    /// CodeCora PR #174 round 1: the env-token escape hatch — wrappers
    /// that unset the depth marker or smuggle a tole invocation behind
    /// an interpreter — must be refused, not just the bare binary name.
    #[test]
    fn child_argv_refuses_env_and_wrapper_escapes() {
        for argv in [
            vec!["env", "-u", "TOLE_AGENT_DEPTH", "tole", "run", "x"],
            vec!["bash", "-c", "unset TOLE_AGENT_DEPTH; exec tole run x"],
            vec!["sh", "-c", "TOLE_AGENT_DEPTH=0 tole chat"],
            vec!["env", "TOLE_AGENT_DEPTH=0", "/usr/bin/tole", "run"],
            vec!["bash", "-c", "echo tole-agents && exec tole run"],
        ] {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(
                check_child_agent_argv(&owned).is_err(),
                "must refuse: {argv:?}"
            );
        }
        // benign mentions of the agents dir are NOT the binary
        let ok: Vec<String> = vec![
            "ls".into(),
            "tole-agents/a-1/log".into(),
            "tole-jobs".into(),
        ];
        assert!(check_child_agent_argv(&ok).is_ok());
        // No env/exec token in the argv → no wipe context at all.
        let benign: Vec<Vec<String>> = vec![
            vec!["cp".into(), "-u".into(), "a".into(), "b".into()],
            vec!["bash".into(), "-c".into(), "ls -i notes".into()],
            // Ordinary variable use is NOT a wipe (CI cora round on
            // #245: the removed any_subst widening refused this).
            vec!["bash".into(), "-c".into(), "echo $HOME".into()],
            vec!["bash".into(), "-c".into(), "rm -- \"$f\"".into()],
            vec!["bash".into(), "-c".into(), "grep -c x $(ls)".into()],
        ];
        for v in &benign {
            assert!(
                check_child_agent_argv(v).is_ok(),
                "benign command must pass: {v:?}"
            );
        }
        // Quoted .exe (CI cora round 2 on #245): the suffix trim must
        // run AFTER quote-stripping.
        let quoted_exe: Vec<String> =
            vec!["bash".into(), "-c".into(), "exec \"tole.exe\" run".into()];
        assert!(check_child_agent_argv(&quoted_exe).is_err());
        // Documented over-block (issue #228 trade): a short i/u/c flag
        // AFTER an env/exec token in the SAME argv is refused even when
        // it is not a wipe (`grep -i`) — the never-clear context is the
        // only sound token-level rule against indirection smuggling.
        let over: Vec<String> = vec!["bash".into(), "-c".into(), "env | grep -i PATH".into()];
        assert!(check_child_agent_argv(&over).is_err());
        // Known token-level bypasses (issue #228 guard boundary) —
        // pinned as OK-here assertions: the structural marker-file fix
        // flips these to refused. Runtime-assembled identifiers are
        // undecidable at the token level; refusing them would break
        // ordinary variable use.
        for known_bypass in [
            vec![
                "bash",
                "-c",
                "V=; unset TOLE_AGE${V}NT_DEPTH; p=to; \"$p\"le run",
            ],
            vec![
                "bash",
                "-c",
                "e=e; f=xec; p=/usr/local/bin/to; $e$f -c \"$p\"le run mission",
            ],
        ] {
            let owned: Vec<String> = known_bypass.iter().map(|s| s.to_string()).collect();
            assert!(
                check_child_agent_argv(&owned).is_ok(),
                "documented token-level bypass changed behavior — update the guard: {known_bypass:?}"
            );
        }
    }

    /// Issue #228 regression (scan #123): quoting/substitution/`env -i`
    /// bypasses are refused. The concrete exploit from the scan —
    /// `env -i /bin/bash -c 'p=/usr/local/bin/to; exec "$p"le run
    /// mission'` — must NOT pass the guard.
    #[test]
    fn child_argv_refuses_quoting_substitution_and_env_clear() {
        for argv in [
            // `env -i` wipes the depth marker entirely.
            vec!["env", "-i", "/bin/bash", "-c", "tole run mission"],
            vec!["env", "--ignore-environment", "tole", "run"],
            // GNU abbreviation + mixed short cluster (cora round 5).
            vec!["env", "--ignore-env", "bash", "-c", "tole run"],
            vec!["env", "-vi", "tole", "run"],
            // Substitution-built env keyword (CI cora round on #245):
            // `$(printf e)$(printf nv)` expands to `env`; any_subst
            // arms the env context so the `-i` is refused.
            vec![
                "bash",
                "-c",
                "p=/usr/local/bin/to; $(printf e)$(printf nv) -i \"$p\"le run mission",
            ],
            // Quote-split depth marker (CI cora round 2 on #245) —
            // statically decodable: stripping quotes yields the literal
            // marker. (The `${V}` variant is NOT statically decodable —
            // documented in the guard boundary below.)
            vec![
                "bash",
                "-c",
                "unset TOLE_AGENT_DEPT\"\"H; p=/usr/local/bin/to; \"$p\"le run mission",
            ],
            // Assembled env keyword with abbreviated long flag (CI cora
            // round 5 on #245): `--i` = `--ignore-environment`.
            vec![
                "bash",
                "-c",
                "p=to; $(printf e)$(printf nv) --i \"$p\"le run mission",
            ],
            // Path-qualified / quoted env (cora review round 1 on #228).
            vec!["/usr/bin/env", "-i", "tole", "run"],
            vec!["\"env\"", "-i", "tole", "run"],
            vec!["env", "-iu", "TOLE_AGENT_DEPTH", "tole", "run"],
            // env-clear hidden INSIDE a -c script string.
            vec!["bash", "-c", "env -i tole run mission"],
            vec!["sh", "-c", "env --ignore-environment tole run"],
            // Variable indirection, 2-hop (`f=-i` then `$e $f`) —
            // cora review round 6 on #228: context is never cleared
            // mid-argv, so the smuggled flag is still caught.
            vec!["bash", "-c", "e=env; f=-i; $e $f bash -c 'tole run'"],
            // Quoting / adjacent punctuation around the binary name.
            vec!["bash", "-c", "exec \"tole\" run"],
            vec!["sh", "-c", "tole) run"],
            vec!["bash", "-c", "$(... command -v tole) run mission"],
            // Assignment-smuggled binary: `t=tole` is caught directly
            // ('=' splits the assignment).
            vec!["bash", "-c", "t=tole; exec $t run"],
            // Cross-element `env -i` (cora review round 4, CRITICAL:
            // the per-element reset skipped the flag in separate argv
            // elements) — the scan's own exploit shape, no literal
            // `tole` token anywhere.
            vec![
                "env",
                "-i",
                "/bin/bash",
                "-c",
                "p=/usr/local/bin/to; exec \"$p\"le run mission",
            ],
            // Benign flag reuse must NOT be refused (round 4 #2) —
            // asserted ok below, after the refusal loop.
            // `exec -c` = empty environment (bash/ksh) — the second
            // wipe path (cora review round 2 on #228).
            vec![
                "bash",
                "-c",
                "p=/usr/local/bin/to; exec -c \"$p\"le run mission",
            ],
            vec!["bash", "-c", "exec -c tole run"],
            // NOTE (issue #228): constructed-name concatenation like
            // `p=/usr/local/bin/to; exec "$p"le run` is undecidable at
            // the argv-token level, but it is NOT a bypass anymore: it
            // inherits TOLE_AGENT_DEPTH=1 and every environment-clearing
            // escape (`env -i/-u/--ignore-environment`, `exec -c`) is
            // refused above, so the spawned tole starts childed — agent
            // tools structurally absent.
            // Backslash separator trick.
            vec!["bash", "-c", "\\tole run"],
            // Substitution-built env keyword (cora CI round on #245 —
            // REVERTED later the same PR: `$(echo e)$(echo nv)` needs
            // `saw_env` to catch the following `-i`, and it does: `env`
            // or not, `-i` after a substitution-adjacent segment in env
            // context is refused via raw_subst. The any_subst widening
            // itself was removed as over-broad (blocked `echo $HOME`).
            // Substitution-built flag (cora review round 7 on #228):
            // `env -$(echo i)` constructs the wipe flag at runtime.
            vec![
                "bash",
                "-c",
                "env -$(echo i) bash -c 'p=/usr/local/bin/to; exec \"$p\"le run'",
            ],
        ] {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(
                check_child_agent_argv(&owned).is_err(),
                "must refuse: {argv:?}"
            );
        }
    }

    /// CodeCora PR #174 round 1: a leading-dash prompt must reach the
    /// child as the POSITIONAL prompt behind `--`, never as flags.
    #[test]
    fn leading_dash_prompt_is_guarded_by_double_dash() {
        let d = tmp("dashguard");
        #[cfg(unix)]
        {
            let p = d.join("dump-tole");
            let dump = d.join("argv.txt");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nexit 0\n",
                dump.display()
            );
            std::fs::write(&p, script).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
            let start = AgentStartTool::new(&p, d.clone());
            let out = start
                .execute(json!({"prompt": "--yes sneaky mission"}))
                .unwrap();
            // Poll for the dump: under parallel test load the detached
            // child can take longer than a fixed sleep.
            let mut dumped = None;
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if let Ok(t) = std::fs::read_to_string(&dump) {
                    dumped = Some(t);
                    break;
                }
            }
            let dumped = dumped
                .unwrap_or_else(|| panic!("child never wrote the argv dump at {}", dump.display()));
            let argv: Vec<&str> = dumped.lines().collect();
            let id = out["agent"].as_str().unwrap();
            let _ = id;
            // the exact sequence: ... "--" "--yes sneaky mission"
            let pos = argv.iter().position(|a| *a == "--");
            assert!(pos.is_some(), "argv must contain --: {argv:?}");
            let prompt_pos = pos.unwrap() + 1;
            assert_eq!(
                argv[prompt_pos], "--yes sneaky mission",
                "prompt must be the positional after --: {argv:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn depth_one_refuses_to_spawn() {
        let d = tmp("depth");
        let t = AgentStartTool::new(fake_child(&d), d.clone()).at_depth(1);
        let err = t
            .execute(json!({"prompt": "grandchild attempt"}))
            .unwrap_err();
        assert!(err.contains("capped at one level"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn spawn_poll_settle_roundtrip() {
        let d = tmp("roundtrip");
        let start = AgentStartTool::new(fake_child(&d), d.clone());
        let poll = AgentPollTool::new(d.clone());
        let out = start.execute(json!({"prompt": "do the thing"})).unwrap();
        let id = out["agent"].as_str().unwrap().to_string();
        assert!(valid_agent_id(&id));
        assert!(out["mailbox"].as_str().unwrap().starts_with("agent-"));
        // Poll until settled (parallel test load makes fixed sleeps
        // flaky — the child is a detached shell echo).
        let mut st = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let r = poll.execute(json!({"agent": id})).unwrap();
            if r["running"] == json!(false) {
                st = Some(r);
                break;
            }
        }
        let st = st.unwrap_or_else(|| panic!("fake child never settled"));
        assert_eq!(st["running"], json!(false), "{st}");
        assert!(
            st["log_tail"].as_str().unwrap().contains("FAKE_AGENT_DONE"),
            "{st}"
        );
        // The agent dir (session home + log) stays for audit.
        assert!(agents_root(&d).join(&id).join("log").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn concurrency_cap_enforced() {
        let d = tmp("cap");
        // Cap 1 + a child that sleeps: the second spawn must refuse.
        let start = AgentStartTool::new(sleeper_child(&d, 3), d.clone());
        let start = AgentStartTool {
            max_concurrent: 1,
            ..start
        };
        let _first = start.execute(json!({"prompt": "slow"})).unwrap();
        let err = start.execute(json!({"prompt": "second"})).unwrap_err();
        assert!(err.contains("cap 1"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Issue #234 regression: the count→register window is serialized
    /// by the flock. Two concurrent parents at cap 1 must yield exactly
    /// ONE success — the old count-then-spawn race let both through.
    #[cfg(unix)]
    #[test]
    fn concurrent_starts_cannot_exceed_the_cap() {
        let d = tmp("cap-race");
        let start = AgentStartTool::new(sleeper_child(&d, 3), d.clone());
        let start = AgentStartTool {
            max_concurrent: 1,
            ..start
        };
        let s1 = AgentStartTool {
            root: d.clone(),
            bin: start.bin.clone(),
            depth: 0,
            worktree_mode: false,
            max_concurrent: 1,
            timeout_secs: start.timeout_secs,
            parent_allows: Vec::new(),
        };
        let (r1, r2) = std::thread::scope(|s| {
            let h1 = s.spawn(|| start.execute(json!({"prompt": "a"})));
            let h2 = s.spawn(|| s1.execute(json!({"prompt": "b"})));
            (h1.join().unwrap(), h2.join().unwrap())
        });
        let oks = [r1.is_ok(), r2.is_ok()].iter().filter(|x| **x).count();
        assert_eq!(
            oks, 1,
            "exactly one spawn may win the cap race: r1={r1:?} r2={r2:?}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Issue #234 regression: the consume-once flag is PERSISTED — a
    /// NEW poll tool instance (fresh process equivalent) must see the
    /// mailbox as consumed and not resurrect the summary.
    #[test]
    fn consumed_flag_survives_a_new_poll_instance() {
        let d = tmp("consumed-persist");
        let bin = fake_child(&d);
        let start = AgentStartTool::new(&bin, d.clone());
        let out = start.execute(json!({"prompt": "quick"})).unwrap();
        let id = out["agent"].as_str().unwrap().to_string();
        // Wait for the fake child to settle.
        let poll = AgentPollTool::new(d.clone());
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let st = poll.execute(json!({"agent": id})).unwrap();
            if st["running"] == json!(false) {
                break;
            }
        }
        // First consume after settle (recalls + cleans + marks). Stubbed
        // recall: success with an explicit empty result (issue #287 —
        // a real `uteke` may be absent or failing in the test env).
        let dir = agents_root(&d).join(&id);
        let mut meta = read_meta(&dir).unwrap();
        let mut out = json!({});
        poll.consume_mailbox(&dir, &mut meta, &mut out, |_, _| Ok(Vec::new()));
        // A brand-new instance = a new parent process.
        let poll2 = AgentPollTool::new(d.clone());
        let again = poll2.execute(json!({"agent": id})).unwrap();
        assert!(
            again.get("summary").is_none(),
            "a fresh poll instance must see the mailbox consumed: {again}"
        );
        // And the flag is on disk.
        let meta: AgentMeta = serde_json::from_str(
            &std::fs::read_to_string(agents_root(&d).join(&id).join("meta.json")).unwrap(),
        )
        .unwrap();
        assert!(meta.mailbox_consumed);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Issue #300: a settled poll forgets the mailbox and persists
    /// `mailbox_consumed`, so it must NOT be classified ReadOnly (the tier
    /// the gate and plan mode key on). Pins both tools' tier.
    #[test]
    fn agent_tools_are_write_tier_because_poll_mutates() {
        let d = tmp("risk-tier");
        assert_eq!(AgentPollTool::new(d.clone()).risk(), Risk::Write);
        assert_eq!(
            AgentStartTool::new("/bin/true", d.clone()).risk(),
            Risk::Write
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Issue #287 regression: a failed recall must NOT mark the mailbox
    /// consumed (nor clean it) — the next poll retries and delivers.
    #[test]
    fn recall_error_keeps_mailbox_unconsumed_and_retries() {
        let d = tmp("recall-err");
        let bin = fake_child(&d);
        let start = AgentStartTool::new(&bin, d.clone());
        let out = start.execute(json!({"prompt": "quick"})).unwrap();
        let id = out["agent"].as_str().unwrap().to_string();
        let dir = agents_root(&d).join(&id);
        let poll = AgentPollTool::new(d.clone());
        let mut meta = read_meta(&dir).unwrap();
        let mut out = json!({});
        poll.consume_mailbox(&dir, &mut meta, &mut out, |_, _| {
            Err("uteke unavailable".into())
        });
        assert!(!meta.mailbox_consumed, "error must not mark consumed");
        assert!(out.get("summary").is_none());
        assert!(out["recall_error"].is_string(), "{out}");
        assert!(
            !read_meta(&dir).unwrap().mailbox_consumed,
            "error must not persist the consumed flag"
        );
        // Retry succeeds: summary delivered, then consumed.
        let mut out2 = json!({});
        poll.consume_mailbox(&dir, &mut meta, &mut out2, |_, _| {
            Ok(vec![crate::memory::RecallHit {
                score: 1.0,
                content: "child result".into(),
            }])
        });
        assert_eq!(out2["summary"], json!("child result"));
        assert!(meta.mailbox_consumed);
        assert!(read_meta(&dir).unwrap().mailbox_consumed);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn worktree_mode_creates_branch_and_jail() {
        let d = tmp("worktree");
        // a real (tiny) git repo — worktree add needs it
        let _ = crate::subprocess::run_with_timeout(
            Command::new("git").arg("init").arg("-q").current_dir(&d),
            crate::subprocess::SUBPROCESS_TIMEOUT,
        );
        let start = AgentStartTool::new(fake_child(&d), d.clone()).with_worktrees(true);
        let out = start
            .execute(json!({"prompt": "isolated mission"}))
            .unwrap();
        let branch = out["branch"].as_str().unwrap().to_string();
        assert!(branch.starts_with("agent/"), "{out}");
        let wt = PathBuf::from(out["worktree"].as_str().unwrap());
        assert!(wt.is_dir(), "worktree dir must exist");
        // the child's jail + cwd WAS the worktree: verify via git itself
        let listed = crate::subprocess::run_with_timeout(
            Command::new("git")
                .args(["worktree", "list"])
                .current_dir(&d),
            crate::subprocess::SUBPROCESS_TIMEOUT,
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&listed.stdout).contains(&branch));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// CodeCora PR #174 round 2: the child's allowlist can never exceed
    /// the parent operator's approved scope — requested patterns must
    /// be glob-covered; the default IS the parent's list.
    #[test]
    fn child_allowlist_is_subset_of_parent_scope() {
        let d = tmp("subset");
        let start = AgentStartTool::new(fake_child(&d), d.clone())
            .with_parent_allows(vec!["write_*".to_string(), "git".to_string()]);
        // wider-than-parent → refused
        let err = start
            .execute(json!({"prompt": "x", "allow": ["*"]}))
            .unwrap_err();
        assert!(err.contains("exceed the parent"), "{err}");
        let err = start
            .execute(json!({"prompt": "x", "allow": ["run_command"]}))
            .unwrap_err();
        assert!(err.contains("exceed the parent"), "{err}");
        // covered → accepted
        for req in [vec!["write_file"], vec!["write_edit", "git"]] {
            let r: Vec<String> = req.iter().map(|s| s.to_string()).collect();
            match start.execute(json!({"prompt": "x", "allow": r})) {
                Ok(_) => {}
                Err(e) => panic!("covered pattern {req:?} rejected: {e}"),
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_inputs_refused() {
        let d = tmp("inputs");
        let t = AgentStartTool::new(fake_child(&d), d.clone());
        assert!(t.execute(json!({})).is_err());
        assert!(t.execute(json!({"prompt": "  "})).is_err());
        assert!(
            t.execute(json!({"prompt": "x", "allow": ["-bad"]}))
                .is_err(),
            "leading-dash allow pattern must be refused"
        );
        let poll = AgentPollTool::new(d.clone());
        assert!(poll.execute(json!({"agent": "nope"})).is_err());
        assert!(poll.execute(json!({"agent": "../escape"})).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
