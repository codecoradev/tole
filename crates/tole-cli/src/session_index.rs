//! Global session index (#439): which project a session lives in, so
//! `tole sessions --all` lists sessions across projects and `tole resume
//! <id>` works from any directory.
//!
//! One append-only JSONL file, `$CODECORA_HOME/tole/sessions-index.jsonl`
//! (default `~/.codecora/tole/`, next to the config trust store), owner-only.
//! Each line is one session START: id, the absolute session file, the cwd,
//! the face and the start time. Nothing a session SAYS is copied here:
//! the preview, status and turn count are read from the session file itself
//! when listing, so the index never holds prompt text. A later line for the
//! same id wins; a malformed or truncated line is skipped, never fatal. No
//! daemon, no lockfile: each record is one `write` of one line in append
//! mode. Recording is best effort and never fails a session;
//! `TOLE_NO_SESSION_INDEX=1` turns it off.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

use std::path::{Path, PathBuf};
use tole_core::storage::{JsonlStorage, Storage};

/// Index file name under the tole data dir.
pub const INDEX_FILE: &str = "sessions-index.jsonl";
/// Set to `1` to stop recording sessions in the index.
pub const DISABLE_ENV: &str = "TOLE_NO_SESSION_INDEX";

/// Longest first-prompt preview shown by `sessions --all`, in chars.
const PREVIEW_CHARS: usize = 60;

/// One indexed session start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Session id (`s-…`).
    pub id: String,
    /// Absolute path of the session's JSONL file.
    pub file: PathBuf,
    /// The working directory the session was started from.
    pub cwd: PathBuf,
    /// The face that started it (`run`, `chat`, `mission`, `serve`, ...).
    pub face: String,
    /// Start time, Unix seconds.
    pub created_at: u64,
}

/// The index path: `$CODECORA_HOME/tole/sessions-index.jsonl`.
pub fn index_path() -> Option<PathBuf> {
    tole_core::paths::tole_data_dir().map(|d| d.join(INDEX_FILE))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record a session start in the default index (best effort: any failure
/// is ignored, a session never depends on the index).
pub fn record(sessions_dir: &Path, id: &str, face: &str) {
    if std::env::var(DISABLE_ENV).is_ok_and(|v| v.trim() == "1") {
        return;
    }
    let Some(index) = index_path() else {
        return;
    };
    let file = sessions_dir.join(format!("{id}.jsonl"));
    let file = file.canonicalize().unwrap_or(file);
    let cwd = std::env::current_dir().unwrap_or_default();
    let _ = append(
        &index,
        &Entry {
            id: id.to_string(),
            file,
            cwd,
            face: face.to_string(),
            created_at: now_secs(),
        },
    );
}

/// Append one entry as one line (one `write`, append mode, owner-only file).
pub fn append(index: &Path, entry: &Entry) -> std::io::Result<()> {
    if let Some(dir) = index.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(index)?;
    f.write_all(line.as_bytes())
}

/// Every session in the index, one per id (the LAST line for an id wins),
/// newest first. A missing index is empty; bad lines are skipped.
pub fn load(index: &Path) -> Vec<Entry> {
    let Ok(f) = std::fs::File::open(index) else {
        return Vec::new();
    };
    let mut by_id: std::collections::HashMap<String, Entry> = std::collections::HashMap::new();
    // Raw byte lines: a torn or non-UTF-8 line is skipped like any other
    // bad line, never the end of the index.
    for line in std::io::BufReader::new(f).split(b'\n') {
        let Ok(line) = line else { break }; // a read error, not bad content
        if let Ok(e) = serde_json::from_slice::<Entry>(&line) {
            by_id.insert(e.id.clone(), e);
        }
    }
    let mut all: Vec<Entry> = by_id.into_values().collect();
    all.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    all
}

/// The indexed session with exactly this id whose file still exists AND
/// is named `<id>.jsonl` — so its directory resolves the id locally; an
/// entry pointing anywhere else (a corrupt or hand-edited line) is ignored.
pub fn find(index: &Path, id: &str) -> Option<Entry> {
    let expected = format!("{id}.jsonl");
    load(index).into_iter().find(|e| {
        e.id == id && e.file.file_name().is_some_and(|n| n == expected.as_str()) && e.file.is_file()
    })
}

/// The command that resumes an indexed session in its own project:
/// `cd <cwd> && tole resume <id>`, plus `--sessions-dir` when the session
/// file is not in that project's default `.tole/sessions`. Paths are
/// single-quoted for a POSIX shell when they need it.
pub fn resume_hint(e: &Entry) -> String {
    fn quote(p: &Path) -> String {
        let s = p.display().to_string();
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c))
        {
            s
        } else {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
    }
    let dir = e.file.parent().unwrap_or(Path::new("."));
    let default = e.cwd.join(".tole/sessions");
    let same = dir.canonicalize().ok() == default.canonicalize().ok();
    let flag = if same {
        String::new()
    } else {
        format!(" --sessions-dir {}", quote(dir))
    };
    format!("cd {} && tole{flag} resume {}", quote(&e.cwd), e.id)
}

/// What `sessions --all` shows for one session, read from its own file.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub entry: Entry,
    pub name: Option<String>,
    pub pc: String,
    pub turns: usize,
    /// The first user prompt, one line, at most [`PREVIEW_CHARS`] chars.
    pub preview: String,
}

