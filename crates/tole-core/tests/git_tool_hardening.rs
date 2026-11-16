//! #748: the `git` tool runs on the HOST, outside the command sandbox, where
//! `.git` is read-only for a sandboxed command, so it is how a model commits.
//!
//! The rule under test: with a command consumer on, NO host-side code execution
//! is reachable from state a sandboxed command can write, whenever and by
//! whomever the repository was created. So:
//!
//! * hooks are off for every op (the owner's own pre-commit does NOT run for a
//!   commit made through the tool);
//! * before an op the tool asks git which git directory it uses and reads that
//!   directory's config without running anything: a key that can name a program
//!   or a file (`filter.*`, `include.*`, `core.hooksPath`, `gpg.*`, ...) makes it
//!   refuse; a bare, linked-worktree or otherwise non-plain layout is refused;
//! * a repository with only inert config works (status, add, diff, commit).
//!
//! Host-side proof: marker files the scripts would create. No bubblewrap is
//! needed, the tool is a host process. The consumer settings are process-wide
//! and set once here (`git_tool_default.rs` is the no-consumer side). The tests
//! are environment independent: no `set_current_dir`, no `set_var`, the git
//! processes get `HOME`/`GIT_CONFIG_*` explicitly, and none of them assumes the
//! test directory has no enclosing repository.

#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use test_dir::TestDir;
use tole_core::integrations::git::GitTool;
use tole_core::sandbox::command::{self, Consumers};
use tole_core::sandbox::Mode;
use tole_core::tool::Tool;

fn host_git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join(".xdg"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git on PATH");
    assert!(o.status.success(), "git {args:?}: {o:?}");
}

fn tool_in(dir: &Path) -> GitTool {
    GitTool::new()
        .in_dir(dir)
        .with_env("HOME", dir.display().to_string())
        .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
}

fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn consumers_on() {
    command::set_consumers(Consumers {
        run_command: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
}

/// A fresh repository (`git init`, a user) at `<tmp>/ws`.
fn init_repo(dir: &TestDir) -> PathBuf {
    let ws = std::fs::canonicalize(&**dir).unwrap().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    host_git(&ws, &["init", "-q", "-b", "main"]);
    host_git(&ws, &["config", "user.name", "t"]);
    host_git(&ws, &["config", "user.email", "t@t"]);
    ws
}

fn marker(ws: &Path, name: &str) -> PathBuf {
    ws.parent().unwrap().join(format!("marker-{name}"))
}

const ALL_OPS: fn() -> [serde_json::Value; 4] = || {
    [
        json!({"op": "status"}),
        json!({"op": "diff"}),
        json!({"op": "add", "paths": ["f.txt"]}),
        json!({"op": "commit", "message": "x"}),
    ]
};

fn no_commit(ws: &Path) -> bool {
    !Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(ws)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap()
        .status
        .success()
}

/// The owner's repository with only inert config works end to end, and the
/// owner's own pre-commit hook does NOT run for the commit (hooks are off while
/// a command consumer is on; a human's own `git commit` would still run it).
#[test]
fn an_owner_repository_with_inert_config_works_and_runs_no_hook() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-owner");
    let ws = init_repo(&dir);
    // `--no-verify` skips only pre-commit and commit-msg: the other hooks a
    // commit and an add would run must be off through `core.hooksPath` too.
    let names = [
        "pre-commit",
        "commit-msg",
        "prepare-commit-msg",
        "post-commit",
        "reference-transaction",
        "post-index-change",
    ];
    let markers: Vec<PathBuf> = names.iter().map(|n| marker(&ws, n)).collect();
    for (n, m) in names.iter().zip(&markers) {
        script(
            &ws.join(".git/hooks").join(n),
            &format!("touch {}", m.display()),
        );
    }
    std::fs::write(ws.join("f.txt"), "one\n").unwrap();
    let tool = tool_in(&ws);
    tool.execute(json!({"op": "status"})).unwrap();
    tool.execute(json!({"op": "add", "paths": ["f.txt"]}))
        .unwrap();
    tool.execute(json!({"op": "diff", "staged": true})).unwrap();
    tool.execute(json!({"op": "commit", "message": "add f"}))
        .expect("the commit succeeds");
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&ws)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("add f"));
    for (n, m) in names.iter().zip(&markers) {
        assert!(!m.exists(), "the owner's {n} hook must not run");
    }
}

