//! Find a program on `PATH` for a test that needs a real one, instead of
//! hard-coding `/bin/rm` or `/usr/bin/env` (their places differ on Alpine and
//! NixOS). A test that cannot find it skips the case and says why.

#![allow(dead_code)]

use std::path::PathBuf;

/// The first `name` on `PATH` that resolves to an executable regular file.
pub fn find_exe(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.canonicalize().is_ok_and(|f| f.is_file()));
    if found.is_none() {
        eprintln!("skipped: no `{name}` on PATH");
    }
    found
}

/// [`find_exe`], also an ordinary file named `name` that is not a script: a
/// multicall binary (busybox, uutils) or a wrapper is not what the case means.
pub fn find_real(name: &str) -> Option<PathBuf> {
    let found = find_exe(name)?;
    let file = found.canonicalize().ok()?;
    let plain = file.file_name().is_some_and(|n| n == name)
        && std::fs::read(&file).is_ok_and(|b| !b.starts_with(b"#!"));
    if !plain {
        eprintln!("skipped: `{name}` on this host is not an ordinary binary named {name}");
    }
    plain.then_some(found)
}
