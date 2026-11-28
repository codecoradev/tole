//! A program written as a path (#1286): `./target/release/foo`, `/usr/bin/make`.
//!
//! [`check_destructive_argv`] still refuses every such program (the cron paths
//! keep that rule). `run_command` and `job_start` call [`check_command_argv`]
//! instead, which lets a path through when it names a real executable file and
//! the blocklist, read on what the path really is, has nothing against it.
//! Nothing a bare name is refused for is allowed here. The program is judged
//! under every name it goes by (the one written, each link on the way, the
//! file it resolves to), and so is every interpreter on its `#!` chain, each
//! with the argv the kernel would give it (the interpreter, its `#!` argument,
//! the script path, then the caller's arguments): a name of tole, the same file
//! as the running binary (a hard link too), the teardown and `rm` rules, and
//! the session guard. A script argument that looks like `-c` is read by a shell
//! interpreter as a command string, as `sh script -c ...` is by the blocklist,
//! and is refused with the argv judged. The `#!` line is split at every blank
//! (the kernel passes the rest as one argument: as strict or stricter). The
//! chain is the program and at most four interpreters (the kernel allows five
//! scripts nested). A program inside a `sh -c` text stays refused (a shell's to
//! resolve). A bare-name allow never matches a path (`approval::argv_matches`),
//! a deny matches its file name, and a sandboxed command runs a bare name only.
//!
//! Residual (`docs/threat-model.md`, RC-1): a swap between this check and the
//! spawn (the approval gate's and the sandbox's to stop), and a renamed COPY
//! of a program (the session marker's, `session_guard::mark`).

use crate::subprocess::check_destructive_argv;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most interpreters after the program (the kernel allows five scripts).
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

const GONE: &str = "no executable file is there";

/// Follow `path` (read from `cwd`) through its links: the file name of each
/// hop, then the canonical file. `Err` is why it does not resolve.
fn follow(path: &str, cwd: &Path) -> Result<Found, &'static str> {
    let mut p = cwd.join(path);
    let mut names = Vec::new();
    let mut resolved = false;
    for _ in 0..MAX_LINKS {
        names.push(
            p.file_name()
                .and_then(|n| n.to_str())
                .ok_or(GONE)?
                .to_string(),
        );
        if !std::fs::symlink_metadata(&p)
            .map_err(|_| GONE)?
            .is_symlink()
        {
            resolved = true;
            break;
        }
        let to = std::fs::read_link(&p).map_err(|_| GONE)?;
        p = p.parent().ok_or(GONE)?.join(to);
    }
    if !resolved {
        return Err("too many levels of symbolic links");
    }
    let file = p.canonicalize().map_err(|_| GONE)?;
    names.push(
        file.file_name()
            .and_then(|n| n.to_str())
            .ok_or(GONE)?
            .to_string(),
    );
    Ok(Found { names, file })
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
        _ => GONE,
    })?;
    let meta = file.metadata().map_err(|_| "it cannot be read")?;
    #[cfg(unix)]
    let exec = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
    #[cfg(not(unix))]
    let exec = true;
    if meta.is_file() && exec {
        Ok(file)
    } else {
        Err(GONE)
    }
}

/// The (device, inode) of the running binary, read through the process and not
/// through a path that is gone once the binary is replaced or deleted. `None`:
/// it cannot be read, and the caller refuses.
#[cfg(unix)]
fn running_exe() -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata("/proc/self/exe")
        .or_else(|_| std::env::current_exe().and_then(std::fs::metadata))
        .ok()?;
    Some((m.dev(), m.ino()))
}

/// The same identity for `f`: a hard link to the running binary has it too.
#[cfg(unix)]
fn file_id(f: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    f.metadata().ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn running_exe() -> Option<(u64, u64)> {
    Some((0, 0))
}

#[cfg(not(unix))]
fn file_id(_: &std::fs::File) -> Option<(u64, u64)> {
    None
}

/// The `#!` line of `file` (up to a newline or NUL, as the kernel reads it),
/// split at every blank: more words to judge than the kernel's one argument.
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
    let Some(me) = running_exe() else {
        return Err(format!(
            "refused \u{2014} the program is written as a path ({}) and this process cannot read the identity of its own binary, so it cannot tell whether the path is the tole binary.",
            esc(prog)
        ));
    };
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
        let found = follow(&at, cwd).map_err(refuse)?;
        let file = open_runnable(&found.file).map_err(refuse)?;
        let verdict = if file_id(&file) == Some(me) {
            Err("a session may not run the tole binary (this is its file)".to_string())
        } else {
            judge(&found, &tail, cwd)
        };
        verdict.map_err(|why| match depth {
            0 => why,
            _ => format!(
                "the `#!` line of {} runs `{}`, which is refused: {why}",
                esc(prog),
                esc(&format!("{at} {}", tail.join(" ")))
            ),
        })?;
        let Some(words) = shebang(&file) else {
            return Ok(());
        };
        // The kernel runs the interpreter (a path, read from the child's
        // working directory) with its optional argument, the script as it was
        // written and then the caller's arguments: that is what is judged next.
        let Some((interp, rest)) = words.split_first() else {
            return Err(refuse("its `#!` line names no interpreter"));
        };
        let script = std::mem::replace(&mut at, interp.clone());
        tail = [rest, &[script], tail.as_slice()].concat();
    }
    Err(format!(
        "refused \u{2014} the program ({}) has a `#!` chain deeper than is followed here (the program and four interpreters).",
        esc(prog)
    ))
}