/// The listing rows, newest first: entries whose file is gone are left
/// out, and `grep` (case-insensitive) keeps sessions whose first prompt,
/// name or cwd contains it.
pub fn rows(index: &Path, grep: Option<&str>) -> Vec<Row> {
    let needle = grep.map(str::to_lowercase);
    load(index)
        .into_iter()
        .filter_map(|entry| {
            let s = JsonlStorage::open_read_only(&entry.file).ok()?;
            let first = s
                .entries()
                .iter()
                .find(|e| tole_core::transcript::opens_turn(e))
                .and_then(|e| e.payload["text"].as_str())
                .unwrap_or("")
                .to_string();
            let row = Row {
                name: s.session_name().map(str::to_string),
                pc: format!("{:?}", s.state().pc),
                turns: tole_core::transcript::turn_count(s.entries()),
                preview: preview(&first),
                entry,
            };
            let keep = needle.as_deref().is_none_or(|n| {
                first.to_lowercase().contains(n)
                    || row
                        .name
                        .as_deref()
                        .is_some_and(|x| x.to_lowercase().contains(n))
                    || row.entry.cwd.to_string_lossy().to_lowercase().contains(n)
            });
            keep.then_some(row)
        })
        .collect()
}

/// One session as the fork tree sees it (#210).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode {
    pub id: String,
    /// The session this one was forked from, if it is a fork.
    pub parent: Option<String>,
    /// The entry it was cut at.
    pub at: Option<String>,
    pub pc: String,
    pub turns: usize,
}

/// The `sessions --tree` lines: roots (and forks whose source is gone)
/// sorted by id, each fork nested under its source with `└─`.
pub fn tree_lines(nodes: Vec<TreeNode>) -> Vec<String> {
    tree_order(nodes)
        .into_iter()
        .map(|(depth, n)| {
            let label = match &n.at {
                Some(at) => format!("{} (@{at})", n.id),
                None => n.id.clone(),
            };
            let pad = if depth == 0 {
                String::new()
            } else {
                format!("{}└─ ", "   ".repeat(depth - 1))
            };
            format!("{pad}{label}  {}  {} turns", n.pc, n.turns)
        })
        .collect()
}

/// Every session file in `dir` as a [`TreeNode`], read-only (no writer lock,
/// no repair): what `sessions --tree` and the TUI's `/tree` are built from.
/// Unreadable files and ids outside the safe charset are skipped.
pub fn tree_nodes(dir: &Path) -> Vec<TreeNode> {
    let mut nodes = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return nodes;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !tole_core::storage::is_valid_session_id(stem) {
            continue;
        }
        let Ok(s) = JsonlStorage::open_read_only(entry.path()) else {
            continue;
        };
        nodes.push(TreeNode::of(stem, &s));
    }
    nodes
}

