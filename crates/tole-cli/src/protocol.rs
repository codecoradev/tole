//! Wire types for remote UI clients (#524, part of the UI epic #523).
//!
//! One place defines the JSON a graphical client reads from `tole serve`,
//! so the desktop, Flutter and web clients (and their code generators)
//! have a single source of truth. Nothing here is a second storage
//! format: an entry on the wire is exactly the [`Entry`] the append-only
//! JSONL log holds, so a client replays what `tole resume` replays.
//!
//! This first piece is the paginated transcript read. Resume-by-id and the
//! SSE event stream follow in their own changes.

use serde::Serialize;
use tole_core::entry::Entry;
use tole_core::storage::{JsonlStorage, Storage};

/// Entries returned when the client gives no `limit`.
pub const ENTRIES_DEFAULT_LIMIT: usize = 100;
/// Hard ceiling on `limit`.
pub const ENTRIES_MAX_LIMIT: usize = 500;
/// Soft byte budget for one page's serialized entries. Tool results can be
/// large, so a page stops adding entries once it passes this (it always
/// carries at least one, so a single huge entry cannot wedge pagination).
pub const ENTRIES_BYTE_BUDGET: usize = 1024 * 1024;

/// `GET /sessions/{id}/entries?after=<seq>&limit=<n>`.
///
/// Page through with `after = nextAfter` while `hasMore` is true.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EntriesPage {
    /// The session id.
    pub id: String,
    /// Entries with `seq > after`, in log order.
    pub entries: Vec<Entry>,
    /// Pass this as `after` to read the next page: the `seq` of the last
    /// entry returned, or the request's `after` when none were.
    pub next_after: u64,
    /// More entries exist past `nextAfter`.
    pub has_more: bool,
    /// `seq` of the newest entry in the log when it was read (0 if empty).
    pub last_seq: u64,
}

/// Why an entries request was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum EntriesError {
    /// `after` / `limit` is not a non-negative integer, or `limit` is 0.
    BadQuery(String),
    /// The session file could not be read.
    Read(String),
}

/// Parse the `after` and `limit` query parameters (`None` = absent).
pub fn parse_entries_query(
    after: Option<&str>,
    limit: Option<&str>,
) -> Result<(u64, usize), EntriesError> {
    let after = match after {
        None => 0,
        Some(a) => a
            .parse::<u64>()
            .map_err(|_| EntriesError::BadQuery("after must be a non-negative integer".into()))?,
    };
    let limit = match limit {
        None => ENTRIES_DEFAULT_LIMIT,
        Some(l) => {
            let n = l
                .parse::<usize>()
                .map_err(|_| EntriesError::BadQuery("limit must be a positive integer".into()))?;
            if n == 0 {
                return Err(EntriesError::BadQuery("limit must be at least 1".into()));
            }
            n.min(ENTRIES_MAX_LIMIT)
        }
    };
    Ok((after, limit))
}

/// Read one page of a session's transcript from its JSONL file.
///
/// Reads the FILE (replayed read-only, a torn tail is skipped and never
/// truncated), not the live storage handle: a running turn holds that
/// handle's lock for its whole duration, and a transcript read must work
/// while a turn is in flight. Cost is a replay of the file per call.
pub fn entries_page(
    path: &std::path::Path,
    id: &str,
    after: u64,
    limit: usize,
) -> Result<EntriesPage, EntriesError> {
    let storage =
        JsonlStorage::open_read_only(path).map_err(|e| EntriesError::Read(e.to_string()))?;
    Ok(page_of(id, storage.entries(), after, limit))
}

