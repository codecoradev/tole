//! #993: every test file that spawns the `tole` binary builds its environment
//! through the shared helper (`common::hermetic_command`,
//! `common::apply_hermetic_env` or `common::path_without_sibling_binaries`),
//! or is listed here with the reason its own environment is hermetic.
//!
//! Why: a child inherits the developer's real `HOME`, `PATH` and
//! `CODECORA_HOME`. On a machine that has `cora`, `uteke` and a `~/.codecora`,
//! a test that does not scrub them can read and write real state or stall on a
//! first-run download, and it passes in CI, which has neither. A new test file
//! that forgets the helper fails here, on any machine, without needing one
//! that has those tools.
//!
//! A file that really needs its own environment goes in `OWN_ENVIRONMENT` with
//! the reason; an entry whose file now uses the helper (or no longer exists)
//! fails too, so the list cannot go stale.

use std::path::Path;

/// Files that spawn the binary with an environment of their own, and why that
/// environment is hermetic.
const OWN_ENVIRONMENT: &[(&str, &str)] = &[
    (
        "cron_sandbox.rs",
        "env_clear, HOME and CODECORA_HOME of its own dir, an explicit PATH",
    ),
    (
        "cron_user_config_e2e.rs",
        "env_clear, HOME and CODECORA_HOME of its own dir, its own bin dir as PATH",
    ),
    (
        "user_config_absent_e2e.rs",
        "env_clear and PATH=/usr/bin:/bin; it tests an absent or broken CODECORA_HOME",
    ),
    (
        "sessions_profile_e2e.rs",
        "HOME and CODECORA_HOME of its own dir and PATH=/usr/bin:/bin",
    ),
    (
        "desktop_supervisor.rs",
        "a wrapper script that exports HOME, CODECORA_HOME and a PATH without the sibling tools",
    ),
];

/// Names that mean "this file uses the shared helper".
const HELPER: &[&str] = &[
    "hermetic_command(",
    "apply_hermetic_env(",
    "path_without_sibling_binaries(",
];

fn test_files() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("the tests dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&p).expect("a test file");
            (name, text)
        })
        .collect();
    files.sort();
    files
}

/// Does the file spawn the binary? (This file names it only in a comment.)
fn spawns_the_binary(name: &str, text: &str) -> bool {
    name != "hermetic_audit.rs" && text.contains("CARGO_BIN_EXE_tole")
}

#[test]
fn a_test_file_that_spawns_the_binary_uses_the_helper_or_says_why_not() {
    let mut bare = Vec::new();
    for (name, text) in test_files() {
        if !spawns_the_binary(&name, &text) {
            continue;
        }
        let uses_helper = HELPER.iter().any(|h| text.contains(h));
        let listed = OWN_ENVIRONMENT.iter().any(|(f, _)| *f == name);
        if !uses_helper && !listed {
            bare.push(name);
        }
    }
    assert!(
        bare.is_empty(),
        "these test files spawn `tole` with the developer's own HOME and PATH: build the \
         Command with `common::hermetic_command(env!(\"CARGO_BIN_EXE_tole\"), &home, &[..])` \
         (see CONTRIBUTING.md, Hermetic e2e tests), or list the file in OWN_ENVIRONMENT with \
         the reason its environment is hermetic: {bare:?}"
    );
}

#[test]
fn the_list_of_own_environments_has_no_stale_entry() {
    let files = test_files();
    for (listed, reason) in OWN_ENVIRONMENT {
        assert!(!reason.is_empty(), "{listed}: a reason is required");
        let Some((_, text)) = files.iter().find(|(n, _)| n == listed) else {
            panic!("{listed} is in OWN_ENVIRONMENT but is not a test file any more");
        };
        assert!(
            spawns_the_binary(listed, text),
            "{listed} no longer spawns the binary: remove it from OWN_ENVIRONMENT"
        );
        // A file listed for an environment of its own that now uses the
        // helper everywhere would hide behind its entry: say so.
        let uses_helper = HELPER.iter().any(|h| text.contains(h));
        if listed == &"desktop_supervisor.rs" {
            assert!(
                uses_helper,
                "the wrapper of {listed} takes its PATH from the helper"
            );
        } else {
            assert!(
                !uses_helper,
                "{listed} uses the helper now: remove it from OWN_ENVIRONMENT"
            );
        }
    }
}
