//! `@file` search: a shallow, capped walk of the working directory. No index,
//! no fuzzy match; the panel filters this list as the user types.

use std::path::Path;

/// Panel results shown at most.
pub const MAX_RESULTS: usize = 50;
const MAX_DEPTH: usize = 3;
const MAX_FILES: usize = 2000;
const SKIP: [&str; 3] = [".git", "target", "node_modules"];

/// Relative `/`-separated file paths under `root`, sorted. Hidden entries,
/// `target`, `node_modules` and names carrying control characters (they would
/// reach the editor and the terminal verbatim) are skipped.
pub fn list(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, "", 1, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        // sanitize keeps '\n', which would split one entry across panel rows
        if name.starts_with('.')
            || SKIP.contains(&name.as_str())
            || name.contains('\n')
            || crate::sanitize(&name) != name
        {
            continue;
        }
        let rel = format!("{prefix}{name}");
        match e.file_type() {
            Ok(t) if t.is_file() => out.push(rel),
            Ok(t) if t.is_dir() && depth < MAX_DEPTH => {
                walk(&e.path(), &format!("{rel}/"), depth + 1, out)
            }
            _ => {}
        }
    }
}

/// Case-insensitive substring filter, capped at [`MAX_RESULTS`].
pub fn matching<'a>(files: &'a [String], filter: &str) -> Vec<&'a str> {
    let f = filter.to_lowercase();
    files
        .iter()
        .filter(|p| p.to_lowercase().contains(&f))
        .take(MAX_RESULTS)
        .map(String::as_str)
        .collect()
}
