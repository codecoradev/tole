//! #748: `run_command` and `job_start` under `[sandbox.consumers]` really run
//! inside bubblewrap with the `command` profile.
//!
//! Needs a working `bwrap`. Without one these tests print a `SKIPPED:` line
//! with the reason and return (`TOLE_REQUIRE_BWRAP=1` makes that a failure,
//! as the CI job that installs bubblewrap sets it). The consumer settings
//! are process-wide and set once here (both `required`); the policy half
//! that needs no bwrap is in `command_sandbox_none.rs`.

#![cfg(unix)]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tole_core::jobs::{JobPollTool, JobStartTool};
use tole_core::run_command::RunCommandTool;
use tole_core::sandbox::command::{self, Consumers, Limits};
use tole_core::sandbox::{self, Availability, Mode, Network};
use tole_core::secret_release::Release;
use tole_core::tool::Tool;

const CANARY: &str = "sandbox-canary-not-a-credential-0123456789";
const LIMITS: Limits = Limits {
    cpu_secs: 120,
    memory_mb: 4096,
    processes: 512,
    file_size_mb: 64,
};

struct Setup {
    base: PathBuf,
    /// The fake `$HOME` of this process, holding a canary file.
    home: PathBuf,
    /// A file inside the fake tole data dir.
    data_secret: PathBuf,
}

fn setup() -> &'static Setup {
    static S: OnceLock<Setup> = OnceLock::new();
    S.get_or_init(|| {
        // Outside `/tmp` (a tmpfs inside the sandbox) and outside any
        // system dir, so "absent" means the sandbox hid it.
        // A fixed directory per target dir (the Setup lives for the whole
        // process, so no per-test guard can own it): it is emptied here, so
        // it never grows from one run to the next.
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shared-cmdsbx");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let home = base.join("home");
        let data = home.join(".codecora/tole");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(home.join("canary"), "x").unwrap();
        std::fs::write(data.join("secret"), "x").unwrap();
        std::env::set_var("HOME", &home);
        std::env::remove_var("CODECORA_HOME");
        std::env::set_var("GITHUB_TOKEN", CANARY);
        std::env::set_var("OTHER_SANDBOX_TOKEN", CANARY);
        sandbox::command::set_consumers(Consumers {
            run_command: Mode::Required,
            job_start: Mode::Required,
            verify: Mode::Required,
            turnend: Mode::Off,
            cron: Mode::Off,
            network: Network::None,
            limits: LIMITS,
        })
        .unwrap();
        Setup {
            base,
            home,
            data_secret: data.join("secret"),
        }
    })
}

/// True (after printing the reason) when no sandbox works here.
fn skipped() -> bool {
    setup();
    match sandbox::detect() {
        Availability::Usable { .. } => false,
        Availability::Unavailable(why) => {
            assert!(
                std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
                "TOLE_REQUIRE_BWRAP is set but no sandbox works: {why}"
            );
            eprintln!("SKIPPED: no working sandbox here: {why}");
            true
        }
    }
}

/// A fresh workspace under the base dir, resolved.
fn ws(tag: &str) -> PathBuf {
    let d = setup().base.join(tag);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::canonicalize(&d).unwrap()
}

fn run(tool: &RunCommandTool, line: &str) -> Value {
    tool.execute(json!({ "command": line }))
        .unwrap_or_else(|e| panic!("{line}: {e}"))
}

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

/// `key=value` lines of a command's stdout.
fn kv(out: &Value, key: &str) -> String {
    text(out, "stdout")
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key}= in {out}"))
        .to_string()
}

const CHECK: &str = r#"#!/bin/sh
# $1 host HOME, $2 data-dir secret, $3 port, $4 host pid of tole
echo "home=$HOME"
echo "pwd=$(pwd)"
echo "data=$(test -e "$2" && echo yes || echo no)"
echo "hosthome=$(test -e "$1/canary" && echo yes || echo no)"
echo "net=$(python3 -c "import socket; s=socket.socket(); s.settimeout(2); s.connect(('127.0.0.1',$3))" 2>/dev/null && echo reached || echo blocked)"
echo "wsw=$( (echo x > made) 2>/dev/null && echo writable || echo readonly)"
echo "hooks=$( (echo x > .git/hooks/evil) 2>/dev/null && echo writable || echo readonly)"
echo "config=$( (echo x >> .git/config) 2>/dev/null && echo writable || echo readonly)"
echo "tole=$( (echo x > .tole/evil) 2>/dev/null && echo writable || echo readonly)"
echo "tolenew=$( (mkdir .tole/sub) 2>/dev/null && echo writable || echo readonly)"
echo "caps=$(grep CapEff /proc/self/status | tr -d '[:space:]')"
echo "nnp=$(grep NoNewPrivs /proc/self/status | tr -d '[:space:]')"
echo "session=$TOLE_UNDER_SESSION"
echo "tmp=$( (echo x > /tmp/sbx-leak-$4) 2>/dev/null && echo writable || echo readonly)"
echo "tolevisible=$(ls /proc | grep -cx "$4")"
echo "pid1canary=$(tr '\0' '\n' < /proc/1/environ 2>/dev/null | grep -c CANARY)"
echo "envcanary=$(env | grep -c CANARY)"
echo "granted=$(printenv GRANTED_SANDBOX_TOKEN)"
echo "limits=$(grep -E '^Max (cpu time|address space|processes|file size)' /proc/self/limits | tr -s ' ' | tr '\n' ';')"
echo "fds=$(ls /proc/self/fd | tr '\n' ' ')"
"#;

