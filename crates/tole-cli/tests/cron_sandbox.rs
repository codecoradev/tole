//! #748 slice 3: cron script jobs and `command:` deliveries under
//! `[sandbox.consumers] cron`, end to end with the REAL binary (`tole cron
//! create`, `tole cron run`) and a scrubbed environment.
//!
//! - `off` (the default): unchanged, the script runs as before.
//! - `optional` / `required` with no `bwrap`: the run (or the delivery) is a
//!   REFUSAL that is visible in the cron ledger and the run log; the script
//!   never runs. A cron job is unattended, so `optional` has no unsandboxed
//!   fallback. A launcher that fails or cannot be spawned is a refusal too,
//!   never a pass.
//! - with `bwrap`: the job's workdir is the workspace; the script and the
//!   delivery write it but not `.git`, `.tole`, and cannot read the masked
//!   user config directory. Skipped with a `SKIPPED:` line when `bwrap` does
//!   not work here (`TOLE_REQUIRE_BWRAP=1` makes that a failure, as CI's
//!   `Sandbox (bubblewrap)` job sets it).
#![cfg(all(unix, feature = "shell-tools"))]

mod common;
use common::{write_executable, TestDir};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tole_cli::cron;

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

struct Env {
    root: TestDir,
    home: PathBuf,
    cron: PathBuf,
    ws: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let root = TestDir::new(&format!("tole-cronsbx-{tag}"));
        let e = Env {
            home: root.join("home"),
            cron: root.join("cron"),
            ws: root.join("ws"),
            root,
        };
        for d in [
            e.ws.join(".git/hooks"),
            e.ws.join(".tole"),
            e.home.join("tole"),
            e.cron.clone(),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(e.ws.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(e.home.join("tole/secret.txt"), "TOPSECRET").unwrap();
        e
    }

    fn user_config(&self, toml: &str) {
        std::fs::write(self.home.join("tole/config.toml"), toml).unwrap();
    }

    fn tole(&self, path: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(args)
            .current_dir(&self.ws)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("CODECORA_HOME", &self.home)
            .env("TOLE_CRON_DIR", &self.cron)
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Creates job `j` (a script) and runs it once with `tole cron run`.
    fn run_job(&self, path: &str, script: &str, workdir: &Path, extra: &[&str]) -> Output {
        let workdir = workdir.display().to_string();
        let mut args = vec![
            "cron",
            "create",
            "@daily",
            "--id",
            "j",
            "--script",
            script,
            "--workdir",
            &workdir,
        ];
        args.extend_from_slice(extra);
        // `create` runs under the real PATH: every `tole` start probes the
        // backend when verify or turnend is only the default (#748), and a fake
        // that removes its own execute bit after its first probe must be
        // spent by the `run`, not by `create`.
        let real = std::env::var("PATH").unwrap_or_default();
        let o = self.tole(&real, &args);
        assert!(o.status.success(), "create: {}", text(&o.stderr));
        self.tole(path, &["cron", "run", "j"])
    }

    fn row(&self) -> cron::RunRow {
        let mut rows = cron::runs(&self.cron, Some("j")).unwrap();
        assert_eq!(rows.len(), 1, "one run recorded");
        rows.remove(0)
    }

    fn log(&self, row: &cron::RunRow) -> String {
        std::fs::read_to_string(cron::tick::output_path(&self.cron, &row.fire_id))
            .unwrap_or_default()
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.ws.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        write_executable(&p, body);
    }
}

fn usage(row: &cron::RunRow, key: &str) -> Value {
    row.usage
        .as_ref()
        .map(|u| u[key].clone())
        .unwrap_or_default()
}

fn status(row: &cron::RunRow) -> String {
    format!("{:?}", row.status.expect("settled"))
}

fn delivery(row: &cron::RunRow, target_prefix: &str) -> Value {
    row.deliveries
        .iter()
        .find(|d| {
            d["target"]
                .as_str()
                .is_some_and(|t| t.starts_with(target_prefix))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no {target_prefix} delivery in {:?}", row.deliveries))
}

fn real_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// A PATH with `sh`, `touch` and `cat` but no `bwrap`: no sandbox backend.
fn path_without_bwrap(dir: &Path) -> String {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for p in ["sh", "touch", "cat", "chmod"] {
        let src = ["/bin", "/usr/bin"]
            .iter()
            .map(|d| Path::new(d).join(p))
            .find(|c| c.exists())
            .unwrap();
        std::os::unix::fs::symlink(src, bin.join(p)).unwrap();
    }
    bin.display().to_string()
}

/// A PATH whose `bwrap` is a script: `--version` and the self-test (`... true`)
/// succeed, so the sandbox looks usable; then `body` runs for every real
/// launch. With `chmod_after_selftest` it removes its own execute bit once it
/// has passed the self-test, so a later launch cannot be spawned.
#[cfg(target_os = "linux")]
fn path_with_fake_bwrap(dir: &Path, body: &str, chmod_after_selftest: bool) -> String {
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let smoke = if chmod_after_selftest {
        "chmod a-x \"$0\""
    } else {
        ":"
    };
    let script = format!(
        "#!/bin/sh\n\
         case \"$1\" in --version) echo 'bubblewrap 0.9.0'; exit 0;; esac\n\
         for a in \"$@\"; do last=$a; done\n\
         if [ \"$last\" = true ]; then {smoke}; exit 0; fi\n\
         {body}\n"
    );
    write_executable(bin.join("bwrap"), &script);
    format!("{}:{}", bin.display(), path_without_bwrap(dir))
}

fn bwrap_works() -> bool {
    let ok = Command::new("bwrap")
        .args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--ro-bind",
            "/",
            "/",
            "true",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        assert!(
            std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
            "TOLE_REQUIRE_BWRAP is set but no sandbox works here"
        );
        eprintln!("SKIPPED: no working bwrap here");
    }
    ok
}

/// The script every protected-path test runs: writes the workdir, tries the
/// protected writes and the masked read, and prints what happened.
const PROBE: &str = "#!/bin/sh\n\
    echo made > made\n\
    (echo x > .git/hooks/pre-commit) 2>/dev/null && echo GIT_WRITTEN\n\
    (echo x >> .git/config) 2>/dev/null && echo GIT_CONFIG_WRITTEN\n\
    (echo x > .tole/evil) 2>/dev/null && echo TOLE_WRITTEN\n\
    if cat \"$SECRET\" 2>/dev/null; then echo SECRET_READ; fi\n\
    echo probe-done\n";

fn probe_script(secret: &Path) -> String {
    PROBE.replace("$SECRET", &secret.display().to_string())
}

#[test]
fn cron_off_is_unchanged_and_explicit_off_is_the_same() {
    for (tag, cfg) in [
        ("default", ""),
        ("off", "[sandbox.consumers]\ncron = \"off\"\n"),
    ] {
        let e = Env::new(&format!("off-{tag}"));
        e.user_config(cfg);
        e.write("job.sh", &probe_script(&e.home.join("tole/secret.txt")));
        let o = e.run_job(&real_path(), "sh job.sh", &e.ws, &[]);
        assert!(o.status.success(), "{}", text(&o.stderr));
        let row = e.row();
        assert_eq!(status(&row), "Ok", "{tag}");
        assert_eq!(usage(&row, "sandboxed"), Value::Bool(false), "{tag}");
        let log = e.log(&row);
        // Unsandboxed, as always: every write and the read work.
        for want in ["GIT_WRITTEN", "TOLE_WRITTEN", "SECRET_READ", "probe-done"] {
            assert!(log.contains(want), "{tag}: {want} in {log}");
        }
        assert!(e.ws.join("made").exists(), "{tag}");
    }
}

#[test]
fn a_cron_script_without_a_sandbox_is_refused_visibly_for_required_and_optional() {
    for mode in ["required", "optional"] {
        let e = Env::new(&format!("nosbx-{mode}"));
        e.user_config(&format!("[sandbox.consumers]\ncron = \"{mode}\"\n"));
        e.write("job.sh", "#!/bin/sh\ntouch ran-marker\n");
        let path = path_without_bwrap(&e.root);
        let o = e.run_job(&path, "sh job.sh", &e.ws, &[]);
        assert_eq!(text(&o.stderr), "", "{mode}: nothing for MAILTO to mail");
        let row = e.row();
        // Visible: an `error` run, the reason in the log, `usage` and `runs`.
        assert_eq!(status(&row), "Error", "{mode}");
        assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(true), "{mode}");
        let err = usage(&row, "error").as_str().unwrap_or("").to_string();
        assert!(
            err.starts_with("cron: ") && err.contains("sandbox.consumers.cron"),
            "{mode}: {err}"
        );
        assert!(e.log(&row).contains(&err), "{mode}: the run log says why");
        assert!(
            !e.ws.join("ran-marker").exists(),
            "{mode}: the script never ran, not even unsandboxed"
        );
        let runs = text(&e.tole(&path, &["cron", "runs"]).stdout);
        assert!(runs.contains("error"), "{mode}: {runs}");
    }
}

/// A delivery that cannot be sandboxed never runs: the delivery record says
/// `failed` with the reason, and the command is not started. (The script is
/// refused too, here; its refusal text is what is delivered.)
#[test]
fn a_command_delivery_without_a_sandbox_is_refused_and_recorded() {
    let e = Env::new("deliver-nosbx");
    e.user_config("[sandbox.consumers]\ncron = \"optional\"\n");
    let path = path_without_bwrap(&e.root);
    let o = e.run_job(
        &path,
        "sh -c 'echo payload'",
        &e.ws,
        &["--deliver", "command:touch delivered-marker"],
    );
    assert_eq!(text(&o.stderr), "");
    let row = e.row();
    let d = delivery(&row, "command:");
    assert_eq!(d["status"], "failed", "{d}");
    let err = d["error"].as_str().unwrap_or("");
    assert!(
        err.starts_with("cron: ") && err.contains("sandbox"),
        "{err}"
    );
    assert!(
        !e.ws.join("delivered-marker").exists(),
        "the delivery command never ran unsandboxed"
    );
}

#[test]
fn a_sandboxed_cron_script_writes_its_workdir_but_not_git_or_tole_and_cannot_read_the_config_dir() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("probe");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", &probe_script(&e.home.join("tole/secret.txt")));
    let o = e.run_job(&real_path(), "sh job.sh", &e.ws, &[]);
    assert_eq!(text(&o.stderr), "", "{}", text(&o.stdout));
    let row = e.row();
    let log = e.log(&row);
    assert_eq!(status(&row), "Ok", "{log}");
    assert_eq!(usage(&row, "sandboxed"), Value::Bool(true));
    assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(false));
    assert_eq!(usage(&row, "exit_code"), 0);
    assert!(log.contains("probe-done"), "stdout is captured: {log}");
    for leak in [
        "GIT_WRITTEN",
        "GIT_CONFIG_WRITTEN",
        "TOLE_WRITTEN",
        "SECRET_READ",
    ] {
        assert!(!log.contains(leak), "{leak} must not happen: {log}");
    }
    assert!(
        e.ws.join("made").exists(),
        "the workdir write reached the host"
    );
    assert!(!e.ws.join(".git/hooks/pre-commit").exists());
    assert!(!e.ws.join(".tole/evil").exists());
    assert_eq!(
        std::fs::read_to_string(e.ws.join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[test]
fn a_failing_sandboxed_script_keeps_its_own_exit_code_and_is_not_a_refusal() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("exit3");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", "#!/bin/sh\necho partial\nexit 3\n");
    e.run_job(&real_path(), "sh job.sh", &e.ws, &[]);
    let row = e.row();
    assert_eq!(status(&row), "Error");
    assert_eq!(usage(&row, "exit_code"), 3, "the command's own code");
    assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(false));
    assert!(e.log(&row).contains("partial"));
}

/// 125, 126 and 127 are the script's own verdict when `bwrap` reported them as
/// the command's exit code (a missing program inside is 127).
#[test]
fn a_sandboxed_script_exiting_127_is_its_own_failure() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("exit127");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", "#!/bin/sh\nexit 127\n");
    e.run_job(&real_path(), "sh job.sh", &e.ws, &[]);
    let row = e.row();
    assert_eq!(status(&row), "Error");
    assert_eq!(usage(&row, "exit_code"), 127);
    assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(false));
}

