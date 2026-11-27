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

/// Programs that treat a quoted argument as data (a pattern, a file name),
/// never as shell text or a program to run. A separator inside such an
/// argument is not a command boundary (#1287). `echo` and `printf` are not
/// listed: their output can be piped into a shell.
const INERT: [&str; 9] = [
    "grep", "egrep", "fgrep", "rg", "ls", "cat", "head", "tail", "wc",
];
const SEPARATORS: [char; 9] = [';', '&', '|', '(', ')', '`', '\n', '{', '}'];

fn naive(s: &str) -> Vec<&str> {
    s.split(SEPARATORS).collect()
}

fn inert(seg: &str) -> bool {
    let mut words = seg.split_whitespace().skip_while(|w| is_assignment(w));
    words.next().is_some_and(|w| INERT.contains(&w))
}

/// Split shell text at the separators the shell acts on, keeping quoted text
/// whole. `None` when unsure: an unbalanced quote, or quoting this scan does
/// not follow (comment, here-document, `$'..'`, command substitution inside
/// double quotes, a backslash in single quotes, which fish treats as an
/// escape). The caller then splits naively: refuse rather than guess.
fn split_quoted(s: &str) -> Option<Vec<&str>> {
    let (mut parts, mut start) = (Vec::new(), 0);
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let next = it.peek().map(|&(_, n)| n);
        match c {
            '\\' => drop(it.next()),
            '\'' => loop {
                match it.next()?.1 {
                    '\'' => break,
                    '\\' if matches!(it.peek(), Some(&(_, '\'' | '\\'))) => return None,
                    _ => {}
                }
            },
            '"' => loop {
                match it.next()?.1 {
                    '"' => break,
                    '\\' => drop(it.next()),
                    '`' => return None,
                    '$' if it.peek().is_some_and(|&(_, n)| n == '(') => return None,
                    _ => {}
                }
            },
            '`' | '#' => return None,
            '$' if next == Some('\'') => return None,
            '<' if next == Some('<') => return None,
            ';' | '&' | '|' | '(' | ')' | '\n' | '{' | '}' => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    Some(parts)
}

/// Shell text: each simple command (split on `; & | ( ) \` newline { }`) is
/// checked like an argv. Text straight from a real argv (depth 1) is split
/// quote-aware and a quoted separator only stays literal in the arguments of
/// an [`INERT`] program; everything else, and any doubt, splits naively.
/// Deeper text was cut on whitespace with its quotes still on, so it is
/// always split naively.
fn payload_invokes_tole(payload: &str, cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    let parts = (depth == 1).then(|| split_quoted(payload)).flatten();
    let segs = match parts {
        Some(parts) => parts
            .into_iter()
            .flat_map(|p| if inert(p) { vec![p] } else { naive(p) })
            .collect(),
        None => naive(payload),
    };
    segs.into_iter().any(|seg| {
        let words: Vec<String> = seg.split_whitespace().map(String::from).collect();
        !words.is_empty() && invokes_tole(&words, cwd, exe, depth)
    })
}