/// A workspace with a git dir, a `.tole` dir and the check script.
fn project(tag: &str) -> PathBuf {
    let w = ws(tag);
    for d in [".git/hooks", ".tole"] {
        std::fs::create_dir_all(w.join(d)).unwrap();
    }
    std::fs::write(w.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(w.join("check.sh"), CHECK).unwrap();
    w
}

#[test]
fn a_sandboxed_command_sees_the_command_profile_and_nothing_else() {
    if skipped() {
        return;
    }
    let s = setup();
    let w = project("view");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // A descriptor leaked into tole by its parent (not close-on-exec).
    let leak = std::fs::File::open("/etc/hostname").unwrap();
    // SAFETY: dup2 onto a number nothing else of this test process uses.
    assert_eq!(unsafe { libc::dup2(leak.as_raw_fd(), 199) }, 199);
    let plain = std::process::Command::new("ls")
        .arg("/proc/self/fd")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&plain.stdout)
            .lines()
            .any(|l| l == "199"),
        "sanity: a plain child inherits the leaked descriptor"
    );

    let mut tool = RunCommandTool::new(w.clone());
    tool.release =
        Some(Release::default().with("GRANTED_SANDBOX_TOKEN", ["run_command".to_string()]));
    std::env::set_var("GRANTED_SANDBOX_TOKEN", "granted-value-0123");
    let out = run(
        &tool,
        &format!(
            "sh check.sh {} {} {port} {}",
            s.home.display(),
            s.data_secret.display(),
            std::process::id()
        ),
    );
    unsafe { libc::close(199) };
    assert_eq!(out["status"], 0, "{out}");
    assert_eq!(kv(&out, "net"), "blocked", "no network");
    assert_eq!(kv(&out, "data"), "no", "the data dir is absent");
    assert_eq!(kv(&out, "hosthome"), "no", "the real $HOME is absent");
    assert_eq!(kv(&out, "home"), "/scratch", "a private home");
    assert_eq!(Path::new(&kv(&out, "pwd")), w, "cwd is the workspace");
    assert_eq!(kv(&out, "wsw"), "writable", "the workspace is writable");
    assert!(w.join("made").exists(), "and the write reached the host");
    assert_eq!(kv(&out, "hooks"), "readonly", ".git/hooks");
    assert_eq!(kv(&out, "config"), "readonly", ".git/config");
    assert_eq!(kv(&out, "tole"), "readonly", ".tole");
    assert_eq!(kv(&out, "tolenew"), "readonly", "nothing new under .tole");
    for p in [".git/hooks/evil", ".tole/evil", ".tole/sub"] {
        assert!(!w.join(p).exists(), "{p} was not created on the host");
    }
    assert_eq!(
        std::fs::read_to_string(w.join(".git/config")).unwrap(),
        "[core]\n"
    );
    assert!(kv(&out, "caps").ends_with('0'), "no capabilities: {out}");
    assert_eq!(kv(&out, "nnp"), "NoNewPrivs:1");
    assert_eq!(kv(&out, "session"), "1", "TOLE_UNDER_SESSION still set");
    assert_eq!(kv(&out, "tmp"), "writable", "a private /tmp");
    assert!(!Path::new(&format!("/tmp/sbx-leak-{}", std::process::id())).exists());
    assert_eq!(
        kv(&out, "tolevisible"),
        "0",
        "tole is not in the PID namespace"
    );
    assert_eq!(
        kv(&out, "pid1canary"),
        "0",
        "bwrap's own environment is scrubbed"
    );
    assert_eq!(kv(&out, "envcanary"), "0", "no secret in the command's env");
    assert_eq!(
        kv(&out, "granted"),
        "granted-value-0123",
        "a [secrets] grant still reaches it"
    );
    let fds = kv(&out, "fds");
    assert!(
        !fds.split_whitespace().any(|f| f == "199"),
        "no inherited descriptor: {fds}"
    );
    let l = kv(&out, "limits");
    for want in [
        "Max cpu time 120 120 seconds",
        "Max address space 4294967296 4294967296 bytes",
        "Max processes 512 512 processes",
        "Max file size 67108864 67108864 bytes",
    ] {
        assert!(l.contains(want), "{want} in {l}");
    }
}

/// Stdout, stderr, the exit code and argv handling are what they are
/// without a sandbox; the command is argv, never a shell string.
#[test]
fn output_status_and_argv_behave_as_without_a_sandbox() {
    if skipped() {
        return;
    }
    let tool = RunCommandTool::new(ws("parity"));
    let out = run(&tool, "sh -c 'echo out; echo err >&2; exit 3'");
    assert_eq!(out["status"], 3);
    assert_eq!(text(&out, "stdout"), "out\n");
    assert_eq!(text(&out, "stderr"), "err\n");
    assert_eq!(out["stdout_truncated"], false);
    // Hostile-looking arguments reach the program verbatim.
    let out = run(
        &tool,
        r#"printf '%s|' --bind / / ';' '$(id)' '--' '--setenv' 'a b'"#,
    );
    assert_eq!(text(&out, "stdout"), "--bind|/|/|;|$(id)|--|--setenv|a b|");
    // The output cap works as before.
    let out = run(&tool, "sh -c 'yes x | head -c 100000'");
    assert_eq!(out["stdout_truncated"], true);
    assert!(text(&out, "stdout").chars().count() <= 4000);
    // An unknown program is an error, not a hang.
    assert!(tool
        .execute(json!({"command": "no-such-program-xyz"}))
        .is_err());
}

/// The approval boundary is unchanged: the tool is still Write, the same
/// argv refusals apply before any sandbox, and the sandbox adds none.
#[test]
fn the_tools_checks_run_before_the_sandbox() {
    if skipped() {
        return;
    }
    let tool = RunCommandTool::new(ws("checks"));
    assert_eq!(tool.risk(), tole_core::tool::Risk::Write);
    let e = tool.execute(json!({"command": "rm -rf /"})).unwrap_err();
    assert!(e.contains("destructive"), "{e}");
    let e = tool
        .execute(json!({"command": "tole config trust"}))
        .unwrap_err();
    assert!(e.contains("may not run the tole binary"), "{e}");
}

/// A workspace with a real repository (`git init`, one commit of `f`) and a
/// `.tole` dir.
fn repo(tag: &str) -> PathBuf {
    let w = ws(tag);
    host_git(&w, &["init", "-q"]);
    std::fs::write(w.join("f"), "one\n").unwrap();
    host_git(&w, &["add", "f"]);
    host_git(&w, &["commit", "-q", "-m", "one"]);
    std::fs::create_dir_all(w.join(".tole")).unwrap();
    w
}

