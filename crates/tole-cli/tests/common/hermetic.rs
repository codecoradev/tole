//! Hermetic environment for tests that spawn the `tole` binary (#993).
//!
//! A child process inherits the developer's real `HOME`, `PATH` and
//! `CODECORA_HOME` unless a test says otherwise. On a machine that has the
//! sibling tools (`cora`, `uteke`) and a `~/.codecora`, such a test then reads
//! and writes real state, can stall on a first-run model download, and can
//! fail where CI (which has neither tool) passes. Build every `Command` that
//! runs the binary, or code that spawns external tools, through this module.
//!
//! ```ignore
//! let home = dir.join("home");
//! let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &home, &[
//!     ("TOLE_BASE_URL", base.as_str()),
//! ]);
//! cmd.args(["run", "hello"]).current_dir(&dir);
//! ```

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

/// The sibling tools tole probes on `PATH`; a directory holding either is
/// dropped from the child's `PATH`.
const SIBLING_BINARIES: [&str; 2] = ["cora", "uteke"];

/// XDG overrides that would point the child outside the test home.
const XDG_VARS: [&str; 4] = [
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// `path` minus every directory that holds a `cora` or `uteke` executable.
/// The rest is kept so `sh`, `git` and the like still resolve.
pub fn path_without_sibling_binaries_from(path: &OsStr) -> OsString {
    let kept: Vec<_> = std::env::split_paths(path)
        .filter(|dir| !SIBLING_BINARIES.iter().any(|n| dir.join(n).is_file()))
        .collect();
    std::env::join_paths(kept).unwrap_or_default()
}

/// The inherited `PATH` without sibling-tool directories.
pub fn path_without_sibling_binaries() -> OsString {
    std::env::var_os("PATH")
        .map(|p| path_without_sibling_binaries_from(&p))
        .unwrap_or_default()
}

/// Make `cmd` hermetic: `HOME` and `CODECORA_HOME` are `home` (created if
/// missing), `PATH` has no directory with `cora` or `uteke`, and XDG
/// overrides are removed so nothing resolves outside `home`. `extra` is
/// applied last, so a test that wants a decoy (its own shim on `PATH`, another
/// `HOME`) overrides the default there.
pub fn apply_hermetic_env(cmd: &mut Command, home: &Path, extra: &[(&str, &str)]) {
    std::fs::create_dir_all(home).expect("create the test home");
    cmd.env("HOME", home)
        .env("CODECORA_HOME", home)
        .env("PATH", path_without_sibling_binaries());
    for var in XDG_VARS {
        cmd.env_remove(var);
    }
    for (k, v) in extra {
        cmd.env(k, v);
    }
}

/// A `Command` for `program` with the hermetic environment applied.
pub fn hermetic_command(
    program: impl AsRef<OsStr>,
    home: &Path,
    extra: &[(&str, &str)],
) -> Command {
    let mut cmd = Command::new(program);
    apply_hermetic_env(&mut cmd, home, extra);
    cmd
}