/// A workdir below a repository root: the sandbox binds that directory only,
/// so the repository's `.git` above it is not in the sandbox at all.
#[test]
fn a_workdir_below_a_repository_root_sees_only_itself() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("subdir");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    let sub = e.ws.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    write_executable(
        sub.join("job.sh"),
        "#!/bin/sh\necho in-sub > made\nif [ -e ../.git ]; then echo PARENT_VISIBLE; fi\n\
         (echo x > ../escaped) 2>/dev/null && echo ESCAPED\necho done\n",
    );
    e.run_job(&real_path(), "sh job.sh", &sub, &[]);
    let row = e.row();
    let log = e.log(&row);
    assert_eq!(status(&row), "Ok", "{log}");
    // `..` inside the sandbox is its own tmpfs: a write there is lost with it.
    assert!(
        log.contains("done") && !log.contains("PARENT_VISIBLE"),
        "{log}"
    );
    assert!(sub.join("made").exists());
    assert!(!e.ws.join("escaped").exists());
}

#[test]
fn a_workdir_that_does_not_exist_or_holds_the_config_dir_is_refused() {
    let e = Env::new("badworkdir");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    // The refusal does not depend on the backend: it is the grant check, and
    // with no backend the answer is a refusal too.
    // `cron create` only takes an existing directory; this one goes away later.
    let gone = e.root.join("goes-away");
    std::fs::create_dir_all(&gone).unwrap();
    for (tag, dir) in [
        ("missing", gone.clone()),
        ("holds-data-dir", e.home.clone()),
    ] {
        let job = format!("j-{tag}");
        let dir = dir.display().to_string();
        let o = e.tole(
            &real_path(),
            &[
                "cron",
                "create",
                "@daily",
                "--id",
                &job,
                "--script",
                "true",
                "--workdir",
                &dir,
            ],
        );
        assert!(o.status.success(), "{}", text(&o.stderr));
        if tag == "missing" {
            std::fs::remove_dir_all(&gone).unwrap();
        }
        e.tole(&real_path(), &["cron", "run", &job]);
        let rows = cron::runs(&e.cron, Some(&job)).unwrap();
        assert_eq!(rows.len(), 1, "{tag}");
        assert_eq!(format!("{:?}", rows[0].status.unwrap()), "Error", "{tag}");
        let u = rows[0].usage.clone().unwrap();
        assert_eq!(u["sandbox_refused"], Value::Bool(true), "{tag}: {u}");
        assert!(
            u["error"].as_str().unwrap_or("").starts_with("cron: "),
            "{tag}: {u}"
        );
    }
}