/// Pure pagination over an in-order entry slice (strictly increasing seq).
fn page_of(id: &str, all: &[Entry], after: u64, limit: usize) -> EntriesPage {
    let start = all.partition_point(|e| e.seq <= after);
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    for e in &all[start..] {
        if entries.len() >= limit {
            break;
        }
        let size = serde_json::to_string(e).map(|s| s.len()).unwrap_or(0);
        if !entries.is_empty() && bytes + size > ENTRIES_BYTE_BUDGET {
            break;
        }
        bytes += size;
        entries.push(e.clone());
    }
    let next_after = entries.last().map(|e| e.seq).unwrap_or(after);
    let has_more = start + entries.len() < all.len();
    EntriesPage {
        id: id.to_string(),
        entries,
        next_after,
        has_more,
        last_seq: all.last().map(|e| e.seq).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, text: &str) -> Entry {
        Entry {
            seq,
            id: format!("e_{seq}"),
            parent_id: None,
            kind: tole_core::entry::EntryType::new(tole_core::entry::EntryType::MESSAGE),
            timestamp: 0,
            payload: json!({"role": "user", "text": text}),
        }
    }

    fn log(n: u64) -> Vec<Entry> {
        (1..=n).map(|s| entry(s, "x")).collect()
    }

    #[test]
    fn query_defaults_clamps_and_refuses_junk() {
        assert_eq!(
            parse_entries_query(None, None),
            Ok((0, ENTRIES_DEFAULT_LIMIT))
        );
        assert_eq!(parse_entries_query(Some("7"), Some("3")), Ok((7, 3)));
        assert_eq!(
            parse_entries_query(None, Some("999999")),
            Ok((0, ENTRIES_MAX_LIMIT))
        );
        for (a, l) in [
            (Some("-1"), None),
            (Some("x"), None),
            (None, Some("0")),
            (None, Some("-5")),
            (None, Some("many")),
        ] {
            assert!(
                matches!(parse_entries_query(a, l), Err(EntriesError::BadQuery(_))),
                "{a:?} {l:?}"
            );
        }
    }

    #[test]
    fn pages_cover_the_log_exactly_once_in_order() {
        let all = log(10);
        let mut after = 0;
        let mut seen = Vec::new();
        loop {
            let p = page_of("s", &all, after, 4);
            seen.extend(p.entries.iter().map(|e| e.seq));
            assert_eq!(p.last_seq, 10);
            if !p.has_more {
                break;
            }
            assert!(p.next_after > after, "must make progress");
            after = p.next_after;
        }
        assert_eq!(seen, (1..=10).collect::<Vec<_>>());
    }

    #[test]
    fn edges_empty_log_past_the_end_and_exact_fit() {
        let empty = page_of("s", &[], 0, 5);
        assert!(empty.entries.is_empty() && !empty.has_more);
        assert_eq!((empty.next_after, empty.last_seq), (0, 0));
        let all = log(3);
        let past = page_of("s", &all, 99, 5);
        assert!(past.entries.is_empty() && !past.has_more);
        assert_eq!(past.next_after, 99, "no progress: after is echoed back");
        let exact = page_of("s", &all, 0, 3);
        assert_eq!(exact.entries.len(), 3);
        assert!(!exact.has_more, "limit == remaining is not 'more'");
    }

    #[test]
    fn seq_gaps_are_respected() {
        // seq is monotonic, not dense (registers and usage share the counter).
        let all = vec![entry(2, "a"), entry(5, "b"), entry(9, "c")];
        let p = page_of("s", &all, 2, 10);
        assert_eq!(p.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), [5, 9]);
        assert_eq!(page_of("s", &all, 4, 1).next_after, 5);
    }

    #[test]
    fn a_page_stops_at_the_byte_budget_but_always_makes_progress() {
        let big = "y".repeat(ENTRIES_BYTE_BUDGET / 2 + 10);
        let all = vec![entry(1, &big), entry(2, &big), entry(3, &big)];
        let p = page_of("s", &all, 0, 500);
        assert_eq!(p.entries.len(), 1, "second entry would pass the budget");
        assert!(p.has_more);
        // One entry larger than the whole budget is still delivered.
        let huge = vec![entry(1, &"z".repeat(ENTRIES_BYTE_BUDGET * 2))];
        let p = page_of("s", &huge, 0, 500);
        assert_eq!((p.entries.len(), p.has_more), (1, false));
    }

    #[test]
    fn the_wire_shape_is_camel_case_and_entries_are_the_log_entries() {
        let p = page_of("s-1", &log(2), 0, 5);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["id"], "s-1");
        assert_eq!(v["nextAfter"], 2);
        assert_eq!(v["hasMore"], false);
        assert_eq!(v["lastSeq"], 2);
        assert_eq!(v["entries"][0]["type"], "message");
        assert_eq!(v["entries"][0]["seq"], 1);
    }

    #[test]
    fn a_file_read_matches_replay_and_skips_a_torn_tail() {
        use tole_core::entry::NewEntry;
        use tole_core::storage::{Commit, JsonlStorage};
        let dir = std::env::temp_dir().join(format!(
            "tole-protocol-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = JsonlStorage::create(&dir, "p1", None).unwrap();
        for t in ["one", "two", "three"] {
            let payload = json!({"role": "user", "text": t});
            let kind = tole_core::entry::EntryType::new(tole_core::entry::EntryType::MESSAGE);
            let new = match s.entries().last() {
                None => NewEntry::root(kind, payload),
                Some(prev) => NewEntry::with_parent(prev.id.clone(), kind, payload),
            };
            s.commit(Commit::new().entry(new)).unwrap();
        }
        let path = dir.join("p1.jsonl");
        // A writer mid-append: a newline-less fragment at the end.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"[{\"kind\":\"entry\",\"seq\":99,\"id\":\"e_9")
                .unwrap();
        }
        let page = entries_page(&path, "p1", 0, 50).unwrap();
        let replayed: Vec<_> = s.entries().to_vec();
        assert_eq!(page.entries, replayed, "same entries as replay");
        assert!(!page.has_more);
        let tail = entries_page(&path, "p1", replayed[1].seq, 50).unwrap();
        assert_eq!(tail.entries, replayed[2..].to_vec());
        assert!(matches!(
            entries_page(&dir.join("nope.jsonl"), "n", 0, 1),
            Err(EntriesError::Read(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
