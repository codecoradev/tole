//! `GET /sessions/index` (#572): the past sessions of this server's own
//! workspace, newest first, as metadata only.
//!
//! What it never does: take a path from the request (the directory is the
//! server's own sessions directory), return transcript text (no prompt,
//! tool output or preview: only the operator's session name, counts and
//! times), or fail the whole list for one bad file (a torn, unreadable,
//! oversized or out-of-workspace file is skipped).
//!
//! The cost is bounded whatever the directory holds: at most
//! [`Limits::max_scan`] directory entries are read (names and `stat` only,
//! no file is opened to order the list), at most one page of files is
//! opened and replayed, never more than [`Limits::max_opens`] per request,
//! none over [`Limits::max_file_bytes`], and at most
//! [`Limits::max_bytes`] bytes in total per request.

use std::path::Path;

use serde_json::{json, Value};
use tole_core::storage::{is_valid_session_id, JsonlStorage, Storage};

/// Default and maximum page size.
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 100;

/// The per-request ceilings (a struct so tests can shrink them).
#[derive(Clone, Copy)]
pub struct Limits {
    /// Directory entries examined.
    pub max_scan: usize,
    /// Session files opened.
    pub max_opens: usize,
    /// A session file larger than this is skipped, never replayed.
    pub max_file_bytes: u64,
    /// Total bytes replayed.
    pub max_bytes: u64,
}

pub const LIMITS: Limits = Limits {
    max_scan: 10_000,
    max_opens: 200,
    max_file_bytes: 16 * 1024 * 1024,
    max_bytes: 64 * 1024 * 1024,
};

/// One candidate: a `<valid id>.jsonl` regular file and its mtime (ms).
struct Cand {
    id: String,
    mtime: u64,
}

/// `(mtime, id)` cursor text: `<mtime_ms>.<id>`; `None` when malformed.
fn parse_cursor(s: &str) -> Option<(u64, String)> {
    let (t, id) = s.split_once('.')?;
    if t.is_empty() || t.len() > 20 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !is_valid_session_id(id) {
        return None;
    }
    Some((t.parse().ok()?, id.to_string()))
}

fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
}

