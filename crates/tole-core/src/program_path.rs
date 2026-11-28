//! A program written as a path (#1286): `./target/release/foo`, `/usr/bin/make`.
//!
//! [`check_destructive_argv`] still refuses every such program (the cron paths
//! keep that rule). `run_command` and `job_start` call [`check_command_argv`]
//! instead, which lets a path through when it names a real executable file and
//! the blocklist, read on what the path really is, has nothing against it.
//! Nothing a bare name is refused for is allowed here. The program is judged
//! under every name it goes by (the one written, each link on the way, the
//! file it resolves to), and so is every interpreter on its `#!` chain (the
//! kernel follows four): a name of tole, the same file as the running binary
//! (a hard link too), the teardown and `rm` rules, and the session guard. A
//! program inside a `sh -c` text stays refused (it is a shell's to resolve).
//! A bare-name allow pattern never matches a path (`approval::argv_matches`),
//! a deny matches its file name, and the sandbox keeps its own rule (a
//! sandboxed command runs a bare name only).
//!
//! Residual (`docs/threat-model.md`, RC-1): the file can be swapped between
//! this check and the spawn, and a renamed COPY of a program is not what its
//! name says. The first is the approval gate's and the sandbox's to stop, the
//! second is what the session marker (`session_guard::mark`) is for.

use crate::subprocess::check_destructive_argv;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most interpreters the kernel follows (`BINPRM_MAX_RECURSION`).
const SHEBANG_DEPTH: usize = 4;
const SHEBANG_MAX: u64 = 256;
const MAX_LINKS: usize = 40;

fn esc(s: &str) -> String {
    s.chars().take(120).flat_map(char::escape_debug).collect()
}

fn is_tole(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.strip_suffix(".exe").unwrap_or(&n), "tole" | "tole-cli")
}

/// A program as exec sees it: every name it goes by, and the file it is.
struct Found {
    names: Vec<String>,
    file: PathBuf,
}

/// Follow `path` (read from `cwd`) through its links: the file name of each
/// hop, then the canonical file. `None`: it does not resolve or has no name.
fn follow(path: &str, cwd: &Path) -> Option<Found> {
    let mut p = cwd.join(path);
    let mut names = Vec::new();
    for _ in 0..MAX_LINKS {
        names.push(p.file_name()?.to_str()?.to_string());
        if !std::fs::symlink_metadata(&p).ok()?.is_symlink() {
            break;
        }
        p = p.parent()?.join(std::fs::read_link(&p).ok()?);
    }
    let file = p.canonicalize().ok()?;
    names.push(file.file_name()?.to_str()?.to_string());
    Some(Found { names, file })
}

/// Open `f` for inspection: not through a link and never blocking on a pipe;
/// only a regular file with an execute bit passes. `Err` is the reason.
fn open_runnable(f: &Path) -> Result<std::fs::File, &'static str> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let file = o.open(f).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => "it cannot be read",
        _ => "no executable file is there",
    })?;
    let meta = file.metadata().map_err(|_| "it cannot be read")?;
    #[cfg(unix)]
    let exec = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
    #[cfg(not(unix))]
    let exec = true;
    if meta.is_file() && exec {
        Ok(file)
    } else {
        Err("no executable file is there")
    }
}

/// Is `f` the running executable (the same inode, so a hard link counts)?
#[cfg(unix)]
fn is_running_exe(f: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (
        f.metadata(),
        std::env::current_exe().and_then(|e| e.metadata()),
    ) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_running_exe(_: &std::fs::File) -> bool {
    false
}

/// The `#!` line of `file` (up to the first newline or NUL, as the kernel
/// reads it), split at blanks: the interpreter and its optional argument.
fn shebang(file: &std::fs::File) -> Option<Vec<String>> {
    let mut head = Vec::new();
    file.take(SHEBANG_MAX).read_to_end(&mut head).ok()?;
    let line = head.strip_prefix(b"#!")?;
    let end = line.iter().position(|b| matches!(b, 0 | b'\n'));
    let line = String::from_utf8_lossy(&line[..end.unwrap_or(line.len())]).into_owned();
    Some(
        line.split([' ', '\t'])
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Judge a program under every name it goes by, with `tail` as its arguments.
fn judge(found: &Found, tail: &[String], cwd: &Path) -> Result<(), String> {
    if found.names.iter().any(|n| is_tole(n)) {
        return Err("a session may not run the tole binary (a file or link named tole)".into());
    }
    let mut argv = vec![String::new()];
    argv.extend_from_slice(tail);
    for name in &found.names {
        argv[0] = name.clone();
        check_destructive_argv(&argv)?;
        crate::session_guard::check_session_argv(&argv, cwd)?;
    }
    argv[0] = found.file.to_string_lossy().into_owned();
    crate::session_guard::check_session_argv(&argv, cwd)
}

/// [`check_destructive_argv`] for the tools that run `argv` from `cwd`, where
/// `argv[0]` may be a path (see the module text). `Err` carries the reason.
pub fn check_command_argv(argv: &[String], cwd: &Path) -> Result<(), String> {
    let Some(prog) = argv.first().filter(|p| p.contains('/')) else {
        return check_destructive_argv(argv);
    };
    // A tole binary is never let through, found or not (the session guard).
    if prog.rsplit('/').next().is_some_and(is_tole) {
        return check_destructive_argv(argv);
    }
    let (mut at, mut tail) = (prog.clone(), argv[1..].to_vec());
    for depth in 0..=SHEBANG_DEPTH {
        let refuse = |why: &str| {
            let via = if depth == 0 {
                String::new()
            } else {
                format!(", the interpreter of a `#!` line ({})", esc(prog))
            };
            format!(
                "refused \u{2014} the program is written as a path ({}{via}) but {why} (a relative path is read from the workspace, {}; there is no shell, so `~` and `$VAR` are not expanded).",
                esc(&at),
                cwd.display()
            )
        };
        let found = follow(&at, cwd).ok_or_else(|| refuse("no executable file is there"))?;
        let file = open_runnable(&found.file).map_err(refuse)?;
        let verdict = if is_running_exe(&file) {
            Err("a session may not run the tole binary (this is its file)".to_string())
        } else {
            judge(&found, &tail, cwd)
        };
        verdict.map_err(|why| match depth {
            0 => why,
            _ => format!(
                "the `#!` line of {} runs a program that is refused: {why}",
                esc(prog)
            ),
        })?;
        let Some(words) = shebang(&file) else {
            return Ok(());
        };
        // The kernel runs the interpreter (a path, read from the child's
        // working directory) with its optional argument, then the script.
        let Some((interp, rest)) = words.split_first() else {
            return Err(refuse("its `#!` line names no interpreter"));
        };
        (at, tail) = (interp.clone(), rest.to_vec());
    }
    Err(format!(
        "refused \u{2014} the program ({}) has a `#!` chain deeper than the kernel follows.",
        esc(prog)
    ))
}