/// Runs `git args` on the HOST in `w`, as the owner would later; fails the test
/// when git fails.
fn host_git_out(w: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        // No background maintenance: a detached `git maintenance`/`gc --auto`
        // after a commit rewrites `.git` while a test is comparing its tree.
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
        .arg("-C")
        .arg(w)
        .args(args)
        .output()
        .expect("git is needed for this test")
}

fn host_git(w: &Path, args: &[&str]) {
    let out = host_git_out(w, args);
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// Every file, directory and link under `dir` with its bytes, for comparing a
/// whole tree before and after (not just the leaves).
fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            let t = e.file_type().unwrap();
            if t.is_dir() {
                out.push((format!("{rel}/"), Vec::new()));
                walk(base, &p, out);
            } else if t.is_symlink() {
                let to = std::fs::read_link(&p).unwrap();
                out.push((format!("{rel} -> {}", to.display()), Vec::new()));
            } else {
                out.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out
}

/// Runs each command in the sandbox and asserts it fails (status not 0).
fn all_fail(tool: &RunCommandTool, attacks: &[&str]) {
    for a in attacks {
        let out = run(tool, a);
        assert_ne!(out["status"], 0, "must fail: {a}: {out}");
    }
}

/// #849: the reviewer's attack on a writable `.git` mount point. Git honours
/// `commondir` INSIDE the gitdir, so pointing it at a planted gitdir in the
/// workspace made a later `git commit` or `git status` on the HOST run the
/// planted hook and `core.fsmonitor`. Now `.git` is read-only as a whole: the
/// write that redirects git fails, and git on the host runs nothing.
#[test]
fn a_commondir_redirect_cannot_make_the_host_run_planted_code() {
    if skipped() {
        return;
    }
    let w = repo("commondir");
    let (hook_mark, fsm_mark) = (w.join("PWNED_HOOK"), w.join("PWNED_FSM"));
    std::fs::write(
        w.join("attack.sh"),
        format!(
            "mkdir evil && cp -r .git/objects .git/refs .git/HEAD evil/ && mkdir evil/hooks\n\
             printf '#!/bin/sh\\ntouch {h}\\n' > evil/hooks/pre-commit\n\
             chmod +x evil/hooks/pre-commit\n\
             printf '[core]\\n\\trepositoryformatversion = 0\\n\\tfsmonitor = touch {f}; echo\\n' > evil/config\n\
             echo ../evil > .git/commondir\n",
            h = hook_mark.display(),
            f = fsm_mark.display()
        ),
    )
    .unwrap();
    let before = tree(&w.join(".git"));
    let tool = RunCommandTool::new(w.clone());
    let out = run(&tool, "sh attack.sh");
    assert_ne!(out["status"], 0, "the redirect must fail: {out}");
    assert!(!w.join(".git/commondir").exists());
    // The owner, later, on the host.
    std::fs::write(w.join("f"), "two\n").unwrap();
    host_git(&w, &["status", "--short"]);
    host_git(&w, &["commit", "-q", "-am", "two"]);
    host_git(&w, &["status", "--short"]);
    assert!(!hook_mark.exists(), "the planted hook ran on the host");
    assert!(!fsm_mark.exists(), "the planted fsmonitor ran on the host");
    // The commit really went to the real repository.
    let log = host_git_out(&w, &["log", "--oneline"]);
    assert!(String::from_utf8_lossy(&log.stdout).contains("two"));
    drop(before);
}

/// Every way to move, remove, replace or write `.git` (and `.tole`) fails, and
/// the whole `.git` tree is byte for byte what it was.
#[test]
fn every_way_to_move_remove_or_write_the_git_dir_fails_and_its_tree_is_unchanged() {
    if skipped() {
        return;
    }
    let w = repo("gitattack");
    let before = tree(&w.join(".git"));
    let tole_before = tree(&w.join(".tole"));
    let tool = RunCommandTool::new(w.clone());
    all_fail(
        &tool,
        &[
            "mv .git .g",
            "rm -rf .git",
            "rm -rf .tole",
            "mv .tole .tole.old",
            "rmdir .git/hooks",
            "mv .git/hooks .git/hooks.old",
            "mv .git/config .git/config.old",
            "rm -f .git/config",
            "rm -f .git/index",
            "rm -rf .git/objects",
            "mkdir .git/x",
            "touch .git/index.lock",
            "sh -c 'echo x > .git/HEAD'",
            "sh -c 'echo x > .git/commondir'",
            "sh -c 'echo [core] >> .git/config'",
            "sh -c 'echo x > .git/hooks/pre-commit'",
            "sh -c 'echo x > .tole/evil'",
            // Replace by rename over a mount point or into the mount.
            "sh -c 'mkdir h2 && mv h2 .git/hooks'",
            "sh -c 'echo x > c2 && mv c2 .git/config'",
            "sh -c 'mkdir g2 && mv g2 .git'",
            // The reviewer's first chain: the rename used to succeed.
            "sh -c 'mv .git .g && cp -a .g .git && printf x > .git/hooks/pre-commit'",
            // Hard links to a protected inode (EXDEV across mounts).
            "ln .git/config .git/config2",
            "ln .git/HEAD head-in-root",
            "ln .git/hooks/applypatch-msg.sample .git/hooks/h2",
        ],
    );
    assert_eq!(tree(&w.join(".git")), before, ".git changed");
    assert_eq!(tree(&w.join(".tole")), tole_before, ".tole changed");
    for p in [".g", ".git.old", "head-in-root"] {
        assert!(!w.join(p).exists(), "{p}");
    }
}

/// A leaf that does not exist under an existing `.git` (a repository made with
/// `git init --template=`, or stripped) cannot be created either.
#[test]
fn a_repo_without_hooks_or_config_cannot_get_them_planted() {
    if skipped() {
        return;
    }
    let w = repo("bareleaves");
    std::fs::remove_dir_all(w.join(".git/hooks")).unwrap();
    std::fs::remove_file(w.join(".git/config")).unwrap();
    let before = tree(&w.join(".git"));
    let tool = RunCommandTool::new(w.clone());
    all_fail(
        &tool,
        &[
            "mkdir .git/hooks",
            "sh -c 'mkdir -p .git/hooks && echo x > .git/hooks/pre-commit'",
            "sh -c 'echo [core] > .git/config'",
            "sh -c 'echo x > .git/info/exclude2'",
        ],
    );
    assert_eq!(tree(&w.join(".git")), before);
    assert!(!w.join(".git/hooks").exists() && !w.join(".git/config").exists());
}

/// A submodule's gitdir lives under `.git/modules/<name>` with its own hooks
/// and config: they are inside the read-only `.git` too.
#[test]
fn a_submodule_gitdir_under_git_modules_cannot_get_a_hook_or_config() {
    if skipped() {
        return;
    }
    let w = repo("submodule");
    std::fs::create_dir_all(w.join(".git/modules/sub/hooks")).unwrap();
    std::fs::write(w.join(".git/modules/sub/config"), "[core]\n").unwrap();
    let before = tree(&w.join(".git"));
    let tool = RunCommandTool::new(w.clone());
    all_fail(
        &tool,
        &[
            "sh -c 'echo x > .git/modules/sub/hooks/pre-commit'",
            "sh -c 'echo [core] >> .git/modules/sub/config'",
            "mkdir .git/modules/other",
            "rm -rf .git/modules",
            "mv .git/modules .git/modules.old",
        ],
    );
    assert_eq!(tree(&w.join(".git")), before);
}

/// What still works with a read-only `.git`, and what the model sees when it
/// does not. Nothing hangs (the tool's own 30 s timeout would fail the test).
#[test]
fn git_reads_work_and_git_writes_fail_with_a_clear_error() {
    if skipped() {
        return;
    }
    let w = repo("gitrw");
    std::fs::write(w.join("f"), "two\n").unwrap();
    let before = tree(&w.join(".git"));
    let tool = RunCommandTool::new(w.clone());
    let started = Instant::now();
    for ok in [
        "git log --oneline",
        "git diff",
        "git show --stat HEAD",
        "git status --short",
        "git rev-parse HEAD",
        "git branch --list",
        "git ls-files",
    ] {
        let out = run(&tool, ok);
        assert_eq!(out["status"], 0, "{ok} must work: {out}");
    }
    assert!(text(&run(&tool, "git log --oneline"), "stdout").contains("one"));
    for bad in [
        "git add f",
        "git -c user.name=t -c user.email=t@t commit -qam x",
        "git branch b",
        "git checkout -q -b b",
        "git stash",
        "git config x.y z",
        "git remote add o /nonexistent",
        "git gc -q",
    ] {
        let out = run(&tool, bad);
        assert_ne!(out["status"], 0, "{bad} must fail: {out}");
        let err = text(&out, "stderr");
        if bad == "git stash" {
            // git stash fails with exit 1 and says nothing here.
            continue;
        }
        assert!(
            err.contains("Read-only file system") || err.contains("read-only"),
            "{bad}: the model should see why: {out}"
        );
    }
    assert!(started.elapsed() < Duration::from_secs(60), "nothing hangs");
    assert_eq!(tree(&w.join(".git")), before, "no write got through");
}

/// A gitdir file (worktree, submodule) cannot be swapped for a directory.
#[test]
fn a_gitdir_file_cannot_be_replaced_by_a_directory_with_hooks() {
    if skipped() {
        return;
    }
    let w = ws("gitfile");
    std::fs::write(w.join(".git"), "gitdir: /nonexistent/elsewhere\n").unwrap();
    let tool = RunCommandTool::new(w.clone());
    for a in [
        "rm -f .git",
        "mv .git .g",
        "sh -c 'rm .git && mkdir -p .git/hooks'",
        "sh -c 'echo x >> .git'",
    ] {
        let out = run(&tool, a);
        assert_ne!(out["status"], 0, "must fail: {a}: {out}");
    }
    assert_eq!(
        std::fs::read_to_string(w.join(".git")).unwrap(),
        "gitdir: /nonexistent/elsewhere\n"
    );
}

/// A link cannot be protected by a mount, so the launch is refused; nothing
/// runs and nothing is created.
#[test]
fn a_symlinked_git_dir_or_leaf_is_refused_not_bound() {
    if skipped() {
        return;
    }
    for (tag, link) in [
        ("l-git", ".git"),
        ("l-hooks", ".git/hooks"),
        ("l-tole", ".tole"),
    ] {
        let w = ws(tag);
        std::fs::create_dir_all(w.join("real")).unwrap();
        if link != ".git" {
            std::fs::create_dir_all(w.join(".git")).unwrap();
        }
        std::os::unix::fs::symlink(w.join("real"), w.join(link)).unwrap();
        let tool = RunCommandTool::new(w.clone());
        let e = tool.execute(json!({"command": "touch ran"})).unwrap_err();
        assert!(e.contains("symbolic link") && e.contains(link), "{e}");
        assert!(!w.join("ran").exists());
    }
}

/// Residual, pinned so it stays documented: with no `.git` at launch nothing
/// is protected and the launch creates nothing on the host; a repository made
/// inside the sandbox is the owner's to review before running git there.
#[test]
fn without_a_git_dir_at_launch_nothing_is_created_or_protected() {
    if skipped() {
        return;
    }
    let w = ws("nogit");
    let tool = RunCommandTool::new(w.clone());
    let out = run(&tool, "sh -c 'test ! -e .git && echo none'");
    assert_eq!(text(&out, "stdout").trim(), "none");
    assert!(!w.join(".git").exists(), "the launch did not create .git");
    assert_eq!(run(&tool, "git init -q")["status"], 0);
    assert!(w.join(".git/hooks").is_dir(), "git init works inside");
}

/// Two sessions, two workspaces, at the same time: each sees its own.
#[test]
fn concurrent_sessions_see_only_their_own_workspace() {
    if skipped() {
        return;
    }
    let dirs: Vec<PathBuf> = (0..6).map(|i| ws(&format!("sess{i}"))).collect();
    for (i, d) in dirs.iter().enumerate() {
        std::fs::write(d.join(format!("mine{i}")), "x").unwrap();
    }
    let handles: Vec<_> = dirs
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, d)| {
            let all = dirs.clone();
            std::thread::spawn(move || {
                let tool = RunCommandTool::new(d.clone());
                for _ in 0..4 {
                    for (j, other) in all.iter().enumerate() {
                        let want = if i == j { "yes" } else { "no" };
                        let line = format!(
                            "sh -c 'test -e {}/mine{j} && echo yes || echo no'",
                            other.display()
                        );
                        let out = run(&tool, &line);
                        assert_eq!(text(&out, "stdout").trim(), want, "session {i} saw {j}");
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

fn host_runs(needle: &str) -> bool {
    !host_pids(needle).is_empty()
}

/// Host pids whose command line holds `needle`.
fn host_pids(needle: &str) -> Vec<libc::pid_t> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter(|e| {
            std::fs::read(e.path().join("cmdline"))
                .map(|b| String::from_utf8_lossy(&b).contains(needle))
                .unwrap_or(false)
        })
        .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
        .collect()
}

fn wait_gone(needle: &str) -> bool {
    test_dir::poll_until(test_dir::WAIT, || (!host_runs(needle)).then_some(())).is_some()
}

/// The timeout kills the command and everything it started: no orphan is
/// left in the sandbox's PID namespace.
#[test]
fn a_timeout_kills_the_command_and_its_whole_process_group() {
    if skipped() {
        return;
    }
    let w = ws("timeout");
    let (a, b) = ("31337.5", "31338.5");
    let mut cmd = command::command_for(
        command::Consumer::RunCommand,
        &w,
        "sh",
        &[
            "-c".into(),
            format!("sleep {a} & (sleep {b} & sleep {a}) & sleep {b}"),
        ],
    )
    .unwrap();
    let started = Instant::now();
    let err =
        tole_core::subprocess::run_with_timeout(&mut cmd, Duration::from_secs(2)).unwrap_err();
    assert!(err.contains("timed out"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(
        wait_gone(&format!("sleep\0{a}")) && wait_gone(&format!("sleep\0{b}")),
        "a descendant outlived the timeout"
    );
}

/// A cancelled command is cleaned up the same way.
#[test]
fn a_cancel_kills_the_command_and_its_descendants() {
    if skipped() {
        return;
    }
    let w = ws("cancel");
    let a = "31339.5";
    let mut cmd = command::command_for(
        command::Consumer::RunCommand,
        &w,
        "sh",
        &["-c".into(), format!("sleep {a} & sleep {a}")],
    )
    .unwrap();
    let token = tole_core::cancel::CancelToken::new();
    let t = token.clone();
    let needle = format!("sleep\0{a}");
    std::thread::spawn(move || {
        // Cancel once the command's descendants run, not after a guess at
        // how long the sandbox takes to start (#812).
        test_dir::wait_until(test_dir::WAIT, "the command to start", || {
            host_runs(&needle)
        });
        t.cancel();
    });
    let err = tole_core::subprocess::run_with_timeout_or_cancel(
        &mut cmd,
        Duration::from_secs(20),
        &token,
    )
    .unwrap_err();
    assert_eq!(err, tole_core::subprocess::CANCELLED);
    assert!(
        wait_gone(&format!("sleep\0{a}")),
        "a descendant outlived the cancel"
    );
}

/// A job runs in the sandbox too, outlives the call (no die-with-parent),
/// writes its log through the descriptor tole opened, and finishes.
#[test]
fn a_job_runs_sandboxed_detached_and_logs() {
    if skipped() {
        return;
    }
    let s = setup();
    let w = project("job");
    let start = JobStartTool::new(w.clone());
    let out = start
        .execute(json!({"command": format!(
            "sh -c 'echo job-ok; echo \"home=$HOME\"; test -e {} && echo data=yes || echo data=no; echo x > made; sleep 2'",
            s.data_secret.display()
        )}))
        .unwrap();
    let job = text(&out, "job");
    let pid = out["pid"].as_u64().unwrap();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline = String::from_utf8_lossy(&cmdline).into_owned();
    assert!(
        cmdline.contains("bwrap"),
        "the job is the bwrap process: {cmdline:?}"
    );
    assert!(cmdline.contains("--unshare-all") && cmdline.contains("--cap-drop"));
    assert!(
        !cmdline.contains("--die-with-parent"),
        "a job outlives tole"
    );
    assert!(!cmdline.contains(CANARY), "no secret in the argv");
    let poll = JobPollTool::new(w.clone());
    let last = test_dir::wait_for(test_dir::WAIT, "the job to finish", || {
        let r = poll.execute(json!({"job": job})).unwrap();
        (r["running"] == json!(false)).then_some(r)
    });
    let log = text(&last, "log_tail");
    assert!(
        log.contains("job-ok") && log.contains("home=/scratch") && log.contains("data=no"),
        "{log}"
    );
    assert!(w.join("made").exists(), "the job wrote the workspace");
}

/// The sandbox of a command that needs a program under a system dir only
/// works because the dir is bound; a program in an owner dir is bound
/// resolved. Here: a script in a bin dir off PATH-less system dirs.
#[test]
fn a_program_from_an_owner_dir_runs_through_its_bound_directory() {
    if skipped() {
        return;
    }
    let bin = ws("ownerbin");
    let p = bin.join("owner-tool");
    test_dir::write_executable(&p, "#!/bin/sh\necho owner-tool-ran\n");
    let w = ws("owner-ws");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // `command_for` reads this process's PATH.
    std::env::set_var("PATH", &path);
    let mut cmd =
        command::command_for(command::Consumer::RunCommand, &w, "owner-tool", &[]).unwrap();
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, Duration::from_secs(10)).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "owner-tool-ran\n");
}

/// A job that was spawned but cannot be tracked is stopped as a whole: the
/// sandbox's init is in the job's process group, and its death takes the
/// namespace with it. Without the group stop the sandbox would stay (jobs
/// have no die-with-parent).
#[test]
fn a_job_that_cannot_be_tracked_leaves_no_process_behind() {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    if skipped() {
        return;
    }
    let w = ws("jobstop");
    let spawn = |n: &str| {
        let mut cmd = command::command_for(
            command::Consumer::JobStart,
            &w,
            "sh",
            &["-c".into(), format!("sleep {n} & sleep {n}")],
        )
        .unwrap();
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = cmd.spawn().unwrap();
        let needle = format!("sleep\0{n}");
        test_dir::wait_until(test_dir::WAIT, "the job to start", || host_runs(&needle));
        (child, needle)
    };
    // Sanity: ending only the first process leaves the detached sandbox.
    let (mut child, needle) = spawn("31341.5");
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        host_runs(&needle),
        "a detached job outlives its bwrap process"
    );
    // Clean up the leftover sandbox: only processes of this test's own
    // unique marker.
    for pid in host_pids(&needle) {
        // SAFETY: kill takes two integers.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(wait_gone(&needle), "cleanup of the sanity case");
    // The helper job_start uses stops everything.
    let (child, needle) = spawn("31342.5");
    tole_core::jobs::kill_detached(child);
    assert!(wait_gone(&needle), "kill_detached left a process behind");
}

#[test]
fn the_same_consumer_settings_are_fine_and_different_ones_are_refused() {
    setup();
    let current = command::consumers();
    assert!(command::set_consumers(current).is_ok());
    let mut other = current;
    other.network = Network::Host;
    assert!(command::set_consumers(other).is_err());
    assert_eq!(command::consumers(), current, "unchanged after the refusal");
}

/// Slice 2 (#748): the mission `--verify` command (`sh -c <string>`) runs in
/// the same `command` profile: the workspace is writable, `.git/hooks`,
/// `.git/config` and `.tole` are not, `$HOME` and the data dir are absent, and
/// the exit status and output are what they are without a sandbox.
#[test]
fn the_verify_command_runs_in_the_command_profile() {
    if skipped() {
        return;
    }
    let s = setup();
    let w = project("verify");
    let script = format!(
        "echo \"wsw=$( (echo x > made) 2>/dev/null && echo writable || echo readonly)\"; \
         echo \"hooks=$( (echo x > .git/hooks/evil) 2>/dev/null && echo writable || echo readonly)\"; \
         echo \"config=$( (echo x >> .git/config) 2>/dev/null && echo writable || echo readonly)\"; \
         echo \"tole=$( (echo x > .tole/evil) 2>/dev/null && echo writable || echo readonly)\"; \
         echo \"home=$(test -e {}/canary && echo visible || echo absent)\"; \
         echo \"data=$(test -e {} && echo visible || echo absent)\"; \
         echo err >&2; exit 3",
        s.home.display(),
        s.data_secret.display()
    );
    let mut cmd =
        command::command_for(command::Consumer::Verify, &w, "sh", &["-c".into(), script]).unwrap();
    tole_core::subprocess::scrub_env_for(&mut cmd, tole_core::secret_release::VERIFY);
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, Duration::from_secs(60)).unwrap();
    assert_eq!(out.status.code(), Some(3), "the exit status is the verdict");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let out = json!({ "stdout": stdout });
    assert_eq!(kv(&out, "wsw"), "writable");
    assert!(w.join("made").exists(), "the write reached the host");
    assert_eq!(kv(&out, "hooks"), "readonly", ".git/hooks");
    assert_eq!(kv(&out, "config"), "readonly", ".git/config");
    assert_eq!(kv(&out, "tole"), "readonly", ".tole");
    assert_eq!(kv(&out, "home"), "absent");
    assert_eq!(kv(&out, "data"), "absent");
    for p in [".git/hooks/evil", ".tole/evil"] {
        assert!(!w.join(p).exists(), "{p} was not created on the host");
    }
    assert_eq!(
        std::fs::read_to_string(w.join(".git/config")).unwrap(),
        "[core]\n"
    );
}

/// The owner's `git status`, `diff` and `add -A` on the HOST in `w`: what a
/// nested planted config or hook would fire on.
fn host_git_routine(w: &Path) {
    for args in [&["status", "--short"][..], &["diff"], &["add", "-A"]] {
        let _ = host_git_out(w, args);
    }
}

/// A registered submodule `sub`: its `.git` is a gitdir FILE inside the
/// workspace pointing at `../.git/modules/sub`. The reviewer's attack moved it
/// aside, copied the gitdir in as a real directory and set a `core.fsmonitor`
/// in it; the owner's plain `git status` in the OUTER repository then ran the
/// script. Now the nested `.git` file is its own read-only mount.
#[test]
fn a_registered_submodule_cannot_get_a_planted_fsmonitor() {
    if skipped() {
        return;
    }
    let w = repo("submod");
    let sub_src = ws("submod-src");
    host_git(&sub_src, &["init", "-q"]);
    std::fs::write(sub_src.join("s"), "s\n").unwrap();
    host_git(&sub_src, &["add", "s"]);
    host_git(&sub_src, &["commit", "-q", "-m", "s"]);
    host_git(
        &w,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_src.to_str().unwrap(),
            "sub",
        ],
    );
    host_git(&w, &["commit", "-q", "-m", "add sub"]);
    assert!(w.join("sub/.git").is_file(), "a gitdir file");
    let mark = w.join("PWNED_FSM");
    std::fs::write(
        w.join("fsm.sh"),
        format!("#!/bin/sh\ntouch {}\n", mark.display()),
    )
    .unwrap();
    std::fs::write(
        w.join("attack.sh"),
        format!(
            "mv sub/.git sub/.gx && cp -a .git/modules/sub sub/.git\n\
             git config -f sub/.git/config core.fsmonitor {}/fsm.sh\n",
            w.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        w.join("fsm.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let before = tree(&w.join(".git"));
    let tool = RunCommandTool::new(w.clone());
    let out = run(&tool, "sh attack.sh");
    assert_ne!(out["status"], 0, "the first step must fail: {out}");
    assert!(
        w.join("sub/.git").is_file(),
        "the gitdir file is still there"
    );
    assert!(!w.join("sub/.gx").exists());
    // The file that names the submodules cannot be rewritten or removed (the
    // next launch's plan reads it).
    all_fail(
        &tool,
        &[
            "sh -c 'echo > .gitmodules'",
            "rm -f .gitmodules",
            "mv .gitmodules .gitmodules.old",
        ],
    );
    assert!(std::fs::read_to_string(w.join(".gitmodules"))
        .unwrap()
        .contains("path = sub"));
    assert_eq!(
        tree(&w.join(".git")),
        before,
        "from the sandbox, nothing changed"
    );
    host_git_routine(&w);
    assert!(!mark.exists(), "the planted fsmonitor ran on the host");
}

/// `git init --separate-git-dir=ws/.gd`: the top-level `.git` is a file whose
/// target is inside the workspace. The target is read-only too.
#[test]
fn a_separate_git_dir_inside_the_workspace_cannot_get_a_hook() {
    if skipped() {
        return;
    }
    let w = ws("sepgit");
    let gd = w.join(".gd");
    host_git(
        &w,
        &["init", "-q", "--separate-git-dir", gd.to_str().unwrap()],
    );
    assert!(w.join(".git").is_file());
    std::fs::write(w.join("f"), "one\n").unwrap();
    host_git(&w, &["add", "f"]);
    host_git(&w, &["commit", "-q", "-m", "one"]);
    let mark = w.join("PWNED_HOOK");
    std::fs::write(
        w.join("attack.sh"),
        format!(
            "mkdir -p .gd/hooks && printf '#!/bin/sh\\ntouch {}\\n' > .gd/hooks/pre-commit \
             && chmod +x .gd/hooks/pre-commit\n",
            mark.display()
        ),
    )
    .unwrap();
    let before = tree(&gd);
    let tool = RunCommandTool::new(w.clone());
    all_fail(
        &tool,
        &[
            "sh attack.sh",
            "rm -rf .gd",
            "mv .gd .gd2",
            "sh -c 'echo x > .gd/config'",
            "sh -c 'rm .git && mkdir -p .git/hooks'",
        ],
    );
    assert_eq!(tree(&gd), before, "the separate gitdir changed");
    std::fs::write(w.join("f"), "two\n").unwrap();
    host_git(&w, &["commit", "-q", "-am", "two"]);
    assert!(!mark.exists(), "the planted hook ran on the host");
}

/// A child agent's worktree (`.tole-worktrees/<id>`): its `.git` file cannot be
/// replaced by a directory with hooks.
#[test]
fn a_child_worktree_git_file_cannot_be_replaced() {
    if skipped() {
        return;
    }
    let w = repo("wtree");
    host_git(
        &w,
        &["worktree", "add", "-q", ".tole-worktrees/wt1", "-b", "wt1"],
    );
    assert!(w.join(".tole-worktrees/wt1/.git").is_file());
    let before = tree(&w.join(".git"));
    let tool = RunCommandTool::new(w.clone());
    all_fail(
        &tool,
        &[
            "rm -f .tole-worktrees/wt1/.git",
            "mv .tole-worktrees/wt1/.git .tole-worktrees/wt1/.gx",
            "sh -c 'rm .tole-worktrees/wt1/.git && mkdir -p .tole-worktrees/wt1/.git/hooks'",
            "sh -c 'echo x >> .tole-worktrees/wt1/.git'",
        ],
    );
    all_fail(
        &tool,
        &[
            "rm -rf .tole-worktrees/wt1",
            "mv .tole-worktrees/wt1 .tole-worktrees/wt2",
            "mv .tole-worktrees .tole-worktrees.old",
            "mkdir .tole-worktrees/wt3",
        ],
    );
    assert!(w.join(".tole-worktrees/wt1/.git").is_file());
    assert_eq!(tree(&w.join(".git")), before);
    host_git(&w.join(".tole-worktrees/wt1"), &["status", "--short"]);
}

/// F1: a submodule that has submodules of its own. The plan reads only the
/// top-level `.gitmodules`, so the inner gitdir (reachable through
/// `mid/inner/.git`) would be unprotected: the launch is refused, the command
/// never runs, and the host's git runs nothing.
#[test]
fn a_submodule_with_submodules_of_its_own_refuses_the_launch() {
    if skipped() {
        return;
    }
    let mk = |tag: &str| {
        let d = ws(tag);
        host_git(&d, &["init", "-q"]);
        std::fs::write(d.join("f"), "x\n").unwrap();
        host_git(&d, &["add", "f"]);
        host_git(&d, &["commit", "-q", "-m", "x"]);
        d
    };
    let allow = ["-c", "protocol.file.allow=always"];
    let add = |into: &Path, src: &Path, name: &str| {
        let mut a: Vec<&str> = allow.to_vec();
        a.extend(["submodule", "add", "-q", src.to_str().unwrap(), name]);
        host_git(into, &a);
        host_git(into, &["commit", "-q", "-m", "add"]);
    };
    let inner = mk("nest-inner");
    let mid = mk("nest-mid");
    add(&mid, &inner, "inner");
    let top = mk("nest-top");
    add(&top, &mid, "mid");
    let mut a: Vec<&str> = allow.to_vec();
    a.extend(["submodule", "update", "--init", "--recursive", "-q"]);
    host_git(&top, &a);
    assert!(top.join("mid/.gitmodules").is_file(), "the layout exists");
    assert!(top.join("mid/inner/.git").exists());
    let mark = top.join("PWNED_FSM");
    let tool = RunCommandTool::new(top.clone());
    let e = tool
        .execute(json!({"command": format!("touch {}", top.join("ran").display())}))
        .unwrap_err();
    assert!(e.contains("nested submodules") && e.contains("mid"), "{e}");
    assert!(!top.join("ran").exists(), "the command never ran");
    host_git_routine(&top);
    assert!(!mark.exists(), "nothing ran on the host");
}

/// F2: `.git` is a file whose target is a worktree of a repository that lives
/// INSIDE the workspace (`repo.git`); `commondir` sends git to its config and
/// hooks, which were writable. The launch is refused, the command never runs.
#[test]
fn a_gitdir_redirected_by_commondir_inside_the_workspace_refuses_the_launch() {
    if skipped() {
        return;
    }
    let w = ws("commondir-ws");
    host_git(&w, &["init", "-q", "--bare", "repo.git"]);
    let wt = w.join("repo.git/worktrees/ws2");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(wt.join("commondir"), "../..\n").unwrap();
    std::fs::write(wt.join("gitdir"), format!("{}\n", w.join(".git").display())).unwrap();
    std::fs::write(w.join(".git"), format!("gitdir: {}\n", wt.display())).unwrap();
    host_git(&w, &["status", "--short"]);
    let mark = w.join("PWNED_FSM");
    let tool = RunCommandTool::new(w.clone());
    let e = tool
        .execute(json!({"command": format!(
            "git config --file repo.git/config core.fsmonitor 'touch {}; echo'", mark.display())}))
        .unwrap_err();
    assert!(e.contains("commondir") && e.contains("redirects"), "{e}");
    host_git(&w, &["status", "--short"]);
    assert!(!mark.exists(), "nothing ran on the host");
    assert!(
        !std::fs::read_to_string(w.join("repo.git/config"))
            .unwrap()
            .contains("fsmonitor"),
        "the config was not written"
    );
}

/// A submodule that is listed but not initialized (a clone without
/// `--recursive`): `sub/` is an empty directory with no `.git` to bind, and a
/// command could `git init sub` and plant a `core.fsmonitor` that the host's
/// plain `git status` in the outer repository runs. The launch is refused.
#[test]
fn an_uninitialized_submodule_refuses_the_launch_and_nothing_runs() {
    if skipped() {
        return;
    }
    let sub_src = ws("uninit-src");
    host_git(&sub_src, &["init", "-q"]);
    std::fs::write(sub_src.join("s"), "s\n").unwrap();
    host_git(&sub_src, &["add", "s"]);
    host_git(&sub_src, &["commit", "-q", "-m", "s"]);
    let top = repo("uninit-top");
    host_git(
        &top,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_src.to_str().unwrap(),
            "sub",
        ],
    );
    host_git(&top, &["commit", "-q", "-m", "add sub"]);
    // A plain clone: the gitlink and `.gitmodules` are there, `sub/` is empty.
    let clone = ws("uninit-clone");
    std::fs::remove_dir_all(&clone).unwrap();
    let out = std::process::Command::new("git")
        .args(["clone", "-q"])
        .arg(&top)
        .arg(&clone)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(clone.join("sub").is_dir() && !clone.join("sub/.git").exists());
    let mark = clone.join("PWNED_FSM");
    std::fs::write(
        clone.join("attack.sh"),
        format!(
            "git init -q sub\ngit -C sub config core.fsmonitor 'touch {}; echo'\n",
            mark.display()
        ),
    )
    .unwrap();
    let tool = RunCommandTool::new(clone.clone());
    let e = tool
        .execute(json!({"command": "sh attack.sh"}))
        .unwrap_err();
    assert!(
        e.contains("not initialized") && e.contains("sub") && e.contains("submodule update --init"),
        "{e}"
    );
    assert!(!clone.join("sub/.git").exists(), "the command never ran");
    host_git_routine(&clone);
    assert!(!mark.exists(), "nothing ran on the host");
    // Initialized, the same workspace is usable (and `.git` of `sub` read-only).
    host_git(
        &clone,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "-q",
        ],
    );
    let ok = run(&tool, "sh -c 'echo x > sub/.git/config'");
    assert_ne!(ok["status"], 0, "now protected: {ok}");
}

/// A linked worktree elsewhere in the workspace (not under `.tole-worktrees`)
/// cannot be protected: the launch is refused and the command never runs.
#[test]
fn a_linked_worktree_elsewhere_in_the_workspace_refuses_the_launch() {
    if skipped() {
        return;
    }
    let w = repo("linkedwt");
    host_git(&w, &["worktree", "add", "-q", "other/wt", "-b", "wtb"]);
    assert!(w.join("other/wt/.git").is_file());
    let tool = RunCommandTool::new(w.clone());
    let e = tool
        .execute(json!({"command": format!("touch {}", w.join("ran").display())}))
        .unwrap_err();
    assert!(
        e.contains("linked worktree") && e.contains("other/wt/.git"),
        "{e}"
    );
    assert!(!w.join("ran").exists(), "the command never ran");
    // Removed, the workspace launches again.
    host_git(&w, &["worktree", "remove", "--force", "other/wt"]);
    let out = run(&tool, "sh -c 'echo ok'");
    assert_eq!(out["status"], 0, "{out}");
}

/// The same with a separate git dir: the registry is in `.gd/worktrees`, the
/// target of the `.git` file, not in a `.git` directory.
#[test]
fn a_linked_worktree_of_a_separate_git_dir_refuses_the_launch() {
    if skipped() {
        return;
    }
    let w = ws("linkedwt-sep");
    let gd = w.join(".gd");
    host_git(
        &w,
        &["init", "-q", "--separate-git-dir", gd.to_str().unwrap()],
    );
    std::fs::write(w.join("f"), "one\n").unwrap();
    host_git(&w, &["add", "f"]);
    host_git(&w, &["commit", "-q", "-m", "one"]);
    host_git(&w, &["worktree", "add", "-q", "other/wt", "-b", "wtb"]);
    assert!(gd.join("worktrees").is_dir());
    let tool = RunCommandTool::new(w.clone());
    let e = tool
        .execute(json!({"command": format!("touch {}", w.join("ran").display())}))
        .unwrap_err();
    assert!(e.contains("linked worktree"), "{e}");
    assert!(!w.join("ran").exists(), "the command never ran");
}
