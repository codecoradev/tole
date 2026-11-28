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

mod text;
use text::{glob, novars, shell_words, unquote_shell};

/// Wrappers and shell keywords that run another program: every later word is
/// checked as a possible program.
const WRAPPERS: [&str; 30] = [
    "env", "nohup", "exec", "command", "time", "nice", "timeout", "setsid", "stdbuf", "sudo",
    "doas", "busybox", "then", "do", "else", "if", "while", "elif", "until", "builtin", "coproc",
    "!", "setpriv", "chrt", "ionice", "taskset", "script", "strace", "flock", "watch",
];
const SHELLS: [&str; 13] = [
    "sh", "bash", "zsh", "dash", "ksh", "fish", "rbash", "ash", "csh", "tcsh", "mksh", "ksh93",
    "yash",
];
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

/// The program word up to what a shell reads as something else glued to it:
/// `tole>f`, `tole$E`, `tole"$E"`, `'to'$E'le'`.
fn stem(w: &str) -> String {
    unquote(&novars(w.split(['<', '>']).next().unwrap_or("")))
}

/// A leading word that is not the program (#1307), and how many words it takes:
/// `X+=1`; a redirection (`>f`, `2>&1`, `{fd}<f`, `&>f`, `>|f`, `>&-`; a bare
/// operator takes its target too); a lone `{` or `}`, `function NAME`; a
/// variable word, which may expand to nothing. 0: the word is the program.
fn lead_len(w: &str) -> usize {
    let head = w.split(['<', '>']).next().unwrap_or("");
    let var = head.contains('$') && novars(head).chars().all(|c| matches!(c, '"' | '\''));
    if matches!(w, "{" | "}") || var {
        return 1;
    }
    if w == "function" {
        return 2;
    }
    let t = w.trim_start_matches(|c: char| c.is_ascii_digit());
    let t = t
        .strip_prefix('{')
        .and_then(|r| r.split_once('}'))
        .map_or(t, |p| p.1);
    if t.starts_with(['<', '>']) || t.starts_with("&>") {
        let bare = t.chars().all(|c| "<>&|-".contains(c)) && !matches!(t, ">&-" | "<&-");
        1 + bare as usize
    } else {
        w.split_once("+=").is_some_and(|(k, _)| ident(k)) as usize
    }
}

/// A word the guard cannot read (#1307): a glob that can expand to the tole
/// binary (`./tol?`, `$PWD/to[l]e`), or an ANSI-C string with an escape
/// (`$'\x74ole'`). A word with an open quote is a piece of a cut that fell
/// inside quotes and is left to the cuts.
fn hidden(w: &str) -> bool {
    let name: Vec<char> = unquote(w.rsplit('/').next().unwrap_or(""))
        .chars()
        .collect();
    !open(w)
        && (w.contains("$'") && w.contains('\\')
            || w.contains(['*', '?', '['])
                && ["tole", "tole-cli", "tole.exe", "tole-cli.exe"]
                    .iter()
                    .any(|t| glob(&name, &t.chars().collect::<Vec<_>>())))
}

/// A program word the guard cannot read: a command substitution, or `hidden`.
fn unsure(w: &str) -> bool {
    w.starts_with("$(")
        || w.starts_with('`')
        || w.split_once('[').is_some_and(|(k, _)| ident(k))
        || hidden(w)
}

/// A word with a quote or backslash: an assignment value may then hold a blank
/// or a separator, and the cut no longer finds the program (#1302).
fn quoted(w: &str) -> bool {
    w.contains(['\\', '\'', '"'])
}

/// Most leading assignments, redirections and the like before a program: more is
/// doubt, and refused.
const MAX_LEAD: usize = 64;