#[test]
fn a_workspace_layout_the_profile_refuses_refuses_the_job() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("layout");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    std::fs::write(
        e.ws.join(".gitmodules"),
        "[submodule \"s\"]\n\tpath = s\n\turl = ../s\n",
    )
    .unwrap();
    e.write("job.sh", "#!/bin/sh\ntouch ran-marker\n");
    e.run_job(&real_path(), "sh job.sh", &e.ws, &[]);
    let row = e.row();
    assert_eq!(status(&row), "Error");
    assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(true));
    assert!(
        usage(&row, "error")
            .as_str()
            .unwrap()
            .contains("not initialized"),
        "{}",
        usage(&row, "error")
    );
    assert!(!e.ws.join("ran-marker").exists());
}

/// A launcher that fails (its own error, exit 1, no exit code of a command) is
/// the sandbox's failure: a refusal, never a script failure and never a pass.
// Linux only: a fake `bwrap` is used only where the backend is detected, so
// elsewhere there is no launcher to fail (#1171).
#[cfg(target_os = "linux")]
#[test]
fn a_launcher_failure_is_a_refusal_not_a_script_failure() {
    let e = Env::new("launcher");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", "#!/bin/sh\ntouch ran-marker\n");
    let path = path_with_fake_bwrap(
        &e.root,
        "echo 'bwrap: setting up uid map: Permission denied' >&2; exit 1",
        false,
    );
    e.run_job(&path, "sh job.sh", &e.ws, &[]);
    let row = e.row();
    assert_eq!(status(&row), "Error");
    assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(true));
    let err = usage(&row, "error").as_str().unwrap().to_string();
    assert!(
        err.contains("the sandbox failed before or while starting") && err.contains("uid map"),
        "{err}"
    );
    assert!(!e.ws.join("ran-marker").exists());
}

