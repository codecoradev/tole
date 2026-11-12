//! Review gate before `git commit` (#534): when the cora agent-as-reviewer
//! tools are attached, a commit is refused until the STAGED diff has been
//! reviewed.
//!
//! tole does not judge, hash or record anything itself. cora >= 0.20.0
//! already owns the record: `mcp_cora_cora_review_finalize` leaves a marker
//! bound to the exact change set of the staged diff (cora's commit gate,
//! cora #674), and `cora hook verify` answers "may this staged diff be
//! committed?" with an exit code (0 validated or nothing to review, 10
//! validated but the verdict blocks, 11 not validated). The gate asks that
//! question before the commit runs, so tole and cora cannot disagree about
//! what "this diff" is.
//!
//! What it guards, and what it does not:
//! - the `git` tool's `commit` op, and `run_command` / `job_start` whose
//!   argv is `git commit` (behind `env`, `VAR=x` and git's global options),
//!   because the dedicated tool alone would leave `run_command` as the way
//!   around it;
//! - it is a process guardrail for a cooperating agent, NOT a security
//!   boundary (the approver is): `sh -c "git commit"`, a script, a git alias
//!   or `merge` / `cherry-pick` / `am` create commits it does not see, and
//!   the review is judged by the same model that wrote the change.
//! - it fails OPEN on tooling trouble (cora too old for `hook verify`, not
//!   runnable, timing out): one warning, commits are not gated.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tole_core::tool::{Risk, Tool};

/// Name of the env var that picks the policy (`--review-gate` sets it).
pub const ENV: &str = "TOLE_REVIEW_GATE";

/// How long `cora hook verify` may take. It only reads the index and one
/// small marker file.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);

/// What the gate does with a commit whose staged diff is not validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Policy {
    /// Never gate.
    Off,
    /// Let the commit through, with a note in the tool result.
    Warn,
    /// Refuse the commit (the default).
    Block,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Warn => "warn",
            Policy::Block => "block",
        }
    }

    /// `TOLE_REVIEW_GATE`: unset or empty = the default (`block`); anything
    /// that is not `off|warn|block` is an error, never a silent default.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Result<Policy, String> {
        match env(ENV)
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .as_deref()
        {
            None => Ok(Policy::Block),
            Some("off") => Ok(Policy::Off),
            Some("warn") => Ok(Policy::Warn),
            Some("block") => Ok(Policy::Block),
            Some(other) => Err(format!(
                "{ENV}={other:?} is not valid: use off, warn or block"
            )),
        }
    }
}

/// What `cora hook verify` said.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Exit 0: validated, or nothing reviewable is staged.
    Validated,
    /// Exit 10: reviewed, and the verdict blocks the commit.
    Blocking,
    /// Exit 11: no usable review of this staged diff.
    NotValidated,
    /// Anything else: cora is too old (a usage error), missing, or broken.
    Unavailable(String),
}

/// The gate's decision for one commit attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Proceed,
    /// Proceed, with this note for the tool result (`warn`, or a one-time
    /// "gate unavailable").
    Note(String),
    Refuse(String),
}

/// Shared by the wrapped tools of one session. Inactive until the review
/// tools are known to be registered ([`Gate::activate`]): a gate with no way
/// to get a review would only lock the agent out.
pub struct Gate {
    policy: Policy,
    workdir: PathBuf,
    cora_bin: PathBuf,
    active: AtomicBool,
    warned_unavailable: AtomicBool,
}

impl Gate {
    pub fn new(policy: Policy, workdir: PathBuf) -> Arc<Gate> {
        Arc::new(Gate {
            policy,
            workdir,
            cora_bin: PathBuf::from("cora"),
            active: AtomicBool::new(false),
            warned_unavailable: AtomicBool::new(false),
        })
    }

    /// Tests point the gate at a fake cora.
    pub fn with_cora_bin(policy: Policy, workdir: PathBuf, bin: PathBuf) -> Arc<Gate> {
        Arc::new(Gate {
            policy,
            workdir,
            cora_bin: bin,
            active: AtomicBool::new(false),
            warned_unavailable: AtomicBool::new(false),
        })
    }

