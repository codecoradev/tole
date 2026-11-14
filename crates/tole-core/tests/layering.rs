//! Layering test (#752): a lower level must not import a higher one.
//!
//! Every top-level module of tole-core is assigned a level below. The test
//! scans `src/**/*.rs` for `crate::<module>` paths (plain, and inside
//! `use crate::{a, b::C}` groups) and fails when a module of a lower level
//! imports a module of a higher level, unless the edge is in [`BASELINE`].
//!
//! Limits of the scan (it is line-based, std only, no parser):
//! - paths produced by macros (`$crate::`, macro-generated `use`) are not seen;
//! - `use super::` / `self::` edges stay inside one module and are ignored;
//! - doc and `//` comments are skipped; the trailing `#[cfg(test)] mod ...`
//!   block of a file and test files (`*_tests.rs`, `tests.rs`, `tests/`)
//!   are skipped, so test-only imports are not layering edges;
//! - `crate::foo` inside a string literal would count (none exist today).
//!
//! The baseline may only shrink: see `baseline_has_no_stale_entries`.
//! New violations are not allowed; fix the dependency instead of adding one.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Kernel: session model, storage, paths, secrets, sensitive-data helpers,
/// provider seam and wire types, ledger, approval and the turn loop.
/// Engines: delegation, discussion, router, advisor, skills, curator, web,
/// MCP client, agents and the shell/vcs tools built on subprocess helpers.
/// Faces: serve-side modules (only a host's serve surface uses them).
#[rustfmt::skip]
const KERNEL_MODULES: &[&str] = &[
    "entry", "register", "state", "storage", "machine", "paths", "secrets", "secret_release",
    "sensitive", "sanitize", "safe_dir", "tool", "approval", "provider", "openai", "mock",
    "failover", "cancel", "interrupt", "image", "ledger", "cron", "tz", "webhook", "transcript",
    "turn", "compaction", "gate", "replay", "hooks", "subprocess", "process_guard",
    "resource_guard", "sandbox", "file_tools", "read_file", "todo", "systemone",
];
#[rustfmt::skip]
const ENGINE_MODULES: &[&str] = &[
    "delegation", "delegation_approvals", "delegation_report", "discussion", "discussion_log",
    "router", "advisor", "skills", "skill_approval", "skill_curator", "skill_draft",
    "skill_index", "web", "mcp", "plugin_lock", "agents", "child_env", "handoff",
    "sibling_watch", "session_guard", "run_command", "jobs", "memory", "uteke", "gh", "git",
    "gitea", "cora_search", "verify_package", "ccimport",
];
#[rustfmt::skip]
const FACE_MODULES: &[&str] = &["mcp_server", "update_check"];

/// Known violations (importer, imported, reason). Only ever shrinks.
#[rustfmt::skip]
const BASELINE: &[(&str, &str, &str)] = &[
    ("approval", "run_command", "allowlist patterns reuse RunCommandTool::split_argv"),
    ("gate", "skill_draft", "the gate carries the skill-draft TaintTracker"),
    ("tool", "skill_draft", "ToolContext stores the skill-draft TaintTracker"),
    ("secrets", "web", "header secret lookup calls web::header_secrets"),
];

fn level_of(module: &str) -> Option<u8> {
    [KERNEL_MODULES, ENGINE_MODULES, FACE_MODULES]
        .iter()
        .position(|level| level.contains(&module))
        .map(|i| i as u8)
}

fn level_name(level: u8) -> &'static str {
    ["kernel", "engines", "faces"][level as usize]
}

/// Top-level modules: `src/x.rs` and `src/x/`, minus lib and test files.
fn top_level_modules(src: &Path) -> BTreeSet<String> {
    let mut mods = BTreeSet::new();
    for entry in fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let is_rs = path.extension().is_some_and(|e| e == "rs");
        if (path.is_dir() || is_rs) && stem != "lib" && !is_test_name(&stem) {
            mods.insert(stem);
        }
    }
    mods
}

fn is_test_name(stem: &str) -> bool {
    stem == "tests" || stem.ends_with("_tests")
}

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        if path.is_dir() {
            if stem != "tests" {
                rs_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") && !is_test_name(&stem) {
            out.push(path);
        }
    }
}