/// Review repro 1: a repository planted in a workspace that had none, read by a
/// tool built AFTER the plant (a new session, a cron run, a child). Refused with
/// the config-key message; nothing the planted repository names fires.
#[test]
fn a_planted_repository_is_refused_by_a_tool_built_after_the_plant() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-planted");
    let ws = init_repo(&dir);
    let (hook, filter, gpg) = (
        marker(&ws, "hook"),
        marker(&ws, "filter"),
        marker(&ws, "gpg"),
    );
    script(
        &ws.join(".git/hooks/pre-commit"),
        &format!("touch {}", hook.display()),
    );
    script(
        &ws.join("clean.sh"),
        &format!("touch {}\ncat", filter.display()),
    );
    host_git(
        &ws,
        &[
            "config",
            "filter.x.clean",
            ws.join("clean.sh").to_str().unwrap(),
        ],
    );
    std::fs::write(ws.join(".gitattributes"), "* filter=x\n").unwrap();
    script(
        &ws.join("gpg.sh"),
        &format!("touch {}\nexit 1", gpg.display()),
    );
    host_git(
        &ws,
        &["config", "gpg.program", ws.join("gpg.sh").to_str().unwrap()],
    );
    host_git(&ws, &["config", "commit.gpgsign", "true"]);
    std::fs::write(ws.join("f.txt"), "one\n").unwrap();
    // Built after the plant: no snapshot could distinguish it from the owner's.
    let tool = tool_in(&ws);
    for input in ALL_OPS() {
        let e = tool.execute(input.clone()).unwrap_err();
        assert!(
            e.starts_with("git: refused: ")
                && e.contains("`filter.x.clean`")
                && e.contains("run git yourself outside the tool, or remove the key"),
            "{input}: {e}"
        );
    }
    for m in [&hook, &filter, &gpg] {
        assert!(!m.exists(), "{} fired on the host", m.display());
    }
    assert!(no_commit(&ws), "nothing was committed");
}

/// Review repro 2: a bare layout planted in an ANCESTOR between the workdir and a
/// real `.git` (git finds the bare one first). Refused, nothing fires.
#[test]
fn a_bare_layout_in_an_ancestor_is_refused() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-bare-ancestor");
    let ws = init_repo(&dir);
    let fired = marker(&ws, "bare-filter");
    script(
        &ws.join("clean.sh"),
        &format!("touch {}\ncat", fired.display()),
    );
    let mid = ws.join("mid");
    std::fs::create_dir_all(mid.join("objects")).unwrap();
    std::fs::create_dir_all(mid.join("refs")).unwrap();
    std::fs::write(mid.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        mid.join("config"),
        format!(
            "[core]\n\tbare = true\n[filter \"x\"]\n\tclean = {}\n",
            ws.join("clean.sh").display()
        ),
    )
    .unwrap();
    let deep = mid.join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("f.txt"), "one\n").unwrap();
    std::fs::write(deep.join(".gitattributes"), "* filter=x\n").unwrap();
    let tool = tool_in(&deep);
    for input in ALL_OPS() {
        let e = tool.execute(input.clone()).unwrap_err();
        assert!(e.starts_with("git: refused: "), "{input}: {e}");
    }
    assert!(!fired.exists(), "the bare config's filter fired");
}

