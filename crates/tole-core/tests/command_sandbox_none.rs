//! #748: the policy of `[sandbox.consumers]` when there is NO working
//! sandbox, end to end through the real tools. A PATH without `bwrap` makes
//! the backend unavailable (this is also what macOS and Windows get), so this
//! file needs no bubblewrap and runs everywhere unix. `run_command` is
//! `optional`, `job_start` is `required`.

#![cfg(unix)]

use serde_json::json;
use std::path::PathBuf;
use tole_core::jobs::JobStartTool;
use tole_core::run_command::RunCommandTool;
use tole_core::sandbox::command::{self, Consumers};
use tole_core::sandbox::{self, Availability, Mode};
use tole_core::tool::Tool;

#[test]
fn without_a_backend_required_refuses_and_optional_depends_on_who_is_watching() {
    // A PATH that has the programs this test runs but no bwrap.
    let base = std::env::temp_dir().join(format!("tole-cmd-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (bin, ws) = (base.join("bin"), base.join("ws"));
    for d in [&bin, &ws] {
        std::fs::create_dir_all(d).unwrap();
    }
    let ws = std::fs::canonicalize(&ws).unwrap();
    for p in ["echo", "sh"] {
        let src = ["/bin", "/usr/bin"]
            .iter()
            .map(|d| PathBuf::from(d).join(p))
            .find(|c| c.exists())
            .unwrap();
        std::os::unix::fs::symlink(src, bin.join(p)).unwrap();
    }
    std::env::set_var("PATH", &bin);
    command::set_consumers(Consumers {
        run_command: Mode::Optional,
        job_start: Mode::Required,
        ..Consumers::default()
    })
    .unwrap();
    assert!(matches!(sandbox::detect(), Availability::Unavailable(_)));

    let run = RunCommandTool::new(ws.clone());
    let job = JobStartTool::new(ws.clone());

    // Attended: optional runs the command as before (with a warning on
    // stderr, once); required refuses with the remedy and leaves no job.
    let out = run.execute(json!({"command": "echo hello"})).unwrap();
    assert_eq!(
        out["stdout"], "hello\n",
        "an optional consumer ran unsandboxed"
    );
    assert_eq!(out["status"], 0);
    let e = job.execute(json!({"command": "echo hi"})).unwrap_err();
    assert!(e.starts_with("job_start: "), "{e}");
    assert!(e.contains("sandbox required but unavailable"), "{e}");
    assert!(
        e.contains("bubblewrap") && e.contains("sandbox.consumers.job_start"),
        "{e}"
    );
    assert!(
        !ws.join("tole-jobs").exists(),
        "a refused job leaves no job directory behind"
    );

    // Unattended (serve, cron): optional counts as required.
    sandbox::mark_unattended();
    let e = run.execute(json!({"command": "echo hello"})).unwrap_err();
    assert!(e.contains("unattended") && e.contains("no sandbox"), "{e}");
    assert!(job.execute(json!({"command": "echo hi"})).is_err());

    // The refusal is not an approval substitute either way: the tool is
    // still Write.
    assert_eq!(run.risk(), tole_core::tool::Risk::Write);
}
