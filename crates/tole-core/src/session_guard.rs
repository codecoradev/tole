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
const WRAPPERS: [&str; 22] = [
    "env", "nohup", "exec", "command", "time", "nice", "timeout", "setsid", "stdbuf", "sudo",
    "doas", "busybox", "then", "do", "else", "if", "while", "elif", "until", "builtin", "coproc",
    "!",
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

fn ident(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| ident(k))
}

/// A word the cut may have split inside quotes or an escaped blank.
fn open(w: &str) -> bool {
    w.ends_with('\\') || ['\'', '"'].iter().any(|q| w.matches(*q).count() % 2 == 1)
}

/// `env`'s own options (`-S`, `--split-string`, abbreviated) up to its program.
fn env_split(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(w) = it.next() {
        if w == "-u" || w == "-C" {
            it.next();
        } else if w.starts_with("--s")
            || (w.starts_with('-') && !w.starts_with("--") && w.contains('S'))
        {
            return true;
        } else if !(w.starts_with('-') || is_assignment(w)) {
            break;
        }
    }
    false
}

/// A leading word that is not the program (#1307): `X+=1` or a redirection
/// (`>f`, `2>&1`, `{fd}<f`, `&>f`). How many words it takes: a bare operator
/// takes its target too.
fn lead_len(w: &str) -> usize {
    let t = w.trim_start_matches(|c: char| c.is_ascii_digit());
    let t = t
        .strip_prefix('{')
        .and_then(|r| r.split_once('}'))
        .map_or(t, |p| p.1);
    if t.starts_with(['<', '>']) || t.starts_with("&>") {
        1 + t.chars().all(|c| "<>&|-".contains(c)) as usize
    } else {
        w.split_once("+=").is_some_and(|(k, _)| ident(k)) as usize
    }
}

/// A word with a quote or backslash: an assignment value may then hold a blank
/// or a separator, and the cut no longer finds the program (#1302).
fn quoted(w: &str) -> bool {
    w.contains(['\\', '\'', '"'])
}

fn invokes_tole(argv: &[String], cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    if depth > 4 {
        return true; // nested beyond any honest use: refuse, do not guess
    }
    let words: Vec<String> = argv.iter().map(|w| unquote(w)).collect();
    let i = words.iter().position(|w| !is_assignment(w));
    if argv[..i.unwrap_or(argv.len())].iter().any(|w| quoted(w)) {
        return true;
    }
    let Some(i) = i else { return false };
    let prog = &words[i];
    if is_tole(prog, cwd, exe) {
        return true;
    }
    // Words before the program that the cut takes for it (#1307): the program
    // behind them is checked too, and what cannot be followed is refused: a
    // quoted one, an array assignment, a command substitution as the program.
    let n = lead_len(&argv[i]);
    let lead = &argv[i..(i + n).min(argv.len())];
    if lead.iter().any(|w| open(w))
        || argv[i].split_once('[').is_some_and(|(k, _)| ident(k))
        || argv[i].starts_with("$(")
        || argv[i].starts_with('`')
        || (n > 0 && invokes_tole(argv.get(i + n..).unwrap_or_default(), cwd, exe, depth))
    {
        return true;
    }
    let b = base(prog);
    if WRAPPERS.contains(&b.as_str()) {
        // `env -S 'tole x'` splits its argument into words; a quote cut open by
        // a separator inside it (`env "X=a;c" tole x`) leaves an open key.
        let env = words[i..].iter().position(|w| base(w) == "env");
        return argv[i + 1..].iter().any(|w| is_assignment(w) && quoted(w))
            || argv[i + 1..]
                .iter()
                .any(|w| is_assignment(&unquote(w)) && open(w))
            || env.is_some_and(|e| env_split(&words[i + e + 1..]))
            || words[i + 1..]
                .iter()
                .any(|w| !is_assignment(w) && is_tole(w, cwd, exe));
    }
    if SHELLS.contains(&b.as_str()) {
        let mut rest = argv[i + 1..].iter();
        while let Some(f) = rest.next() {
            if f.starts_with('-') && !f.starts_with("--") && f.contains('c') {
                // The payload, and anything after it, is shell text; written
                // unquoted it may hold escaped blanks and separators, and its
                // words only mean something together (#1307).
                let rest: Vec<&str> = rest.map(String::as_str).collect();
                let text = |t: &str| {
                    payload_invokes_tole(t, cwd, exe, depth + 1)
                        || (t.contains('\\')
                            && payload_invokes_tole(&unescape(t), cwd, exe, depth + 1))
                };
                return text(&rest.join(" ")) || rest.iter().any(|p| *p != "$(" && text(p));
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
    let quote_aware = parts.is_some();
    let segs = match parts {
        Some(parts) => parts
            .into_iter()
            .flat_map(|p| if inert(p) { vec![p] } else { naive(p) })
            .collect(),
        None => naive(payload),
    };
    // Both cuts, so a word glued by NBSP, CR, VT or FF is never read looser
    // than the shell-faithful cut alone would (`env {<NBSP>X= tole x`).
    segs.into_iter().any(|seg| {
        let cuts = [
            blank_words(seg).map(String::from).collect::<Vec<_>>(),
            seg.split_whitespace().map(String::from).collect(),
        ];
        cuts.iter()
            .any(|w| !w.is_empty() && invokes_tole(w, cwd, exe, depth))
    }) || (!quote_aware
        && shell_words(payload)
            .iter()
            .any(|w| !w.is_empty() && invokes_tole(w, cwd, exe, depth)))
}

/// What a shell makes of an unquoted word: a backslash keeps the next character.
fn unescape(w: &str) -> String {
    let mut it = w.chars();
    std::iter::from_fn(|| {
        it.next()
            .map(|c| if c == '\\' { it.next() } else { Some(c) })
    })
    .flatten()
    .collect()
}

/// A second reading of shell text, in addition to the cuts above and only when
/// they were naive (#1307): where a cut splits a word the shell keeps whole, the
/// program behind it was never seen. A backslash keeps the next character in
/// its word (`a\ b`, `a\;b`), `{` and `}` are word characters, and the `&` of
/// `>&`, `<&`, `&>` belongs to a redirection. A substitution that opens a word
/// (`$(..)`, a backtick) becomes the word `$(`: an unknown program. Quotes are
/// not followed, so this only ever adds refusals.
fn shell_words(s: &str) -> Vec<Vec<String>> {
    let (mut cmds, mut cmd, mut word) = (Vec::new(), Vec::new(), String::new());
    let (mut esc, mut prev) = (false, ' ');
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        let amp = c == '&' && !matches!(prev, '<' | '>') && it.peek() != Some(&'>');
        let sep = !esc && (matches!(c, ';' | '|' | '(' | ')' | '`' | '\n') || amp);
        let at = word.trim_start_matches(['\'', '"']);
        if (c == '`' || c == '(') && !esc && at == if c == '`' { "" } else { "$" } {
            word = "$(".into();
        }
        if sep || (!esc && matches!(c, ' ' | '\t')) {
            if !word.is_empty() {
                cmd.push(std::mem::take(&mut word));
            }
            if sep {
                cmds.push(std::mem::take(&mut cmd));
            }
        } else {
            word.push(c);
        }
        (esc, prev) = (!esc && c == '\\', c);
    }
    if !word.is_empty() {
        cmd.push(word);
    }
    cmds.push(cmd);
    cmds
}