/// Answer `GET /sessions/index?limit=&cursor=`.
///
/// `dir` is the server's sessions directory and `jailed` says it lies inside
/// the workspace by construction (the default `<workspace>/.tole/sessions`);
/// with an operator `--sessions-dir` it may be shared, so each session's
/// recorded folder must lie inside `workspace_root`. `live` maps a session id
/// to its busy flag when it is live in this process.
pub fn list(
    workspace_root: &Path,
    dir: &Path,
    jailed: bool,
    query: &str,
    limits: Limits,
    live: &dyn Fn(&str) -> Option<bool>,
) -> (u16, Value) {
    let limit = match param(query, "limit") {
        None | Some("") => DEFAULT_LIMIT,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n >= 1 => n.min(MAX_LIMIT),
            _ => return (400, json!({"error": "limit must be a positive integer"})),
        },
    };
    let cursor = match param(query, "cursor") {
        None | Some("") => None,
        Some(c) => match parse_cursor(c) {
            Some(k) => Some(k),
            None => return (400, json!({"error": "malformed cursor"})),
        },
    };

    // Names and stat only: bounded by `max_scan` entries.
    let mut cands: Vec<Cand> = Vec::new();
    let mut scan_truncated = false;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for (n, ent) in rd.enumerate() {
            if n >= limits.max_scan {
                scan_truncated = true;
                break;
            }
            let Ok(ent) = ent else { continue };
            let name = ent.file_name().to_string_lossy().to_string();
            let Some(id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if !is_valid_session_id(id) {
                continue;
            }
            let Ok(md) = std::fs::symlink_metadata(ent.path()) else {
                continue;
            };
            if !md.is_file() {
                continue;
            }
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as u64);
            cands.push(Cand {
                id: id.to_string(),
                mtime,
            });
        }
    }
    // Newest first, id as the tie-break: a total order the cursor can resume.
    cands.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
    let start = match &cursor {
        None => 0,
        // the list is (mtime desc, id asc): skip everything up to the cursor
        Some((t, id)) => cands
            .partition_point(|c| c.mtime > *t || (c.mtime == *t && c.id.as_str() <= id.as_str())),
    };

    let mut out: Vec<Value> = Vec::new();
    let (mut opens, mut bytes, mut skipped) = (0usize, 0u64, 0usize);
    let mut last_key: Option<String> = None;
    let mut exhausted = true;
    for c in &cands[start.min(cands.len())..] {
        if out.len() >= limit || opens >= limits.max_opens {
            exhausted = false;
            break;
        }
        let path = dir.join(format!("{}.jsonl", c.id));
        let size = std::fs::symlink_metadata(&path).map_or(u64::MAX, |m| m.len());
        if size > limits.max_file_bytes {
            skipped += 1;
            last_key = Some(format!("{}.{}", c.mtime, c.id));
            continue;
        }
        if bytes.saturating_add(size) > limits.max_bytes {
            exhausted = false;
            break;
        }
        opens += 1;
        bytes += size;
        last_key = Some(format!("{}.{}", c.mtime, c.id));
        let Ok(s) = JsonlStorage::open_read_only(&path) else {
            skipped += 1;
            continue;
        };
        let cwd = s.cwd();
        // Sessions opened by serve record no cwd: in the workspace's own
        // directory their folder is that workspace; in a shared
        // `--sessions-dir` an unknown folder cannot be shown to be inside
        // the workspace, so the session is left out.
        let folder = if cwd.is_empty() {
            jailed.then(|| workspace_root.display().to_string())
        } else {
            let p = Path::new(cwd);
            let p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            p.starts_with(workspace_root).then(|| cwd.to_string())
        };
        let Some(folder) = folder else {
            // another folder's session in a shared directory: never listed
            continue;
        };
        let busy = live(&c.id);
        out.push(json!({
            "id": c.id,
            "name": s.session_name(),
            "folder": folder,
            "lastActivity": c.mtime,
            "entries": s.entries().len(),
            "turns": tole_core::transcript::turn_count(s.entries()),
            "parent": s.forked_from().map(|f| f.session.clone()),
            "live": busy.is_some(),
            "busy": busy.unwrap_or(false),
        }));
    }
    let more = !exhausted;
    let next = if more { last_key } else { None };
    (
        200,
        json!({
            "sessions": out,
            "nextCursor": next,
            "skipped": skipped,
            "scanTruncated": scan_truncated,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::storage::Commit;

    fn make(dir: &Path, id: &str, cwd: &str, name: Option<&str>, secs: u64, text: &str) {
        let mut s = JsonlStorage::create_named(dir, id, Some(cwd.to_string()), None, name).unwrap();
        s.commit(Commit::new().entry(NewEntry::root(
            EntryType::new(EntryType::MESSAGE),
            json!({"role": "user", "text": text}),
        )))
        .unwrap();
        drop(s);
        set_mtime(&dir.join(format!("{id}.jsonl")), secs);
    }

    fn set_mtime(p: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn ids(v: &Value) -> Vec<String> {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn setup(
        name: &str,
    ) -> (
        crate::test_dir::TestDir,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let base = crate::test_dir::TestDir::new(name);
        let ws = base.join("ws");
        let dir = ws.join(".tole/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let ws = ws.canonicalize().unwrap();
        (base, ws.clone(), ws.join(".tole/sessions"))
    }

    fn q(ws: &Path, dir: &Path, jailed: bool, query: &str, l: Limits) -> (u16, Value) {
        list(ws, dir, jailed, query, l, &|id| {
            (id == "s-live").then_some(true)
        })
    }

    #[test]
    fn lists_newest_first_with_paging_metadata_only_and_live_flags() {
        let (_b, ws, dir) = setup("tole-past-shape");
        let cwd = ws.to_str().unwrap();
        make(&dir, "s-old", cwd, None, 1_000, "SECRET-PROMPT-OLD");
        make(
            &dir,
            "s-live",
            cwd,
            Some("my work"),
            3_000,
            "SECRET-PROMPT-LIVE",
        );
        make(&dir, "s-mid", cwd, None, 2_000, "SECRET-PROMPT-MID");
        let (c, v) = q(&ws, &dir, true, "", LIMITS);
        assert_eq!(c, 200);
        assert_eq!(ids(&v), ["s-live", "s-mid", "s-old"]);
        let first = &v["sessions"][0];
        assert_eq!(first["name"], "my work");
        assert_eq!(first["folder"], cwd);
        assert_eq!(first["lastActivity"], 3_000_000);
        assert_eq!(first["turns"], 1);
        assert_eq!(first["live"], true);
        assert_eq!(first["busy"], true);
        assert_eq!(v["sessions"][1]["live"], false);
        assert!(v["nextCursor"].is_null());
        let text = v.to_string();
        assert!(
            !text.contains("SECRET-PROMPT"),
            "no transcript text: {text}"
        );

        // paging by 1 follows the cursor through every session once
        let (mut got, mut cur) = (Vec::new(), String::new());
        loop {
            let (c, v) = q(&ws, &dir, true, &format!("limit=1&cursor={cur}"), LIMITS);
            assert_eq!(c, 200);
            got.extend(ids(&v));
            match v["nextCursor"].as_str() {
                Some(n) => cur = n.to_string(),
                None => break,
            }
        }
        assert_eq!(got, ["s-live", "s-mid", "s-old"]);
    }

    #[test]
    fn equal_mtimes_page_without_loss_or_repeat() {
        let (_b, ws, dir) = setup("tole-past-ties");
        let cwd = ws.to_str().unwrap();
        for id in ["s-a", "s-b", "s-c", "s-d"] {
            make(&dir, id, cwd, None, 5_000, "x");
        }
        let (mut got, mut cur) = (Vec::new(), String::new());
        loop {
            let (_, v) = q(&ws, &dir, true, &format!("limit=3&cursor={cur}"), LIMITS);
            got.extend(ids(&v));
            match v["nextCursor"].as_str() {
                Some(n) => cur = n.to_string(),
                None => break,
            }
        }
        assert_eq!(got, ["s-a", "s-b", "s-c", "s-d"]);
    }

    #[test]
    fn a_bad_limit_or_cursor_is_400_and_a_big_limit_is_clamped() {
        let (_b, ws, dir) = setup("tole-past-400");
        for bad in [
            "cursor=zzz",
            "cursor=12",
            "cursor=12.",
            "cursor=12.../etc",
            "cursor=12.a/b",
            "cursor=-1.s-a",
            "cursor=.s-a",
            "limit=0",
            "limit=abc",
            "limit=-1",
        ] {
            assert_eq!(q(&ws, &dir, true, bad, LIMITS).0, 400, "{bad}");
        }
        assert_eq!(q(&ws, &dir, true, "limit=999999", LIMITS).0, 200);
        assert_eq!(q(&ws, &dir, true, "cursor=1.s-ok", LIMITS).0, 200);
    }

    #[test]
    fn sessions_of_other_folders_in_a_shared_dir_are_never_listed() {
        let (b, ws, dir) = setup("tole-past-jail");
        let outside = b.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        make(&dir, "s-in", ws.to_str().unwrap(), None, 2, "x");
        make(
            &dir,
            "s-sub",
            ws.join("sub").to_str().unwrap(),
            None,
            3,
            "x",
        );
        make(
            &dir,
            "s-out",
            outside.to_str().unwrap(),
            Some("leak"),
            4,
            "x",
        );
        make(&dir, "s-none", "", None, 5, "x");
        let (_, v) = q(&ws, &dir, false, "", LIMITS);
        assert_eq!(ids(&v), ["s-sub", "s-in"], "{v}");
        assert!(!v.to_string().contains("elsewhere"));
        // in the workspace's own directory a header without a cwd is listed,
        // with the workspace as its folder
        let (_, v) = q(&ws, &dir, true, "", LIMITS);
        let none = v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "s-none")
            .unwrap();
        assert_eq!(none["folder"], ws.to_str().unwrap());
    }

    #[test]
    fn unreadable_oversized_and_foreign_files_are_skipped_not_fatal() {
        let (_b, ws, dir) = setup("tole-past-skip");
        let cwd = ws.to_str().unwrap();
        make(&dir, "s-good", cwd, None, 10, "x");
        std::fs::write(dir.join("s-torn.jsonl"), b"{\"v\":1,\"kind\":\"hea").unwrap();
        std::fs::write(dir.join("s-bin.jsonl"), [0xff, 0xfe, 0, 1]).unwrap();
        std::fs::write(dir.join("not a session.jsonl"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(dir.join("s-dir.jsonl")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", dir.join("s-link.jsonl")).unwrap();
        let (c, v) = q(&ws, &dir, true, "", LIMITS);
        assert_eq!(c, 200);
        assert_eq!(ids(&v), ["s-good"], "{v}");
        assert!(v["skipped"].as_u64().unwrap() >= 2);

        // oversized: skipped, never replayed
        let tiny = Limits {
            max_file_bytes: 10,
            ..LIMITS
        };
        let (_, v) = q(&ws, &dir, true, "", tiny);
        assert!(ids(&v).is_empty());
        // a missing directory is an empty list
        let (c, v) = q(&ws, &ws.join("nope"), true, "", LIMITS);
        assert_eq!((c, ids(&v).len()), (200, 0));
    }

    #[test]
    fn the_scan_and_the_opens_are_bounded() {
        let (_b, ws, dir) = setup("tole-past-bounds");
        let cwd = ws.to_str().unwrap();
        for i in 0..12 {
            make(&dir, &format!("s-{i:02}"), cwd, None, 100 + i, "x");
        }
        // opens: 5 per request, but the cursor still reaches everything
        let l = Limits {
            max_opens: 5,
            ..LIMITS
        };
        let (_, v) = q(&ws, &dir, true, "limit=100", l);
        assert_eq!(ids(&v).len(), 5);
        let cur = v["nextCursor"].as_str().unwrap().to_string();
        let (_, v2) = q(&ws, &dir, true, &format!("limit=100&cursor={cur}"), l);
        assert_eq!(ids(&v2).len(), 5);
        // bytes: a budget of one file's size opens one file per request
        let one = std::fs::metadata(dir.join("s-00.jsonl")).unwrap().len();
        let l = Limits {
            max_bytes: one + 1,
            ..LIMITS
        };
        let (_, v) = q(&ws, &dir, true, "limit=100", l);
        assert_eq!(ids(&v).len(), 1);
        assert!(v["nextCursor"].is_string());
        // scan: only the first max_scan directory entries are considered
        let l = Limits {
            max_scan: 4,
            ..LIMITS
        };
        let (_, v) = q(&ws, &dir, true, "limit=100", l);
        assert!(ids(&v).len() <= 4);
        assert_eq!(v["scanTruncated"], true);
    }
}
