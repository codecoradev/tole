//! Owner-only records are not an agent's to write (#684).
//!
//! Records that mean "the owner did this" (`tole cron create`, `tole cron
//! resume`, `plugin approve`, `skills approve`, `config trust`) are written
//! by the official CLI, and any same-uid process a session starts could run
//! it. Two layers, both defense in depth and not a boundary:
//!
//! 1. [`check_session_argv`]: in EVERY session kind, `run_command` and
//!    `job_start` refuse an argv whose program is the tole binary (by name,
//!    or by resolution to the running executable, symlinks included), also
//!    behind the usual wrappers (`env`, `nohup`, `sh -c`, ...), and refuse
//!    any argument that names the [`UNDER_SESSION`] marker.
//! 2. [`mark`]: every process those tools start carries the marker, so a
//!    tole that still gets started (a renamed copy of the binary) refuses
//!    the owner-only subcommands (`child_env::under_session`).
//!
//! Residual (documented in `docs/threat-model.md`): a copied binary run
//! with a cleared environment, or an interpreter writing the files
//! directly, is not stopped here. Only a sandbox that cannot write the data
//! dir closes that (#601).

use crate::child_env::UNDER_SESSION;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Wrapper programs and shell keywords that run another program: every
/// later word is then checked as a possible program.
const WRAPPERS: [&str; 17] = [
    "env", "nohup", "exec", "command", "time", "nice", "timeout", "setsid", "stdbuf", "sudo",
    "doas", "busybox", "then", "do", "else", "if", "while",
];
const SHELLS: [&str; 6] = ["sh", "bash", "zsh", "dash", "ksh", "fish"];
const ERR_TOLE: &str = "a session may not run the tole binary: owner-only records (cron jobs, \
     plugin, skill and config approvals) are written by the owner from a terminal \
     (issue #684)";
const ERR_MARKER: &str = "a session may not touch the TOLE_UNDER_SESSION marker (issue #684)";

/// Mark a process a session's command tool is about to start.
pub fn mark(cmd: &mut Command) -> &mut Command {
    cmd.env(UNDER_SESSION, "1")
}

/// Refuse an argv that runs the tole binary or manipulates the marker.
/// `cwd` resolves relative program paths the way the spawn will.
pub fn check_session_argv(argv: &[String], cwd: &Path) -> Result<(), String> {
    let strip = |a: &str| -> String {
        a.chars()
            .filter(|c| !matches!(c, '"' | '\'' | '\\' | '$' | '{' | '}'))
            .collect()
    };
    if argv.iter().any(|a| strip(a).contains(UNDER_SESSION)) {
        return Err(ERR_MARKER.into());
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    if invokes_tole(argv, cwd, exe.as_deref(), 0) {
        return Err(ERR_TOLE.into());
    }
    Ok(())
}

fn unquote(w: &str) -> String {
    w.chars()
        .filter(|c| !matches!(c, '"' | '\'' | '`' | '\\' | '$' | '(' | ')' | '{' | '}'))
        .collect()
}

fn base(w: &str) -> String {
    let b = Path::new(w)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(w);
    b.strip_suffix(".exe").unwrap_or(b).to_string()
}

fn named_tole(w: &str) -> bool {
    matches!(base(w).as_str(), "tole" | "tole-cli")
}

/// Resolve a program word the way exec does (a path, or a `PATH` lookup).
fn resolve(w: &str, cwd: &Path) -> Option<PathBuf> {
    let p = if w.contains('/') || w.contains('\\') {
        let p = PathBuf::from(w);
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(w))
            .find(|c| c.is_file())?
    };
    p.canonicalize().ok()
}