fn invokes_tole(argv: &[String], cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    if depth > 4 {
        return true; // nested beyond any honest use: refuse, do not guess
    }
    let words: Vec<String> = argv.iter().map(|w| unquote(w)).collect();
    let (mut i, mut lead) = (0, 0);
    loop {
        let from = i;
        while i < argv.len() && is_assignment(&words[i]) {
            i += 1;
        }
        if argv[from..i].iter().any(|w| quoted(w)) {
            return true;
        }
        if i == argv.len() {
            return false;
        }
        // The word the cut takes for the program, then, past the words that are
        // none (#1307), the program behind them.
        let n = lead_len(&argv[i]);
        if program_runs_tole(argv, &words, i, cwd, exe, depth) || (n == 0 && unsure(&argv[i])) {
            return true;
        }
        lead += 1;
        let skipped = &argv[i..(i + n).min(argv.len())];
        if n == 0 || lead > MAX_LEAD || skipped.iter().any(|w| open(w)) {
            return n > 0;
        }
        i = (i + n).min(argv.len());
    }
}

/// Is `argv[i]`, taken as the program, tole or a wrapper or shell that runs it?
fn program_runs_tole(
    argv: &[String],
    words: &[String],
    i: usize,
    cwd: &Path,
    exe: Option<&Path>,
    depth: u32,
) -> bool {
    let (prog, own) = (&words[i], stem(&argv[i]));
    if is_tole(prog, cwd, exe) || is_tole(&own, cwd, exe) {
        return true;
    }
    // `time< f tole x`: a redirection or variable can be glued to the name.
    let named = |list: &[&str], w: &str, own: &str| {
        [base(w), base(own)]
            .iter()
            .any(|b| list.contains(&b.as_str()))
    };
    if named(&WRAPPERS, prog, &own) {
        // `env -S 'tole x'` splits its argument into words; a quote cut open by
        // a separator inside it (`env "X=a;c" tole x`) leaves an open key.
        return argv[i + 1..].iter().any(|w| is_assignment(w) && quoted(w))
            || argv[i + 1..]
                .iter()
                .any(|w| is_assignment(&unquote(w)) && open(w))
            || (i..words.len()).any(|e| base(&words[e]) == "env" && env_split(&words[e + 1..]))
            // The program a wrapper runs is a substitution: `env $(echo tole) x`.
            || argv[i + 1..]
                .iter()
                .find(|w| {
                    !(w.starts_with('-')
                        || is_assignment(&unquote(w))
                        || w.chars().all(|c| c.is_ascii_digit() || c == '.'))
                })
                .is_some_and(|w| w.starts_with("$(") || w.starts_with('`'))
            || (i + 1..words.len()).any(|j| {
                !is_assignment(&words[j])
                    && (is_tole(&words[j], cwd, exe)
                        || is_tole(&stem(&argv[j]), cwd, exe)
                        || hidden(&argv[j]))
            })
            // The first shell behind the wrapper: `env -i sh -c 'tole x'`.
            || (i + 1..words.len())
                .find(|&j| named(&SHELLS, &words[j], &stem(&argv[j])))
                .is_some_and(|j| shell_runs_tole(argv, j, cwd, exe, depth));
    }
    named(&SHELLS, prog, &own) && shell_runs_tole(argv, i, cwd, exe, depth)
}

/// Does the shell at `argv[at]` run tole from its `-c` payload (`-c`, `-lc`, `-xc`)?
fn shell_runs_tole(argv: &[String], at: usize, cwd: &Path, exe: Option<&Path>, depth: u32) -> bool {
    let mut rest = argv[at + 1..].iter();
    while let Some(f) = rest.next() {
        if f.starts_with('-') && !f.starts_with("--") && f.contains('c') {
            // The payload, and anything after it, is shell text. Words that come
            // from shell text (depth > 0) are still quoted for the shell the
            // text is handed to, and mean something only together (#1307).
            let rest: Vec<&str> = rest.map(String::as_str).collect();
            let text = |t: &str| {
                payload_invokes_tole(t, cwd, exe, depth + 1)
                    || [false, true].iter().any(|&q| {
                        let on = if q { depth > 0 } else { true };
                        on && t.contains(if q {
                            &['\\', '\'', '"'][..]
                        } else {
                            &['\\'][..]
                        }) && payload_invokes_tole(&unquote_shell(t, q), cwd, exe, depth + 1)
                    })
            };
            return text(&rest.join(" ")) || rest.iter().any(|p| *p != "$(" && text(p));
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
        && [false, true].iter().any(|&q| {
            shell_words(payload, q)
                .iter()
                .any(|w| !w.is_empty() && invokes_tole(w, cwd, exe, depth))
        }))
}