impl TreeNode {
    /// The node for an already-open session.
    pub fn of(id: &str, s: &JsonlStorage) -> TreeNode {
        TreeNode {
            id: id.to_string(),
            parent: s.forked_from().map(|o| o.session.clone()),
            at: s.forked_from().map(|o| o.entry.clone()),
            pc: format!("{:?}", s.state().pc),
            turns: tole_core::transcript::turn_count(s.entries()),
        }
    }
}

/// Depth-first order of the fork forest: roots (and forks whose source is
/// gone) sorted by id, each fork under its source. Depth 0 = a root.
pub fn tree_order(mut nodes: Vec<TreeNode>) -> Vec<(usize, TreeNode)> {
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let known: std::collections::HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let is_root = |n: &TreeNode| n.parent.as_ref().is_none_or(|p| !known.contains(p));
    fn walk(n: &TreeNode, all: &[TreeNode], depth: usize, out: &mut Vec<(usize, TreeNode)>) {
        out.push((depth, n.clone()));
        // A forked file cannot cycle (its id is new), but bound the walk anyway.
        if depth < 64 {
            for c in all
                .iter()
                .filter(|c| c.parent.as_deref() == Some(n.id.as_str()))
            {
                walk(c, all, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for n in nodes.iter().filter(|n| is_root(n)) {
        walk(n, &nodes, 0, &mut out);
    }
    out
}

/// The family of `current` (#463 `/tree`): the whole fork tree that contains
/// it, i.e. its parent chain up to the root and every fork below that root
/// (siblings and grandchildren included), in [`tree_order`] with depths
/// relative to that root. Empty when `current` is not among `nodes`.
pub fn family(nodes: Vec<TreeNode>, current: &str) -> Vec<(usize, TreeNode)> {
    let parent_of: std::collections::HashMap<&str, &str> = nodes
        .iter()
        .filter_map(|n| n.parent.as_deref().map(|p| (n.id.as_str(), p)))
        .collect();
    let known: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    if !known.contains(current) {
        return Vec::new();
    }
    let mut root = current;
    // Bounded: ids are unique files, so a cycle is corruption, not a case.
    for _ in 0..64 {
        match parent_of.get(root) {
            Some(p) if known.contains(p) => root = p,
            _ => break,
        }
    }
    let root = root.to_string();
    let mut base = None;
    let mut out = Vec::new();
    for (d, n) in tree_order(nodes) {
        match base {
            None if n.id == root => {
                base = Some(d);
                out.push((0, n));
            }
            // The root's subtree is contiguous: it ends at the first row
            // that is not deeper than the root.
            Some(b) if d > b => out.push((d - b, n)),
            Some(_) => break,
            None => {}
        }
    }
    out
}

/// One-line, control-free, bounded preview.
fn preview(text: &str) -> String {
    // Control, bidi and separator characters become spaces, runs collapse:
    // a preview can never move the terminal cursor or reorder the line.
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let line = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let total = line.chars().count();
    if total > PREVIEW_CHARS {
        let head: String = line.chars().take(PREVIEW_CHARS).collect();
        format!("{head}…")
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tole-session-index-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn entry(id: &str, file: &Path, at: u64) -> Entry {
        Entry {
            id: id.into(),
            file: file.to_path_buf(),
            cwd: PathBuf::from("/work/proj"),
            face: "run".into(),
            created_at: at,
        }
    }

    #[test]
    fn last_line_wins_newest_first_and_bad_lines_are_skipped() {
        let d = tmp();
        let idx = d.join("idx.jsonl");
        append(&idx, &entry("s-a", &d.join("a.jsonl"), 10)).unwrap();
        append(&idx, &entry("s-b", &d.join("b.jsonl"), 20)).unwrap();
        // A torn / foreign line must not break the index.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&idx)
            .unwrap()
            .write_all(b"{\"id\": \"s-x\", trunc\nnot json\n")
            .unwrap();
        // Non-UTF-8 garbage mid-file must not hide the lines after it.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&idx)
            .unwrap()
            .write_all(b"\xff\xfe\x00 torn\n")
            .unwrap();
        append(&idx, &entry("s-a", &d.join("a2.jsonl"), 30)).unwrap();
        let all = load(&idx);
        let ids: Vec<&str> = all.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["s-a", "s-b"]);
        assert_eq!(all[0].file, d.join("a2.jsonl"));
        assert!(load(&d.join("missing.jsonl")).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&idx).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "owner-only");
        }
    }

    #[test]
    fn rows_read_the_session_file_and_skip_gone_sessions() {
        let d = tmp();
        let idx = d.join("idx.jsonl");
        let mut s = JsonlStorage::create(&d, "s-live", None).unwrap();
        use tole_core::entry::{EntryType, NewEntry};
        s.commit(tole_core::storage::Commit::new().entry(NewEntry::root(
            EntryType::new(EntryType::MESSAGE),
            serde_json::json!({"role": "user", "text": "fix the\nflaky parser test please, it fails on CI every other run"}),
        )))
        .unwrap();
        append(&idx, &entry("s-live", &d.join("s-live.jsonl"), 5)).unwrap();
        append(&idx, &entry("s-gone", &d.join("s-gone.jsonl"), 9)).unwrap();
        let rows = rows(&idx, None);
        assert_eq!(rows.len(), 1, "a deleted session is not listed");
        assert_eq!(rows[0].turns, 1);
        assert!(
            rows[0].preview.starts_with("fix the flaky parser"),
            "{:?}",
            rows[0]
        );
        assert!(rows[0].preview.ends_with('…'));
        assert!(!rows[0].preview.contains('\n'));
        assert_eq!(super::rows(&idx, Some("PARSER")).len(), 1);
        assert_eq!(super::rows(&idx, Some("proj")).len(), 1, "cwd matches");
        assert!(super::rows(&idx, Some("nothing-like-this")).is_empty());
        assert_eq!(find(&idx, "s-live").unwrap().id, "s-live");
        assert!(find(&idx, "s-gone").is_none());
        // The resume hint names the project dir (and a non-default
        // sessions dir).
        let e = find(&idx, "s-live").unwrap();
        assert_eq!(
            resume_hint(&e),
            format!(
                "cd /work/proj && tole --sessions-dir {} resume s-live",
                d.display()
            )
        );
        let spaced = Entry {
            cwd: PathBuf::from("/work/my proj"),
            ..e.clone()
        };
        assert!(resume_hint(&spaced).starts_with("cd '/work/my proj' && "));
        // An entry whose file is not `<id>.jsonl` never resolves the id.
        append(&idx, &entry("s-other", &d.join("s-live.jsonl"), 7)).unwrap();
        assert!(find(&idx, "s-other").is_none());
    }

    fn node(id: &str, parent: Option<&str>) -> TreeNode {
        TreeNode {
            id: id.into(),
            parent: parent.map(str::to_string),
            at: parent.map(|_| "e1".to_string()),
            pc: "Final".into(),
            turns: 1,
        }
    }

    fn ids(v: &[(usize, TreeNode)]) -> Vec<(usize, &str)> {
        v.iter().map(|(d, n)| (*d, n.id.as_str())).collect()
    }

    /// #463: the family is the whole tree around the session, depths are
    /// relative to its root, other trees and the rest of the forest are out.
    #[test]
    fn family_is_the_tree_around_a_session() {
        let nodes = || {
            vec![
                node("a", None),
                node("b", Some("a")),
                node("c", Some("a")),
                node("d", Some("b")),
                node("x", None),
                node("y", Some("x")),
                // a fork whose source file is gone is its own root
                node("o", Some("gone")),
                node("p", Some("o")),
            ]
        };
        let want = [(0, "a"), (1, "b"), (2, "d"), (1, "c")];
        for cur in ["a", "b", "c", "d"] {
            assert_eq!(ids(&family(nodes(), cur)), want, "from {cur}");
        }
        assert_eq!(ids(&family(nodes(), "y")), [(0, "x"), (1, "y")]);
        assert_eq!(ids(&family(nodes(), "p")), [(0, "o"), (1, "p")]);
        assert!(family(nodes(), "missing").is_empty());
        // a corrupt parent cycle terminates
        let cyc = vec![node("m", Some("n")), node("n", Some("m"))];
        let _ = family(cyc, "m");
    }
}
