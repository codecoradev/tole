//! A program written as a path (#1286): `./target/release/foo`, `/usr/bin/make`.
//!
//! [`check_destructive_argv`] still refuses every such program (the cron paths
//! keep that rule). `run_command` and `job_start` call [`check_command_argv`]
//! instead, which lets a path through when it names a real executable file and
//! the blocklist, read on what the path really is, has nothing against it.
//! Nothing a bare name is refused for is allowed here: the same teardown and
//! `rm` rules run on the name as written AND on the name of the file it
//! resolves to (a symlink to `rm` is `rm`), a tole binary is never let
//! through (the session guard runs next, and so does it on a `#!` line), and a
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
use std::path::Path;

/// The longest `#!` line read.
const SHEBANG_MAX: u64 = 256;

fn esc(s: &str) -> String {
    s.chars().take(120).flat_map(char::escape_debug).collect()
}

fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

fn is_tole(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.strip_suffix(".exe").unwrap_or(&n), "tole" | "tole-cli")
}

/// [`check_destructive_argv`] for the tools that run `argv` from `cwd`, where
/// `argv[0]` may be a path (see the module text). `Err` carries the reason.
pub fn check_command_argv(argv: &[String], cwd: &Path) -> Result<(), String> {
    let Some(prog) = argv.first().filter(|p| p.contains('/')) else {
        return check_destructive_argv(argv);
    };
    // A tole binary is never let through, found or not (the session guard).
    if is_tole(base(prog)) {
        return check_destructive_argv(argv);
    }
    // What exec will run: a relative path is read from `cwd`, `~` is no shell's.
    let file = cwd.join(prog).canonicalize().ok().filter(|f| runnable(f));
    let Some(file) = file else {
        return Err(format!(
            "refused \u{2014} the program is written as a path ({}) but no executable file is there (a relative path is read from the workspace, {}; there is no shell, so `~` and `$VAR` are not expanded).",
            esc(prog),
            cwd.display()
        ));
    };
    // The blocklist reads names: the one written and the one it resolves to.
    let Some(real) = file
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !is_tole(n))
    else {
        return Err(format!(
            "refused \u{2014} the program ({}) resolves to a file named tole (a session may not run the tole binary) or to a name that cannot be read.",
            esc(prog)
        ));
    };
    let mut by_name = argv.to_vec();
    for name in [base(prog), real] {
        by_name[0] = name.to_string();
        check_destructive_argv(&by_name)?;
    }
    // A `#!` line names the program that really runs: the same rules for it.
    if let Some(line) = shebang(&file) {
        let words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        let named = words.first().is_some_and(|w| is_tole(base(w)));
        let mut inner = words.clone();
        if let Some(w) = inner.first_mut() {
            *w = base(w).to_string();
        }
        let bad = named
            .then(|| "a session may not run the tole binary".to_string())
            .or_else(|| crate::session_guard::check_session_argv(&words, cwd).err())
            .or_else(|| check_destructive_argv(&inner).err());
        if let Some(why) = bad {
            return Err(format!(
                "refused \u{2014} {} starts with a `#!` line that is refused: {why}",
                esc(prog)
            ));
        }
    }
    Ok(())
}

/// A regular file with an execute bit (any file where there is no such bit).
fn runnable(f: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(f) else {
        return false;
    };
    #[cfg(unix)]
    let exec = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let exec = true;
    meta.is_file() && exec
}

/// The `#!` line of `f` (without the marker), when it has one.
fn shebang(f: &Path) -> Option<String> {
    let mut head = Vec::new();
    std::fs::File::open(f)
        .ok()?
        .take(SHEBANG_MAX)
        .read_to_end(&mut head)
        .ok()?;
    let text = String::from_utf8_lossy(head.strip_prefix(b"#!")?).into_owned();
    Some(text.lines().next().unwrap_or_default().to_string())
}