    /// Start gating: the review tools are registered.
    pub fn activate(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Ask cora about the repository at `dir` (cora reads the staged diff of
    /// the repo it is run in, so `dir` must be where the commit will happen).
    fn verify(&self, dir: &Path) -> Outcome {
        let mut cmd = Command::new(&self.cora_bin);
        cmd.args(["hook", "verify", "--quiet"]).current_dir(dir);
        tole_core::subprocess::scrub_env_for(&mut cmd, "cora");
        match tole_core::subprocess::run_with_timeout(&mut cmd, VERIFY_TIMEOUT) {
            Ok(out) => match out.status.code() {
                Some(0) => Outcome::Validated,
                Some(10) => Outcome::Blocking,
                Some(11) => Outcome::NotValidated,
                Some(2) => Outcome::Unavailable(
                    "this cora has no `hook verify` (needs cora >= 0.20.0)".into(),
                ),
                other => Outcome::Unavailable(format!("`cora hook verify` exited {other:?}")),
            },
            Err(e) => Outcome::Unavailable(e),
        }
    }

    /// Decide for one `git commit` attempt in the session's repository.
    pub fn check_commit(&self) -> Decision {
        self.check_commit_in(&self.workdir)
    }

    /// Where a commit with these `git -C` arguments happens: each `-C` is
    /// relative to the previous one, the first to the session directory.
    pub fn commit_dir(&self, c_paths: &[String]) -> PathBuf {
        c_paths
            .iter()
            .fold(self.workdir.clone(), |base, p| base.join(p))
    }

    /// A commit this gate cannot verify (it redirects git to another git
    /// dir or work tree): refused under `block`, noted under `warn`.
    pub fn unverifiable(&self, why: &str) -> Decision {
        if !self.is_active() || self.policy == Policy::Off {
            return Decision::Proceed;
        }
        let msg = format!(
            "git commit refused by the review gate: {why}, so the review of the staged diff cannot be checked. Run the commit in the workspace repository (git -C <dir> is fine) without redirecting git's git dir or work tree."
        );
        match self.policy {
            Policy::Block => {
                Decision::Refuse(format!("{msg} (operator: {ENV}=warn or off changes this)"))
            }
            _ => Decision::Note(format!(
                "review gate (warn): {}",
                msg.replacen("refused by the review gate: ", "would be refused: ", 1)
            )),
        }
    }

    /// Like [`Gate::check_commit`] for the repository at `dir`.
    pub fn check_commit_in(&self, dir: &Path) -> Decision {
        if !self.is_active() || self.policy == Policy::Off {
            return Decision::Proceed;
        }
        let msg = match self.verify(dir) {
            Outcome::Validated => return Decision::Proceed,
            Outcome::Unavailable(why) => {
                if self.warned_unavailable.swap(true, Ordering::SeqCst) {
                    return Decision::Proceed;
                }
                let note = format!("review gate unavailable, commits are not gated: {why}");
                eprintln!("tole: {note}");
                return Decision::Note(note);
            }
            Outcome::Blocking => "git commit refused by the review gate: the review verdict for \
                the staged diff BLOCKS the commit. Fix the findings, re-stage, review the new \
                diff again (mcp_cora_cora_review_prepare, then mcp_cora_cora_review_finalize \
                with the SAME diff), and retry."
                .to_string(),
            Outcome::NotValidated => "git commit refused by the review gate: the staged diff \
                has not been reviewed. Get it (git tool: op diff, staged true), call \
                mcp_cora_cora_review_prepare with it, judge it yourself, call \
                mcp_cora_cora_review_finalize with the SAME diff and your findings, then retry \
                the commit. Staging more changes after the review makes it stale."
                .to_string(),
        };
        match self.policy {
            Policy::Block => {
                Decision::Refuse(format!("{msg} (operator: {ENV}=warn or off changes this)"))
            }
            _ => Decision::Note(format!(
                "review gate (warn): {}",
                msg.replacen("refused by the review gate: ", "would be refused: ", 1)
            )),
        }
    }
}

/// What `git commit` an argv is, when it is one.
#[derive(Debug, PartialEq, Eq)]
pub enum CommitCall {
    /// A commit in the directory reached through these `git -C` arguments
    /// (none = the session directory).
    In(Vec<String>),
    /// A commit that redirects git's git dir or work tree (`--git-dir`,
    /// `--work-tree`, `GIT_DIR=`, `-c core.worktree=`, ...): the staged diff
    /// cora would look at is not necessarily the one git commits.
    Redirected(String),
}

/// Environment variables that point git somewhere other than the directory
/// it runs in.
const GIT_REDIRECT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// Classify `argv`: `None` unless it runs `git commit` (through `env` and
/// `VAR=value` prefixes, a path to git, and git's global options). Aliases
/// cannot be resolved and are not seen.
pub fn git_commit_call(argv: &[String]) -> Option<CommitCall> {
    let mut redirect: Option<String> = None;
    let mut note = |name: &str| {
        redirect.get_or_insert_with(|| format!("it sets {name}"));
    };
    let mut i = 0;
    // `env [-i] [-u NAME] [VAR=val ...] cmd` and bare `VAR=val cmd`.
    while let Some(tok) = argv.get(i) {
        let base = tok.rsplit('/').next().unwrap_or(tok);
        if base == "env" {
            i += 1;
            while let Some(t) = argv.get(i) {
                if t == "-C" || t == "--chdir" || t == "-S" {
                    // The commit would land in a directory we cannot resolve.
                    note("env -C");
                    i += 2;
                } else if t.starts_with("-C") || t.starts_with("--chdir=") || t.starts_with("-S") {
                    note("env -C");
                    i += 1;
                } else if t == "-u" || t == "--unset" {
                    i += 2;
                } else if t.starts_with('-') {
                    i += 1;
                } else if let Some((name, _)) = t.split_once('=') {
                    if GIT_REDIRECT_ENV.contains(&name) {
                        note(name);
                    }
                    i += 1;
                } else {
                    break;
                }
            }
        } else if tok.contains('=') && !tok.starts_with('-') && !tok.starts_with('/') {
            let name = tok.split('=').next().unwrap_or_default();
            if GIT_REDIRECT_ENV.contains(&name) {
                note(name);
            }
            i += 1;
        } else {
            break;
        }
    }
    let prog = argv.get(i)?;
    if prog.rsplit('/').next() != Some("git") {
        return None;
    }
    i += 1;
    let mut c_paths = Vec::new();
    while let Some(tok) = argv.get(i) {
        if !tok.starts_with('-') {
            if tok != "commit" {
                return None;
            }
            return Some(match redirect {
                Some(why) => CommitCall::Redirected(why),
                None => CommitCall::In(c_paths),
            });
        }
        match tok.as_str() {
            "-C" => {
                if let Some(p) = argv.get(i + 1) {
                    c_paths.push(p.clone());
                }
                i += 2;
            }
            "-c" | "--config-env" => {
                // Config can move the work tree or git dir.
                let kv = argv.get(i + 1).map(String::as_str).unwrap_or_default();
                let key = kv
                    .split('=')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if key.starts_with("core.worktree") || key.starts_with("core.gitdir") {
                    note(&format!("-c {key}"));
                }
                i += 2;
            }
            "--git-dir" | "--work-tree" => {
                note(tok);
                i += 2;
            }
            t if t.starts_with("--git-dir=") || t.starts_with("--work-tree=") => {
                note(t.split('=').next().unwrap_or(t));
                i += 1;
            }
            "--namespace" | "--super-prefix" | "--exec-path" => i += 2,
            t if t.starts_with("-C") && t.len() > 2 => {
                c_paths.push(t[2..].to_string());
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// True when `argv` runs `git commit`.
pub fn is_git_commit(argv: &[String]) -> bool {
    git_commit_call(argv).is_some()
}

/// Which input shape a wrapped tool takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The `git` tool: `{"op": "commit", ...}`.
    Git,
    /// `run_command` / `job_start`: `{"command": "git commit -m x"}`.
    Command,
}

/// Tool decorator: unchanged except that a `git commit` is asked of the
/// [`Gate`] first. Everything else passes through untouched.
pub struct Gated {
    inner: Box<dyn Tool>,
    gate: Arc<Gate>,
    kind: Kind,
}

impl Gated {
    pub fn new(inner: Box<dyn Tool>, gate: Arc<Gate>, kind: Kind) -> Self {
        Self { inner, gate, kind }
    }

    /// `None` = not a commit; `Some(decision)` = what the gate says about it.
    fn decide(&self, input: &Value) -> Option<Decision> {
        match self.kind {
            Kind::Git => (input.get("op").and_then(Value::as_str) == Some("commit"))
                .then(|| self.gate.check_commit()),
            Kind::Command => {
                let argv = input
                    .get("command")
                    .and_then(Value::as_str)
                    .and_then(|line| {
                        tole_core::run_command::RunCommandTool::split_argv(line).ok()
                    })?;
                Some(match git_commit_call(&argv)? {
                    CommitCall::In(c) => self.gate.check_commit_in(&self.gate.commit_dir(&c)),
                    CommitCall::Redirected(why) => self.gate.unverifiable(&why),
                })
            }
        }
    }
}

impl Tool for Gated {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn risk(&self) -> Risk {
        self.inner.risk()
    }
    fn is_poll(&self) -> bool {
        self.inner.is_poll()
    }
    fn describe(&self, input: &Value) -> String {
        self.inner.describe(input)
    }
    fn approval_target(&self, input: &Value) -> Option<String> {
        self.inner.approval_target(input)
    }
    fn summary(&self) -> String {
        self.inner.summary()
    }
    fn spec(&self) -> Option<Value> {
        self.inner.spec()
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let Some(decision) = self.decide(&input) else {
            return self.inner.execute(input);
        };
        match decision {
            Decision::Refuse(msg) => Err(msg),
            Decision::Proceed => self.inner.execute(input),
            Decision::Note(note) => {
                let mut out = self.inner.execute(input)?;
                match out.as_object_mut() {
                    Some(map) => {
                        map.insert("review_gate".into(), json!(note));
                    }
                    None => out = json!({ "result": out, "review_gate": note }),
                }
                Ok(out)
            }
        }
    }
}

/// `tole doctor`'s one line about the gate.
pub fn doctor_line(env: &dyn Fn(&str) -> Option<String>, workdir: &Path) -> String {
    let policy = match Policy::from_env(env) {
        Ok(p) => p,
        Err(e) => return format!("  ✗ review gate: {e}"),
    };
    if policy == Policy::Off {
        return format!("  - review gate: off ({ENV}=off)");
    }
    let mut cmd = Command::new("cora");
    cmd.args(["hook", "verify", "--help"]).current_dir(workdir);
    match tole_core::subprocess::run_with_timeout(&mut cmd, Duration::from_secs(5)) {
        Ok(o) if o.status.success() => format!(
            "  ✓ review gate: {} (git commit needs a reviewed staged diff while the cora review tools are attached)",
            policy.as_str()
        ),
        Ok(_) => format!(
            "  - review gate: {} but this cora has no `hook verify` (needs cora >= 0.20.0): commits are not gated",
            policy.as_str()
        ),
        Err(_) => format!(
            "  - review gate: {} but cora is not installed: commits are not gated",
            policy.as_str()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn recognizes_git_commit_in_its_common_spellings() {
        for yes in [
            "git commit -m x",
            "git commit",
            "/usr/bin/git commit -m x",
            "git -C sub commit -m x",
            "git -c user.name=a -c user.email=b commit -m x",
            "git --git-dir=/r/.git --work-tree=/r commit",
            "git --git-dir /r/.git commit",
            "git --no-pager commit",
            "env GIT_AUTHOR_NAME=a git commit -m x",
            "env -i git commit",
            "env -u HOME git commit",
            "GIT_AUTHOR_NAME=a git commit -m x",
            "git commit --no-verify -m x",
        ] {
            assert!(is_git_commit(&argv(yes)), "{yes}");
        }
    }

    #[test]
    fn does_not_mistake_other_commands_for_a_commit() {
        for no in [
            "git status",
            "git log --oneline commit",
            "git add commit",
            "git diff --staged",
            "git -C commit status",
            "echo git commit",
            "cargo commit",
            "git",
            "",
            "env",
        ] {
            assert!(!is_git_commit(&argv(no)), "{no}");
        }
    }

    // ---- the gate itself, against a fake `cora hook verify` ---------------

    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;

    struct Fake {
        dir: PathBuf,
        bin: PathBuf,
    }

    /// A fake cora whose `hook verify` exits with whatever the `exit` file
    /// says and counts its invocations in `calls`.
    fn fake_cora(tag: &str) -> Fake {
        let dir = std::env::temp_dir().join(format!(
            "tole-gate-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("cora");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nd={}\necho x >> \"$d/calls\"\npwd >> \"$d/cwds\"\n[ \"$1 $2\" = \"hook verify\" ] && exit $(cat \"$d/exit\")\nexit 99\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let f = Fake { dir, bin };
        f.exit(0);
        f
    }

    impl Fake {
        fn exit(&self, code: i32) {
            std::fs::write(self.dir.join("exit"), code.to_string()).unwrap();
        }
        /// The directories `hook verify` was run in, in order.
        fn cwds(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("cwds"))
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default()
        }
        fn calls(&self) -> usize {
            std::fs::read_to_string(self.dir.join("calls"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }
        fn gate(&self, policy: Policy) -> Arc<Gate> {
            let g = Gate::with_cora_bin(policy, self.dir.clone(), self.bin.clone());
            g.activate();
            g
        }
    }

    #[test]
    fn an_inactive_or_off_gate_never_asks_cora() {
        let f = fake_cora("inactive");
        f.exit(11);
        let inactive = Gate::with_cora_bin(Policy::Block, f.dir.clone(), f.bin.clone());
        assert_eq!(inactive.check_commit(), Decision::Proceed);
        assert_eq!(f.gate(Policy::Off).check_commit(), Decision::Proceed);
        assert_eq!(
            f.calls(),
            0,
            "no review tools or policy off: cora is not run"
        );
    }

    #[test]
    fn block_refuses_an_unreviewed_or_blocked_diff_and_passes_a_validated_one() {
        let f = fake_cora("block");
        let g = f.gate(Policy::Block);
        f.exit(11);
        match g.check_commit() {
            Decision::Refuse(m) => {
                assert!(m.contains("has not been reviewed"), "{m}");
                assert!(m.contains("mcp_cora_cora_review_finalize"), "{m}");
                assert!(
                    m.contains(ENV),
                    "the operator can see how to change it: {m}"
                );
            }
            other => panic!("{other:?}"),
        }
        f.exit(10);
        match g.check_commit() {
            Decision::Refuse(m) => assert!(m.contains("BLOCKS"), "{m}"),
            other => panic!("{other:?}"),
        }
        f.exit(0);
        assert_eq!(g.check_commit(), Decision::Proceed);
    }

    #[test]
    fn warn_lets_the_commit_through_with_a_note() {
        let f = fake_cora("warn");
        f.exit(11);
        match f.gate(Policy::Warn).check_commit() {
            Decision::Note(n) => assert!(n.contains("would be refused"), "{n}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tooling_trouble_fails_open_with_one_note() {
        // An old cora: `hook verify` is a usage error (exit 2).
        let f = fake_cora("old");
        f.exit(2);
        let g = f.gate(Policy::Block);
        match g.check_commit() {
            Decision::Note(n) => assert!(n.contains("needs cora >= 0.20.0"), "{n}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            g.check_commit(),
            Decision::Proceed,
            "said once, not on every commit"
        );
        // A cora that is not there at all behaves the same.
        let gone = Gate::with_cora_bin(Policy::Block, f.dir.clone(), f.dir.join("nope"));
        gone.activate();
        assert!(matches!(gone.check_commit(), Decision::Note(_)));
    }

    struct Probe(Arc<AtomicUsize>);
    impl Tool for Probe {
        fn name(&self) -> &str {
            "probe"
        }
        fn risk(&self) -> Risk {
            Risk::Write
        }
        fn execute(&self, _input: Value) -> Result<Value, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "ok": true }))
        }
    }

    #[test]
    fn the_decorator_gates_only_commits_and_only_in_the_two_input_shapes() {
        let f = fake_cora("deco");
        f.exit(11);
        let runs = Arc::new(AtomicUsize::new(0));
        let git = Gated::new(
            Box::new(Probe(runs.clone())),
            f.gate(Policy::Block),
            Kind::Git,
        );
        let cmd = Gated::new(
            Box::new(Probe(runs.clone())),
            f.gate(Policy::Block),
            Kind::Command,
        );

        // Commits are refused before the inner tool runs.
        assert!(git
            .execute(json!({"op": "commit", "message": "x"}))
            .is_err());
        assert!(cmd.execute(json!({"command": "git commit -m x"})).is_err());
        assert!(cmd
            .execute(json!({"command": "git -C . commit --no-verify -m x"}))
            .is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        // Everything else passes straight through (and cora is not asked).
        let asked = f.calls();
        assert!(git.execute(json!({"op": "status"})).is_ok());
        assert!(git.execute(json!({"op": "diff", "staged": true})).is_ok());
        assert!(cmd.execute(json!({"command": "git status"})).is_ok());
        assert!(cmd.execute(json!({"command": "cargo test"})).is_ok());
        assert_eq!(runs.load(Ordering::SeqCst), 4);
        assert_eq!(f.calls(), asked);
        // A validated diff goes through.
        f.exit(0);
        assert!(git.execute(json!({"op": "commit", "message": "x"})).is_ok());
    }

    #[test]
    fn a_warning_is_attached_to_the_result_not_swallowed() {
        let f = fake_cora("note");
        f.exit(11);
        let runs = Arc::new(AtomicUsize::new(0));
        let git = Gated::new(
            Box::new(Probe(runs.clone())),
            f.gate(Policy::Warn),
            Kind::Git,
        );
        let out = git
            .execute(json!({"op": "commit", "message": "x"}))
            .unwrap();
        assert_eq!(out["ok"], json!(true));
        assert!(
            out["review_gate"]
                .as_str()
                .unwrap()
                .contains("would be refused"),
            "{out}"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// The real `git` tool in a real repository: an unreviewed commit does
    /// not happen, and the same commit goes through once cora validates it.
    #[test]
    fn the_real_git_tool_cannot_commit_an_unreviewed_staged_diff() {
        let f = fake_cora("realgit");
        let repo = f.dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let o = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        git(&["add", "a.txt"]);

        let g = Gate::with_cora_bin(Policy::Block, repo.clone(), f.bin.clone());
        g.activate();
        let tool = Gated::new(
            Box::new(tole_core::git::GitTool::new().in_dir(&repo)),
            g,
            Kind::Git,
        );
        f.exit(11);
        let err = tool
            .execute(json!({"op": "commit", "message": "add a"}))
            .unwrap_err();
        assert!(err.contains("has not been reviewed"), "{err}");
        let log = Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            !log.status.success() || log.stdout.is_empty(),
            "no commit was made"
        );

        f.exit(0);
        tool.execute(json!({"op": "commit", "message": "add a"}))
            .unwrap();
        assert_eq!(git(&["log", "--oneline"]).lines().count(), 1);
    }

    #[test]
    fn a_commit_call_knows_where_it_commits_and_when_git_is_redirected() {
        let c = |s: &str| git_commit_call(&argv(s));
        assert_eq!(c("git commit -m x"), Some(CommitCall::In(vec![])));
        assert_eq!(
            c("git -C sub commit"),
            Some(CommitCall::In(vec!["sub".into()]))
        );
        assert_eq!(
            c("git -C a -C b commit"),
            Some(CommitCall::In(vec!["a".into(), "b".into()]))
        );
        assert_eq!(
            c("env GIT_AUTHOR_NAME=a git commit"),
            Some(CommitCall::In(vec![]))
        );
        assert_eq!(c("git -c user.name=a commit"), Some(CommitCall::In(vec![])));
        for redirected in [
            "git --git-dir=/r/.git commit",
            "git --git-dir /r/.git commit",
            "git --work-tree=/r commit",
            "git --work-tree /r commit",
            "env GIT_DIR=/r/.git git commit",
            "GIT_WORK_TREE=/r git commit",
            "GIT_INDEX_FILE=/tmp/i git commit",
            "git -c core.worktree=/r commit",
            "git -C sub --git-dir=/r/.git commit",
        ] {
            assert!(
                matches!(c(redirected), Some(CommitCall::Redirected(_))),
                "{redirected}: {:?}",
                c(redirected)
            );
        }
        assert_eq!(c("git status"), None);
        assert_eq!(c("git -C sub status"), None);
    }

    /// `git -C sub commit` commits in `sub`: cora must be asked about THAT
    /// repository, not about the session one (a validated session repo must
    /// not unlock a commit into a nested repo).
    #[test]
    fn attached_c_and_env_chdir_forms_are_not_missed() {
        let v = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            git_commit_call(&v("git -Cnested commit -m x")),
            Some(CommitCall::In(vec!["nested".into()]))
        );
        for cmd in [
            "env -C nested git commit -m x",
            "env -Cnested git commit -m x",
            "env --chdir=nested git commit -m x",
            "env --chdir nested git commit -m x",
        ] {
            assert!(
                matches!(git_commit_call(&v(cmd)), Some(CommitCall::Redirected(_))),
                "{cmd}"
            );
        }
    }

    #[test]
    fn a_commit_in_another_directory_is_verified_there() {
        let f = fake_cora("cdir");
        std::fs::create_dir_all(f.dir.join("sub")).unwrap();
        let gate = f.gate(Policy::Block);
        let cmd = Gated::new(
            Box::new(Probe(Arc::new(AtomicUsize::new(0)))),
            gate,
            Kind::Command,
        );
        f.exit(0);
        assert!(cmd
            .execute(json!({"command": "git -C sub commit -m x"}))
            .is_ok());
        let cwds = f.cwds();
        assert!(cwds.last().unwrap().ends_with("/sub"), "{cwds:?}");
        // Same call, but that repository is not validated: refused.
        f.exit(11);
        assert!(cmd
            .execute(json!({"command": "git -C sub commit -m x"}))
            .is_err());
        assert!(f.cwds().last().unwrap().ends_with("/sub"));
        // Plain commit: the session directory.
        assert!(cmd.execute(json!({"command": "git commit -m x"})).is_err());
        assert!(!f.cwds().last().unwrap().ends_with("/sub"));
    }

    #[test]
    fn a_commit_that_redirects_git_cannot_be_verified_and_is_not_let_through() {
        let f = fake_cora("redir");
        f.exit(0); // even a "validated" answer for the session repo must not help
        let runs = Arc::new(AtomicUsize::new(0));
        let block = Gated::new(
            Box::new(Probe(runs.clone())),
            f.gate(Policy::Block),
            Kind::Command,
        );
        for line in [
            "git --git-dir=/elsewhere/.git commit -m x",
            "env GIT_DIR=/elsewhere/.git git commit -m x",
            "git -C sub --work-tree=/w commit -m x",
        ] {
            let e = block.execute(json!({ "command": line })).unwrap_err();
            assert!(e.contains("cannot be checked"), "{line}: {e}");
        }
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(f.calls(), 0, "cora is not even asked");
        // warn: through, with a note.
        let warn = Gated::new(
            Box::new(Probe(runs.clone())),
            f.gate(Policy::Warn),
            Kind::Command,
        );
        let out = warn
            .execute(json!({"command": "git --git-dir=/e/.git commit -m x"}))
            .unwrap();
        assert!(
            out["review_gate"]
                .as_str()
                .unwrap()
                .contains("would be refused"),
            "{out}"
        );
        // off: untouched.
        let off = Gated::new(
            Box::new(Probe(runs.clone())),
            f.gate(Policy::Off),
            Kind::Command,
        );
        assert!(off
            .execute(json!({"command": "git --git-dir=/e/.git commit -m x"}))
            .is_ok());
    }

    #[test]
    fn policy_from_env_defaults_to_block_and_refuses_junk() {
        let none = |_: &str| None;
        assert_eq!(Policy::from_env(&none), Ok(Policy::Block));
        let set = |v: &'static str| move |_: &str| Some(v.to_string());
        assert_eq!(Policy::from_env(&set("off")), Ok(Policy::Off));
        assert_eq!(Policy::from_env(&set(" WARN ")), Ok(Policy::Warn));
        assert_eq!(Policy::from_env(&set("")), Ok(Policy::Block));
        assert!(Policy::from_env(&set("maybe")).is_err());
    }
}