/// Source text without `//` comments and without the trailing test module.
fn production_text(file: &Path) -> String {
    let lines: Vec<String> = fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let cut = (0..lines.len())
        .find(|&i| {
            lines[i].trim() == "#[cfg(test)]"
                && lines[i + 1..]
                    .iter()
                    .take(3)
                    .any(|l| l.trim_start().starts_with("mod "))
        })
        .unwrap_or(lines.len());
    lines[..cut]
        .iter()
        .filter(|l| !l.trim_start().starts_with("//"))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Leading identifier of a path fragment: `foo::Bar` -> `foo`.
fn head(s: &str) -> &str {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    &s[..end]
}

/// Top-level module names named after each `crate::` in `text`.
fn crate_targets(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("crate::") {
        let before = rest[..pos].chars().next_back();
        rest = &rest[pos + "crate::".len()..];
        if before.is_some_and(|c| c == '$' || c.is_alphanumeric() || c == '_') {
            continue; // `$crate::` or `foo_crate::`
        }
        if let Some(group) = rest.strip_prefix('{') {
            let mut depth = 1;
            let mut item_start = 0;
            for (i, c) in group.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    ',' if depth == 1 => {
                        found.push(head(&group[item_start..i]).to_owned());
                        item_start = i + 1;
                    }
                    _ => {}
                }
                if depth == 0 {
                    found.push(head(&group[item_start..i]).to_owned());
                    break;
                }
            }
        } else {
            found.push(head(rest).to_owned());
        }
    }
    found
}

/// Edges (importer, imported) between distinct top-level modules.
fn scan_edges() -> BTreeMap<(String, String), BTreeSet<String>> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mods = top_level_modules(&src);
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let mut edges: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for file in files {
        let rel = file.strip_prefix(&src).unwrap();
        let from = rel
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy();
        let from = from.trim_end_matches(".rs").to_owned();
        if !mods.contains(&from) {
            continue; // lib.rs
        }
        for to in crate_targets(&production_text(&file)) {
            if to != from && mods.contains(&to) {
                edges
                    .entry((from.clone(), to))
                    .or_default()
                    .insert(rel.display().to_string());
            }
        }
    }
    edges
}

fn is_violation(from: &str, to: &str) -> bool {
    level_of(from).zip(level_of(to)).is_some_and(|(f, t)| f < t)
}

fn report(edges: &[(&(String, String), &BTreeSet<String>)]) -> String {
    let mut out = String::new();
    for ((from, to), files) in edges {
        let files: Vec<_> = files.iter().cloned().collect();
        out += &format!(
            "  {from} ({}) -> {to} ({}) [{}]\n",
            level_name(level_of(from).unwrap_or(0)),
            level_name(level_of(to).unwrap_or(0)),
            files.join(", ")
        );
    }
    out
}

#[test]
fn every_module_has_a_level() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mods = top_level_modules(&src);
    let missing: Vec<_> = mods.iter().filter(|m| level_of(m).is_none()).collect();
    assert!(
        missing.is_empty(),
        "modules without a level in the level lists: {missing:?}"
    );
    let all = [KERNEL_MODULES, ENGINE_MODULES, FACE_MODULES].concat();
    let stale: Vec<_> = all.iter().filter(|m| !mods.contains(**m)).collect();
    assert!(
        stale.is_empty(),
        "level lists name modules that no longer exist: {stale:?}"
    );
}

#[test]
fn lower_levels_do_not_import_higher_levels() {
    let edges = scan_edges();
    let new: Vec<_> = edges
        .iter()
        .filter(|((from, to), _)| {
            is_violation(from, to) && !BASELINE.iter().any(|(f, t, _)| f == from && t == to)
        })
        .collect();
    assert!(
        new.is_empty(),
        "new layering violations (a lower level imports a higher one; \
         fix the dependency, do not extend BASELINE):\n{}",
        report(&new)
    );
}

#[test]
fn baseline_has_no_stale_entries() {
    let edges = scan_edges();
    let stale: Vec<_> = BASELINE
        .iter()
        .filter(|(f, t, _)| {
            !(is_violation(f, t) && edges.contains_key(&(f.to_string(), t.to_string())))
        })
        .map(|(f, t, reason)| format!("  {f} -> {t} ({reason})"))
        .collect();
    assert!(
        stale.is_empty(),
        "BASELINE entries that are no longer violations; remove them:\n{}",
        stale.join("\n")
    );
}

#[test]
fn scanner_reads_plain_group_and_nested_paths() {
    let text = "use crate::a::B;\nuse crate::{c, d::{E, F}, g as h};\nlet x = $crate::z::y; crate::i::j();";
    assert_eq!(crate_targets(text), ["a", "c", "d", "g", "i"]);
}