/// The workdir itself a bare repository.
#[test]
fn a_workdir_that_is_a_bare_repository_is_refused() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-bare");
    let ws = init_repo(&dir);
    let bare = ws.join("sub");
    std::fs::create_dir_all(bare.join("objects")).unwrap();
    std::fs::create_dir_all(bare.join("refs")).unwrap();
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(bare.join("config"), "[core]\n\tbare = true\n").unwrap();
    let tool = tool_in(&bare);
    for input in ALL_OPS() {
        let e = tool.execute(input.clone()).unwrap_err();
        assert!(
            e.starts_with("git: refused: ") && e.contains("only runs in a plain repository"),
            "{input}: {e}"
        );
    }
}

/// A linked worktree (`.git` is a file) is a layout the tool does not vet.
#[test]
fn a_linked_worktree_is_refused() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-worktree");
    let ws = init_repo(&dir);
    std::fs::write(ws.join("f.txt"), "one\n").unwrap();
    host_git(&ws, &["add", "f.txt"]);
    host_git(&ws, &["commit", "-q", "-m", "init"]);
    let wt = ws.parent().unwrap().join("wt");
    host_git(&ws, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let e = tool_in(&wt).execute(json!({"op": "status"})).unwrap_err();
    assert!(
        e.starts_with("git: refused: ") && e.contains("linked worktree"),
        "{e}"
    );
}

/// Keys that name a program or a file make the tool refuse, naming the key.
/// Nothing fires.
#[test]
fn config_keys_that_name_a_program_or_a_file_are_refused() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-keys");
    let ws = init_repo(&dir);
    let fired = marker(&ws, "keys");
    script(
        &ws.join("p.sh"),
        &format!("touch {}\nexit 1", fired.display()),
    );
    let p = ws.join("p.sh").display().to_string();
    // include.path points at a workspace file that defines a clean filter.
    std::fs::write(
        ws.join("extra.cfg"),
        format!("[filter \"y\"]\n\tclean = {p}\n"),
    )
    .unwrap();
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "include.path",
            ws.join("extra.cfg").display().to_string(),
            "include.path",
        ),
        ("filter.z.clean", p.clone(), "filter.z.clean"),
        ("core.fsmonitor", p.clone(), "core.fsmonitor"),
        (
            "core.hooksPath",
            "hooks-in-workspace".into(),
            "core.hookspath",
        ),
        ("core.sshCommand", p.clone(), "core.sshcommand"),
        ("core.pager", p.clone(), "core.pager"),
        ("core.worktree", "..".into(), "core.worktree"),
        ("gpg.program", p.clone(), "gpg.program"),
        ("commit.gpgsign", "true".into(), "commit.gpgsign"),
        ("alias.st", format!("!{p}"), "alias.st"),
        ("diff.external", p.clone(), "diff.external"),
        ("diff.q.textconv", p.clone(), "diff.q.textconv"),
        ("merge.m.driver", p.clone(), "merge.m.driver"),
        ("credential.helper", p.clone(), "credential.helper"),
        (
            "extensions.worktreeConfig",
            "true".into(),
            "extensions.worktreeconfig",
        ),
        ("submodule.s.update", format!("!{p}"), "submodule.s.update"),
    ];
    std::fs::write(ws.join("f.txt"), "one\n").unwrap();
    for (key, value, shown) in cases {
        host_git(&ws, &["config", key, &value]);
        let tool = tool_in(&ws);
        for input in [
            json!({"op": "status"}),
            json!({"op": "commit", "message": "x"}),
        ] {
            let e = tool.execute(input.clone()).unwrap_err();
            let needle = format!("`{shown}`");
            assert!(
                e.starts_with("git: refused: ") && e.contains(&needle),
                "{key} {input}: {e}"
            );
        }
        host_git(&ws, &["config", "--unset-all", key]);
        // Back to an inert config: it works again.
        tool_in(&ws).execute(json!({"op": "status"})).unwrap();
    }
    assert!(!fired.exists(), "something named by the config fired");
}