// Linux only: a fake `bwrap` is used only where the backend is detected, so
// elsewhere there is no launcher to fail (#1171).
#[cfg(target_os = "linux")]
#[test]
fn a_launcher_that_cannot_be_spawned_is_a_refusal() {
    let e = Env::new("spawn");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", "#!/bin/sh\ntouch ran-marker\n");
    let path = path_with_fake_bwrap(&e.root, "exit 1", true);
    e.run_job(&path, "sh job.sh", &e.ws, &[]);
    let row = e.row();
    assert_eq!(status(&row), "Error");
    assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(true));
    assert!(
        usage(&row, "error")
            .as_str()
            .unwrap()
            .contains("the sandbox did not start"),
        "{}",
        usage(&row, "error")
    );
}

/// A sandboxed `command:` delivery gets the run's output on stdin and the
/// `TOLE_CRON_*` variables, writes the workdir only, and its exit code lands
/// in the delivery record: a failing one is `failed` with its own stderr, not
/// a refusal.
#[test]
fn a_sandboxed_delivery_gets_stdin_and_env_and_its_failure_is_recorded() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("deliver");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", "#!/bin/sh\necho payload\n");
    e.write(
        "deliver-ok",
        "#!/bin/sh\ncat > got.txt\necho \"$TOLE_CRON_JOB $TOLE_CRON_STATUS\" >> got.txt\n",
    );
    e.write(
        "deliver-bad",
        "#!/bin/sh\necho 'no write for you' >&2\n(echo x > .git/hooks/post-commit) || exit 7\n",
    );
    e.run_job(
        &real_path(),
        "sh job.sh",
        &e.ws,
        &[
            "--deliver",
            "command:sh deliver-ok",
            "--deliver",
            "command:sh deliver-bad",
        ],
    );
    let row = e.row();
    assert_eq!(status(&row), "Ok", "{}", e.log(&row));
    let got = std::fs::read_to_string(e.ws.join("got.txt")).unwrap();
    assert_eq!(got, "payload\nj ok\n");
    assert_eq!(delivery(&row, "command:sh deliver-ok")["status"], "sent");
    let bad = delivery(&row, "command:sh deliver-bad");
    assert_eq!(bad["status"], "failed", "{bad}");
    let err = bad["error"].as_str().unwrap();
    assert!(
        err.contains("exit Some(7)") && err.contains("no write for you"),
        "its own exit code and stderr, not a refusal: {err}"
    );
    assert!(!e.ws.join(".git/hooks/post-commit").exists());
}