fn is_tole(w: &str, cwd: &Path, exe: Option<&Path>) -> bool {
    if w.is_empty() || w.starts_with('-') {
        return false;
    }
    named_tole(w) || exe.is_some_and(|e| resolve(w, cwd).is_some_and(|r| r == e))
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn invokes_tole(argv: &[String], cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    if depth > 4 {
        // Nested beyond any honest use: refuse rather than guess.
        return true;
    }
    let words: Vec<String> = argv.iter().map(|w| unquote(w)).collect();
    let Some(i) = words.iter().position(|w| !is_assignment(w)) else {
        return false;
    };
    let prog = &words[i];
    if is_tole(prog, cwd, exe) {
        return true;
    }
    let b = base(prog);
    if WRAPPERS.contains(&b.as_str()) {
        // Any later word may be the wrapped program (`env -u X tole`,
        // `timeout 5 tole`).
        return words[i + 1..]
            .iter()
            .any(|w| !is_assignment(w) && is_tole(w, cwd, exe));
    }
    if SHELLS.contains(&b.as_str()) {
        let mut rest = argv[i + 1..].iter();
        while let Some(f) = rest.next() {
            if f.starts_with('-') && !f.starts_with("--") && f.contains('c') {
                // The payload, and anything after it, is shell text.
                return rest.any(|p| payload_invokes_tole(p, cwd, exe, depth + 1));
            }
        }
    }
    false
}

/// Shell text: each simple command (split on `; & | ( ) \` newline`) is
/// checked like an argv.
fn payload_invokes_tole(payload: &str, cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    payload
        .split([';', '&', '|', '(', ')', '`', '\n', '{', '}'])
        .any(|seg| {
            let words: Vec<String> = seg.split_whitespace().map(String::from).collect();
            !words.is_empty() && invokes_tole(&words, cwd, exe, depth)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        crate::run_command::RunCommandTool::split_argv(line).unwrap()
    }

    fn refused(line: &str) -> bool {
        check_session_argv(&argv(line), Path::new("/tmp")).is_err()
    }

    #[test]
    fn the_tole_binary_is_refused_in_every_form() {
        for line in [
            "tole cron resume job-1",
            "/usr/local/bin/tole plugin approve x --hash h",
            "./tole config trust --yes",
            "tole-cli skills approve s",
            "tole.exe cron create '@daily' --script x",
            "FOO=1 tole cron resume j",
            "env tole cron resume j",
            "env -u TOLE_FOO tole cron resume j",
            "env -i /opt/tole cron resume j",
            "nohup tole cron resume j",
            "timeout 5 tole cron resume j",
            "sh -c 'tole cron resume j'",
            "bash -lc 'cd x && tole cron resume j'",
            "sh -c 'echo hi; tole cron resume j'",
            "bash -c 'x=$(tole cron list)'",
            "sh -c 'if true; then tole cron resume j; fi'",
            "bash -c \"bash -c 'tole cron resume j'\"",
        ] {
            assert!(refused(line), "must refuse: {line}");
        }
    }

    #[test]
    fn benign_commands_that_mention_tole_pass() {
        for line in [
            "git commit -m 'fix tole bug'",
            "echo tole",
            "grep -rn tole src",
            "sh -c 'echo tole'",
            "cat tole.toml",
            "cargo test -p tole-core",
            "ls tole-agents",
            "env FOO=1 printenv FOO",
            "sh -c 'cargo build -p tole-cli'",
        ] {
            assert!(!refused(line), "must allow: {line}");
        }
    }

    #[test]
    fn the_marker_cannot_be_named() {
        for line in [
            "env -u TOLE_UNDER_SESSION printenv",
            "sh -c 'unset TOLE_UNDER_SESSION; ls'",
            "env TOLE_UNDER_SE''SSION= ls",
            "env TOLE_UNDER_SESSION= ls",
        ] {
            assert!(refused(line), "must refuse: {line}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_the_running_executable_is_refused() {
        let dir = std::env::temp_dir().join(format!("tole-guard-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        std::os::unix::fs::symlink(&exe, dir.join("helper")).unwrap();
        assert!(check_session_argv(&["./helper".into(), "x".into()], &dir).is_err());
        let abs = dir.join("helper").display().to_string();
        assert!(check_session_argv(&[abs, "x".into()], &dir).is_err());
        assert!(check_session_argv(&["env".into(), "./helper".into()], &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_sets_the_marker() {
        let mut cmd = Command::new("true");
        mark(&mut cmd);
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == UNDER_SESSION && v.is_some()));
    }
}