/// A nested repository with its own hostile config and attributes, registered as
/// a submodule: status and diff never descend into it, so its filter never runs.
#[test]
fn a_nested_repository_is_never_run_in() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-nested");
    let ws = init_repo(&dir);
    let fired = marker(&ws, "nested-filter");
    let inner = ws.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    host_git(&inner, &["init", "-q", "-b", "main"]);
    host_git(&inner, &["config", "user.name", "t"]);
    host_git(&inner, &["config", "user.email", "t@t"]);
    std::fs::write(inner.join("a.txt"), "a\n").unwrap();
    host_git(&inner, &["add", "a.txt"]);
    host_git(&inner, &["commit", "-q", "-m", "inner"]);
    std::fs::write(
        ws.join(".gitmodules"),
        "[submodule \"inner\"]\n\tpath = inner\n\turl = ./inner\n\tignore = none\n",
    )
    .unwrap();
    host_git(&ws, &["add", ".gitmodules", "inner"]);
    host_git(&ws, &["commit", "-q", "-m", "outer"]);
    // The inner repository turns hostile after the owner's setup.
    script(
        &ws.join("clean.sh"),
        &format!("touch {}\ncat", fired.display()),
    );
    host_git(
        &inner,
        &[
            "config",
            "filter.x.clean",
            ws.join("clean.sh").to_str().unwrap(),
        ],
    );
    std::fs::write(inner.join(".gitattributes"), "* filter=x\n").unwrap();
    // Same size, new content: git must hash it, which runs a clean filter.
    std::fs::write(inner.join("a.txt"), "b\n").unwrap();
    std::fs::write(ws.join("g.txt"), "g\n").unwrap();
    let tool = tool_in(&ws);
    tool.execute(json!({"op": "status"})).unwrap();
    tool.execute(json!({"op": "diff"})).unwrap();
    // `git add` would run git inside the dirty gitlink (no option stops it):
    // refused, with the way out. Commit does not descend.
    let e = tool
        .execute(json!({"op": "add", "paths": ["."]}))
        .unwrap_err();
    assert!(
        e.starts_with("git: refused: ") && e.contains("submodule entry (`inner`)"),
        "{e}"
    );
    host_git(&ws, &["add", "--", "g.txt"]);
    tool.execute(json!({"op": "commit", "message": "outer again"}))
        .unwrap();
    assert!(
        !fired.exists(),
        "the nested repository's filter fired on the host"
    );
}

/// The model is told where the way out is: `run_command`'s summary (its
/// consumer is on here) names the `git` tool; `job_start`'s consumer is off, so
/// its summary does not. The workdir is a test directory, not TMPDIR.
#[test]
fn the_run_command_summary_points_at_the_git_tool_when_its_consumer_is_on() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-summary");
    let w = std::fs::canonicalize(&*dir).unwrap();
    let s = tole_core::run_command::RunCommandTool::new(w.clone()).summary();
    assert!(
        s.contains(".git directory is read-only") && s.contains("use the git tool"),
        "{s}"
    );
    let j = tole_core::jobs::JobStartTool::new(w).summary();
    assert!(!j.contains("git tool"), "{j}");
}

/// The directory the child probe below runs the production tool in.
const CHILD_DIR: &str = "TOLE_GIT_ENV_CHILD_DIR";

/// Runs only as a child process of
/// `a_hostile_process_environment_never_reaches_git` (it is `#[ignore]`d, so a
/// normal run skips it): the PRODUCTION `GitTool::new()` path, no `with_env`,
/// under whatever environment the parent gave this process. Its results are
/// ignored; the parent checks the markers.
#[test]
#[ignore = "child of a_hostile_process_environment_never_reaches_git"]
fn env_child_probe() {
    let Some(ws) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    consumers_on();
    let tool = GitTool::new().in_dir(PathBuf::from(ws));
    for op in ALL_OPS() {
        let _ = tool.execute(op);
    }
}