/// The timeout kills a sandboxed script (and its children, which die with the
/// launcher) and the run is an error that says so, not a refusal.
#[test]
fn a_sandboxed_script_is_killed_at_its_timeout() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("timeout");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", "#!/bin/sh\nsleep 30\ntouch after-sleep\n");
    let t = std::time::Instant::now();
    e.run_job(&real_path(), "sh job.sh", &e.ws, &["--timeout", "1"]);
    assert!(t.elapsed().as_secs() < 20, "killed, not waited for");
    let row = e.row();
    assert_eq!(status(&row), "Error");
    assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(false));
    assert!(
        usage(&row, "error").as_str().unwrap().contains("timed out"),
        "{}",
        usage(&row, "error")
    );
    assert!(!e.ws.join("after-sleep").exists());
}

/// The cron dir holds the ledger and the run logs that decide a job's future
/// definition and deliveries. `TOLE_CRON_DIR` may be anywhere, so a workdir
/// that holds the cron dir (or is inside it) would let a sandboxed script
/// forge job lines: refused, for a script and for a delivery, whatever the
/// backend. The default layout (the cron dir apart from the workdir) runs.
#[test]
fn a_workdir_that_overlaps_the_cron_dir_is_refused_for_script_and_delivery() {
    for (tag, inside) in [("cron-in-workdir", true), ("workdir-in-cron", false)] {
        let mut e = Env::new(tag);
        e.cron = if inside {
            e.ws.join(".cron")
        } else {
            e.root.to_path_buf()
        };
        std::fs::create_dir_all(&e.cron).unwrap();
        e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
        e.write("job.sh", "#!/bin/sh\ntouch ran-marker\n");
        e.run_job(
            &real_path(),
            "sh job.sh",
            &e.ws,
            &["--deliver", "command:touch delivered-marker"],
        );
        let row = e.row();
        assert_eq!(status(&row), "Error", "{tag}");
        assert_eq!(usage(&row, "sandbox_refused"), Value::Bool(true), "{tag}");
        let err = usage(&row, "error").as_str().unwrap_or("").to_string();
        assert!(
            err.contains("overlap") && err.contains("cron dir"),
            "{tag}: {err}"
        );
        assert!(
            !e.ws.join("ran-marker").exists(),
            "{tag}: the script never ran"
        );
        let d = delivery(&row, "command:");
        assert_eq!(d["status"], "failed", "{tag}: {d}");
        assert!(
            d["error"].as_str().unwrap_or("").contains("overlap"),
            "{tag}: the delivery is refused the same way: {d}"
        );
        assert!(!e.ws.join("delivered-marker").exists(), "{tag}");
    }
}

