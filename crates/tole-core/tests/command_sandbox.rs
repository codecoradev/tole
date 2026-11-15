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

/// A workspace with a real repository (`git init`), a `.tole` dir and a
/// marker in the hooks, so the host state is easy to compare afterwards.
fn repo(tag: &str) -> PathBuf {
    let w = ws(tag);
    let out = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&w)
        .output()
        .expect("git is needed for this test");
    assert!(out.status.success(), "git init: {out:?}");
    std::fs::write(w.join(".git/hooks/marker"), "owner\n").unwrap();
    std::fs::create_dir_all(w.join(".tole")).unwrap();
    w
}

/// #849: the leaf mounts alone did not protect `.git/hooks` and `.git/config`:
/// a mount point cannot be renamed, but its ancestor can, so `mv .git .g &&
/// cp -a .g .git` gave the command a plain copy to plant a hook in, which a
/// later `git commit` outside the sandbox ran. Now `.git` is a mount point
/// too (read-write: git itself keeps working). Every way to move, remove or
/// replace `.git`, `.git/hooks`, `.git/config` and `.tole` fails.
#[test]
fn a_sandboxed_command_cannot_rename_or_replace_the_git_dir_and_its_leaves() {
    if skipped() {
        return;
    }
    let w = repo("gitattack");
    let before = std::fs::read_to_string(w.join(".git/config")).unwrap();
    let tool = RunCommandTool::new(w.clone());
    // Each step prints its exit status; none may succeed.
    let attacks = [
        "mv .git .g",
        "mv .git/hooks .git/hooks.old",
        "mv .git/config .git/config.old",
        "mv .tole .tole.old",
        "rmdir .git/hooks",
        "rm -rf .git",
        "rm -rf .tole",
        "rm -f .git/config",
        // Replace by rename over the mount point.
        "sh -c 'mkdir h2 && mv h2 .git/hooks'",
        "sh -c 'echo x > c2 && mv c2 .git/config'",
        // The reviewer's chain: the rename is what used to succeed.
        "sh -c 'mv .git .g && cp -a .g .git && printf x > .git/hooks/pre-commit'",
        "sh -c 'echo evil > .git/hooks/pre-commit'",
        "sh -c 'echo [core] >> .git/config'",
        "sh -c 'echo x > .tole/evil'",
    ];
    for a in attacks {
        let out = run(&tool, a);
        assert_ne!(out["status"], 0, "must fail: {a}: {out}");
    }
    // Host state: nothing moved, planted or removed.
    assert!(w.join(".git").is_dir() && !w.join(".g").exists());
    assert!(w.join(".git/hooks").is_dir() && !w.join(".git/hooks.old").exists());
    assert_eq!(
        std::fs::read_to_string(w.join(".git/hooks/marker")).unwrap(),
        "owner\n"
    );
    assert!(!w.join(".git/hooks/pre-commit").exists());
    assert_eq!(
        std::fs::read_to_string(w.join(".git/config")).unwrap(),
        before
    );
    assert!(w.join(".tole").is_dir() && !w.join(".tole/evil").exists());
}

/// The anchor must not break git: objects, index and refs stay writable.
#[test]
fn git_still_works_inside_the_sandbox() {
    if skipped() {
        return;
    }
    let w = repo("gitworks");
    let tool = RunCommandTool::new(w.clone());
    let out = run(&tool, "sh -c 'echo a > f && git add f && git -c user.name=t -c user.email=t@t commit -q -m one && git status --short && git log --oneline'");
    assert_eq!(out["status"], 0, "{out}");
    assert!(text(&out, "stdout").contains("one"), "{out}");
    let host = std::process::Command::new("git")
        .args(["-C"])
        .arg(&w)
        .args(["log", "--oneline"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&host.stdout).contains("one"));
    // The hooks and the config are still the owner's.
    assert!(w.join(".git/hooks/marker").exists());
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
