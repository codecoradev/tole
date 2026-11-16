//! #866: a sessions directory the owner moved INTO the workspace, outside
//! `.tole` (`-s ./sessions`, the `sessions_dir` key), is part of the writable
//! workspace in the `command` profile unless it is protected: a sandboxed
//! command could rewrite or delete past entries, which undermines the
//! append-only claim and the resume/replay evidence. It is bound read-only like
//! `.git` and `.tole`, while tole itself (the host process, outside the
//! sandbox) keeps writing sessions.
//!
//! Needs a working `bwrap`; without one these tests print a `SKIPPED:` line
//! (`TOLE_REQUIRE_BWRAP=1` makes that a failure, as CI's `Sandbox (bubblewrap)`
//! job sets it). The sessions directory and the consumers are process-wide and
//! set once here; the refusal side is `command_sandbox_sessions_refused.rs`.

#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tole_core::run_command::RunCommandTool;
use tole_core::sandbox::command::{self, Consumers, Limits};
use tole_core::sandbox::{self, Availability, Mode, Network};
use tole_core::tool::Tool;

struct Setup {
    ws: PathBuf,
}

fn setup() -> &'static Setup {
    static S: OnceLock<Setup> = OnceLock::new();
    S.get_or_init(|| {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shared-sessbx");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let ws = base.join("ws");
        for d in [ws.join(".git/hooks"), ws.join("sessions")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(
            ws.join("sessions/a.jsonl"),
            "{\"entry\":1}\n{\"entry\":2}\n",
        )
        .unwrap();
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
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
        command::set_sessions_dir(ws.join("sessions")).unwrap();
        Setup { ws }
    })
}

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

/// Every file, directory and link under `dir` with its bytes.
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

fn run(line: &str) -> serde_json::Value {
    RunCommandTool::new(setup().ws.clone())
        .execute(json!({ "command": line }))
        .unwrap_or_else(|e| panic!("{line}: {e}"))
}

/// Every way to write, append, create, remove, rename, replace or re-point the
/// sessions directory from inside fails, the whole tree is byte-identical on
/// the host afterwards, and the rest of the workspace is still writable.
#[test]
fn a_sandboxed_command_cannot_touch_the_sessions_dir_but_the_host_still_can() {
    if skipped() {
        return;
    }
    let s = setup();
    let sessions = s.ws.join("sessions");
    let before = tree(&sessions);
    let attempts = [
        "echo x >> sessions/a.jsonl",
        "echo x > sessions/a.jsonl",
        "truncate -s 0 sessions/a.jsonl",
        "rm -f sessions/a.jsonl",
        "echo x > sessions/b.jsonl",
        "mkdir sessions/sub",
        "mv sessions/a.jsonl sessions/z.jsonl",
        "mv sessions sessions2",
        "mv sessions sessions2 && mkdir sessions && echo forged > sessions/a.jsonl",
        "rm -f sessions/a.jsonl && rmdir sessions",
        "rmdir sessions",
        "ln -sf /etc/passwd sessions/a.jsonl",
        "mv sessions sessions2 && ln -s /tmp sessions",
        "cp -a sessions sessions-copy && mv sessions sessions-old && mv sessions-copy sessions",
        "chmod 777 sessions && echo x > sessions/c.jsonl",
        "touch sessions/d.jsonl",
    ];
    for line in attempts {
        // The command must really RUN in the sandbox (not be refused by the
        // tool) and fail; the proof that nothing changed is on the host.
        let out = RunCommandTool::new(s.ws.clone())
            .execute(json!({ "command": format!("sh -c '{line}'") }))
            .unwrap_or_else(|e| panic!("{line:?} was refused before it ran: {e}"));
        assert_ne!(out["status"], 0, "{line:?} must fail inside: {out}");
        assert_eq!(
            tree(&sessions),
            before,
            "{line:?} changed the sessions dir on the host"
        );
        assert!(!s.ws.join("sessions2").exists(), "{line:?}: renamed away");
        // (A COPY next to it, `sessions-copy`, is just a workspace file.)
        assert!(!s.ws.join("sessions-old").exists(), "{line:?}: moved");
    }
    // The rest of the workspace is writable (control: the profile is the command profile).
    let out = run("sh -c 'echo ok > made && echo wrote'");
    assert_eq!(out["stdout"], "wrote\n");
    assert!(s.ws.join("made").exists());
    // tole itself, a host process, keeps appending sessions.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(sessions.join("a.jsonl"))
        .unwrap();
    writeln!(f, "{{\"entry\":3}}").unwrap();
    std::fs::write(sessions.join("new.jsonl"), "{}\n").unwrap();
    // ...and a sandboxed command still READS them.
    let read = run("sh -c 'cat sessions/a.jsonl | wc -l'");
    assert_eq!(read["stdout"].as_str().unwrap().trim(), "3");
}
