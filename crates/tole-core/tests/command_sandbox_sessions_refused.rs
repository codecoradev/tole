//! #866, the refusal side: a sessions directory the sandbox cannot protect
//! refuses the launch (the consumer is `required`), so nothing runs. A
//! directory deeper than a direct child of the workspace could be renamed away
//! through its parent and replaced by a planted copy that tole then writes
//! into; a mount cannot prevent that, so the layout is refused. The directory
//! is named through a link and a `..` (resolved by the operating system, not
//! collapsed as text).
//!
//! Needs a working `bwrap` (`SKIPPED:` line otherwise; CI's `Sandbox
//! (bubblewrap)` job sets `TOLE_REQUIRE_BWRAP=1`). Process-wide settings are
//! set once here, so this is its own test target.

#![cfg(all(unix, feature = "shell-tools"))]

use serde_json::json;
use std::path::PathBuf;
use std::sync::OnceLock;
use tole_core::run_command::RunCommandTool;
use tole_core::sandbox::command::{self, Consumers, Limits};
use tole_core::sandbox::{self, Availability, Mode, Network};
use tole_core::tool::Tool;

fn ws() -> &'static PathBuf {
    static S: OnceLock<PathBuf> = OnceLock::new();
    S.get_or_init(|| {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shared-sessbx-refused");
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(ws.join("a/sessions")).unwrap();
        std::fs::create_dir_all(ws.join("a/b")).unwrap();
        std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        std::env::set_var("HOME", ws.parent().unwrap());
        std::env::remove_var("CODECORA_HOME");
        sandbox::command::set_consumers(Consumers {
            run_command: Mode::Required,
            job_start: Mode::Off,
            verify: Mode::Off,
            turnend: Mode::Off,
            cron: Mode::Off,
            network: Network::None,
            limits: Limits {
                cpu_secs: 120,
                memory_mb: 4096,
                processes: 512,
                file_size_mb: 64,
            },
        })
        .unwrap();
        // `-s base/out/lnk/../sessions` with lnk -> ws/a/b: the operating system
        // resolves it to ws/a/sessions (the `..` after a link is the link
        // target's parent), which is deeper than a direct child. Collapsing the
        // `..` as text named `base/out/sessions` and protected nothing.
        let out = ws.parent().unwrap().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(ws.join("a/b"), out.join("lnk")).unwrap();
        command::set_sessions_dir(out.join("lnk/../sessions")).unwrap();
        ws
    })
}

#[test]
fn a_sessions_dir_deeper_than_a_direct_child_refuses_the_launch_and_nothing_runs() {
    let ws = ws();
    if let Availability::Unavailable(why) = sandbox::detect() {
        assert!(
            std::env::var("TOLE_REQUIRE_BWRAP").is_err(),
            "TOLE_REQUIRE_BWRAP is set but no sandbox works: {why}"
        );
        eprintln!("SKIPPED: no working sandbox here: {why}");
        return;
    }
    let marker = ws.join("ran-marker");
    let e = RunCommandTool::new(ws.clone())
        .execute(json!({ "command": format!("touch {}", marker.display()) }))
        .unwrap_err();
    assert!(
        e.starts_with("run_command: ")
            && e.contains("the sessions directory")
            && e.contains("deeper than a direct child")
            && e.contains(".tole/sessions"),
        "{e}"
    );
    assert!(!marker.exists(), "the command never ran");
}