#[test]
fn the_default_layout_with_the_cron_dir_apart_from_the_workdir_runs() {
    if !bwrap_works() {
        return;
    }
    let e = Env::new("apart");
    e.user_config("[sandbox.consumers]\ncron = \"required\"\n");
    e.write("job.sh", "#!/bin/sh\ntouch ran-marker\n");
    e.run_job(&real_path(), "sh job.sh", &e.ws, &[]);
    let row = e.row();
    assert_eq!(status(&row), "Ok", "{}", e.log(&row));
    assert!(e.ws.join("ran-marker").exists());
}

/// No path around the sandbox: nothing in `src/cron` builds a process of its
/// own. A script and a delivery both take their `Command` from
/// `launch::prepare`, which asks `command_in_dir(Consumer::Cron, ..)`.
#[test]
fn only_the_one_launcher_builds_a_process_in_cron() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cron");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(&path).unwrap();
        // Only the part before the unit tests counts.
        let code = src.split("#[cfg(test)]").next().unwrap();
        let spawns = ["Command::new(", "process::Command::new"]
            .iter()
            .any(|p| code.contains(p));
        assert!(
            !spawns,
            "{name} builds its own process: use launch::prepare"
        );
        seen += code.matches("launch::prepare(").count();
    }
    assert_eq!(seen, 2, "the script run and the delivery both use it");
}
