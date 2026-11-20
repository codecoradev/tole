//! One way to run a turn (#944, epic #941): outside `tole-core`, only
//! `session/runner.rs` (the `turn_runner`) calls the core turn functions.
//! The CLI `run`/`resume`/`chat`, mission, cron and the doctor probe all go
//! through it, so whatever the runner adds applies on every face.
//!
//! A source scan on purpose (like `evals/replay`): a new direct call fails
//! here, in review, instead of drifting in silently. Test modules (from the
//! first `#[cfg(test)]` on) are not scanned: a unit test may drive the core
//! loop with a scripted provider.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// True when the (whitespace-joined) production code of `src` reaches a core
/// turn function: a path `tole_core::turn::run_turn…` / `resume_turn…`, or a
/// `use tole_core::turn::{…}` list naming one.
fn calls_core_turn(src: &str) -> bool {
    let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
    let flat = prod.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut rest = flat.as_str();
    while let Some(at) = rest.find("tole_core::turn::") {
        rest = &rest[at + "tole_core::turn::".len()..];
        let stmt = rest.split(';').next().unwrap_or(rest);
        let head: String = stmt.chars().take(200).collect();
        if head.contains("run_turn") || head.contains("resume_turn") {
            return true;
        }
    }
    false
}

#[test]
fn only_the_turn_runner_calls_the_core_turn_functions() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(
        files.len() > 20,
        "scan found too few files: {}",
        files.len()
    );
    let runner = src.join("session").join("runner.rs");
    let offenders: Vec<_> = files
        .iter()
        .filter(|f| **f != runner)
        .filter(|f| calls_core_turn(&std::fs::read_to_string(f).unwrap()))
        .map(|f| f.strip_prefix(&src).unwrap().display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "these files call tole_core::turn directly; use turn_runner: {offenders:?}"
    );
}

#[test]
fn the_scan_recognizes_the_shapes_it_guards() {
    assert!(calls_core_turn("use tole_core::turn::run_turn;"));
    assert!(calls_core_turn(
        "use tole_core::turn::{\n    resume_turn_with_cancel, TurnOutcome,\n};"
    ));
    assert!(calls_core_turn(
        "let o = tole_core::turn::run_turn_with_images(a);"
    ));
    assert!(!calls_core_turn(
        "use tole_core::turn::{TurnOutcome, LOOP_TRIP_AFTER};"
    ));
    // Test modules are not scanned.
    assert!(!calls_core_turn(
        "fn f() {}\n#[cfg(test)]\nmod t { use tole_core::turn::run_turn; }"
    ));
}