/// A tole process started by a sandboxed command inherits whatever that command
/// put in its environment. Every variable that relocates git's config (or names
/// a program) must be dropped for the tool, which runs on the host: no marker
/// may appear, whatever the variable. The hostile environment is set on the
/// child's `Command`, never on this process (no `set_var`).
#[test]
fn a_hostile_process_environment_never_reaches_git() {
    consumers_on();
    let dir = TestDir::new("tole-gittool-hostile-env");
    let ws = init_repo(&dir);
    std::fs::write(ws.join("f.txt"), "f\n").unwrap();
    std::fs::write(ws.join(".gitattributes"), "* filter=x\n").unwrap();
    let hostile = ws.parent().unwrap().join("hostile");
    let clean = |name: &str| {
        let m = marker(&ws, name);
        let s = hostile.join(format!("{name}.sh"));
        script(&s, &format!("touch {}\ncat", m.display()));
        (m, s.display().to_string())
    };
    let cfg = |script: &str| format!("[filter \"x\"]\n\tclean = {script}\n");
    let write = |p: PathBuf, body: String| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    };
    // (vector name, environment of the child, marker that must not appear)
    type Vector<'a> = (&'a str, Vec<(String, String)>, PathBuf);
    let mut vectors: Vec<Vector> = Vec::new();

    let (m, s) = clean("global");
    let p = write(hostile.join("global.cfg"), cfg(&s));
    vectors.push((
        "GIT_CONFIG_GLOBAL",
        vec![("GIT_CONFIG_GLOBAL".into(), p.display().to_string())],
        m,
    ));
    let (m, s) = clean("legacy");
    let p = write(hostile.join("legacy.cfg"), cfg(&s));
    vectors.push((
        "GIT_CONFIG",
        vec![("GIT_CONFIG".into(), p.display().to_string())],
        m,
    ));
    let (m, s) = clean("system");
    let p = write(hostile.join("system.cfg"), cfg(&s));
    vectors.push((
        "GIT_CONFIG_SYSTEM",
        vec![
            ("GIT_CONFIG_SYSTEM".into(), p.display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".into(), "0".into()),
        ],
        m,
    ));
    let (m, s) = clean("count");
    vectors.push((
        "GIT_CONFIG_COUNT/KEY_0/VALUE_0 (filter)",
        vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "filter.x.clean".into()),
            ("GIT_CONFIG_VALUE_0".into(), s),
        ],
        m,
    ));
    let (m, s) = clean("fsmonitor");
    vectors.push((
        "GIT_CONFIG_COUNT/KEY_0/VALUE_0 (core.fsmonitor)",
        vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.fsmonitor".into()),
            ("GIT_CONFIG_VALUE_0".into(), s),
        ],
        m,
    ));
    let (m, s) = clean("params");
    vectors.push((
        "GIT_CONFIG_PARAMETERS",
        vec![(
            "GIT_CONFIG_PARAMETERS".into(),
            format!("'filter.x.clean'='{s}'"),
        )],
        m,
    ));
    let (m, s) = clean("home");
    let home = hostile.join("home");
    write(home.join(".gitconfig"), cfg(&s));
    vectors.push(("HOME", vec![("HOME".into(), home.display().to_string())], m));
    let (m, s) = clean("xdg");
    let xdg = hostile.join("xdg");
    write(xdg.join("git").join("config"), cfg(&s));
    vectors.push((
        "XDG_CONFIG_HOME",
        vec![
            ("XDG_CONFIG_HOME".into(), xdg.display().to_string()),
            ("HOME".into(), hostile.join("nohome").display().to_string()),
        ],
        m,
    ));

    for (i, (name, env, m)) in vectors.into_iter().enumerate() {
        // New content of a new size each round: git must hash it again, which is
        // what runs a clean filter.
        std::fs::write(ws.join("f.txt"), "f".repeat(i + 2)).unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "env_child_probe",
                "--exact",
                "--ignored",
                "--test-threads=1",
            ])
            .env(CHILD_DIR, &ws)
            .envs(env)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && text.contains("1 passed"),
            "{name}: the child probe did not run: {text} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !m.exists(),
            "{name}: something named by the hostile environment fired on the host"
        );
    }
}
