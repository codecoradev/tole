//! Owner-only records are not an agent's to write (#684).
//!
//! Records that mean "the owner did this" (`tole cron create`, `tole cron
//! resume`, `plugin approve`, `skills approve`, `config trust`) are written
//! by the official CLI, which any same-uid process a session starts could
//! run. Two layers, both defense in depth and not a boundary:
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
//! Residual (`docs/threat-model.md`): a copied binary run with a cleared
//! environment, or an interpreter writing the files directly, is not stopped
//! here. Only a sandbox that cannot write the data dir closes that (#601).

use crate::child_env::UNDER_SESSION;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Wrappers and shell keywords that run another program: every later word is
/// checked as a possible program.
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
    if argv.iter().any(|a| unquote(a).contains(UNDER_SESSION)) {
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

/// Resolve a program word the way exec does (a path, or a `PATH` lookup).
fn resolve(w: &str, cwd: &Path) -> Option<PathBuf> {
    let p = if w.contains('/') || w.contains('\\') {
        cwd.join(w) // an absolute `w` replaces `cwd`
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
    matches!(base(w).as_str(), "tole" | "tole-cli")
        || exe.is_some_and(|e| resolve(w, cwd).is_some_and(|r| r == e))
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// A blank-cut word that ends in a quote or a backslash, or holds `$'` or a
/// backslash in single quotes (fish): the value may hold a blank or separator,
/// so the words after it are not the ones a shell sees (#1302).
fn open_word(w: &str) -> bool {
    let (mut q, mut esc) = (None, false);
    for c in w.chars() {
        match (q, c, esc) {
            (_, _, true) => esc = false,
            (Some('\''), '\\', _) => return true,
            (_, '\\', _) => esc = true,
            (Some(k), c, _) if c == k => q = None,
            (None, '\'' | '"', _) => q = Some(c),
            _ => {}
        }
    }
    esc || q.is_some() || w.contains("$'")
}

fn invokes_tole(argv: &[String], cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    if depth > 4 {
        return true; // nested beyond any honest use: refuse, do not guess
    }
    let words: Vec<String> = argv.iter().map(|w| unquote(w)).collect();
    let i = words.iter().position(|w| !is_assignment(w));
    let open = |j: usize| is_assignment(&words[j]) && open_word(&argv[j]);
    if (0..i.unwrap_or(words.len())).any(open) {
        return true;
    }
    let Some(i) = i else { return false };
    let prog = &words[i];
    if is_tole(prog, cwd, exe) {
        return true;
    }
    let b = base(prog);
    if WRAPPERS.contains(&b.as_str()) {
        return (i + 1..words.len()).any(open)
            || words[i + 1..]
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

/// Programs whose arguments are data: none has an option that runs a program,
/// so a quoted separator is no command boundary (#1287). Not listed: `echo`,
/// `printf` (piped into a shell) and `rg` (`--pre`, `-z`... run programs).
const INERT: [&str; 8] = ["grep", "egrep", "fgrep", "ls", "cat", "head", "tail", "wc"];
/// The only commands allowed next to an inert program in a quote-aware cut:
/// any other (`alias`, `function`, `eval`...) might redefine an [`INERT`] name.
const FILLER: [&str; 6] = ["echo", "true", "false", ":", "cd", "pwd"];
const SEPARATORS: [char; 9] = [';', '&', '|', '(', ')', '`', '\n', '{', '}'];

fn naive(s: &str) -> Vec<&str> {
    s.split(SEPARATORS).collect()
}

/// Words cut at space and tab only (a shell keeps NBSP, `\r`, `\v` in a word).
fn blank_words(seg: &str) -> impl Iterator<Item = &str> {
    seg.split([' ', '\t']).filter(|w| !w.is_empty())
}

/// Inert only when the first word is an [`INERT`] program (a leading assignment
/// may hold a quoted blank: `X='a;tole grep b' tole x` runs tole).
fn inert(seg: &str) -> bool {
    blank_words(seg).next().is_some_and(|w| INERT.contains(&w))
}

/// Split shell text at the separators the shell acts on, keeping quoted text
/// whole. `None` when unsure: an unbalanced quote, or quoting this scan does
/// not follow (comment, here-document, `$'..'`, a substitution or `${` in
/// double quotes: bash 5.3 and ksh run `${ cmd; }` there, a backslash in
/// single quotes: fish). The caller then splits naively, the stricter reading.
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
                    '$' if it.peek().is_some_and(|&(_, n)| matches!(n, '(' | '{')) => return None,
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
/// quote-aware, and a quoted separator stays literal only when every command
/// is an [`INERT`] program or a [`FILLER`] (seen through the naive cut).
/// Anything else, and any doubt, splits the whole text naively, as does
/// deeper text (blank-cut words, quotes still on), so nesting cannot loosen it.
fn payload_invokes_tole(payload: &str, cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    let filler = |q: &str| blank_words(q).next().is_none_or(|f| FILLER.contains(&f));
    let parts = (depth == 1)
        .then(|| split_quoted(payload))
        .flatten()
        .filter(|ps| {
            ps.iter()
                .all(|p| inert(p) || naive(p).into_iter().all(filler))
        });
    let segs = match parts {
        Some(parts) => parts
            .into_iter()
            .flat_map(|p| if inert(p) { vec![p] } else { naive(p) })
            .collect(),
        None => naive(payload),
    };
    segs.into_iter().any(|seg| {
        let words: Vec<String> = blank_words(seg).map(String::from).collect();
        !words.is_empty() && invokes_tole(&words, cwd, exe, depth)
    })
}
