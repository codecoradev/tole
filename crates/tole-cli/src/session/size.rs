//! Read-only size report of a sessions directory (#1020, step 2 of
//! `docs/session-retention-design.md`): how many session files, how many bytes,
//! the largest and the oldest. It reads directory metadata only: no file is
//! opened, replayed, moved or changed, so it is cheap on a long-lived volume.

use std::path::Path;

/// What a sessions directory holds, from `<valid id>.jsonl` regular files.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SizeReport {
    pub files: usize,
    pub bytes: u64,
    /// The largest file: id and bytes (ties: the smaller id).
    pub largest: Option<(String, u64)>,
    /// The file written longest ago: id and mtime in epoch seconds.
    pub oldest: Option<(String, u64)>,
}

/// Scan `dir`; `None` when it cannot be read (it does not exist, no access).
pub fn scan(dir: &Path) -> Option<SizeReport> {
    let mut r = SizeReport::default();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_suffix(".jsonl")
            .filter(|i| tole_core::storage::is_valid_session_id(i))
        else {
            continue;
        };
        let Some(m) = e.metadata().ok().filter(|m| m.is_file()) else {
            continue;
        };
        let at = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        r.files += 1;
        r.bytes += m.len();
        if r.largest.as_ref().is_none_or(|(i, b)| {
            (m.len(), std::cmp::Reverse(id)) > (*b, std::cmp::Reverse(i.as_str()))
        }) {
            r.largest = Some((id.to_string(), m.len()));
        }
        let at = at.map_or(0, |d| d.as_secs());
        if r.oldest
            .as_ref()
            .is_none_or(|(i, t)| (at, id) < (*t, i.as_str()))
        {
            r.oldest = Some((id.to_string(), at));
        }
    }
    Some(r)
}

/// Bytes as `B`, `KiB`, `MiB` or `GiB` with one decimal.
pub fn human(bytes: u64) -> String {
    let mut v = bytes as f64;
    for unit in ["B", "KiB", "MiB"] {
        if v < 1024.0 {
            return if unit == "B" {
                format!("{bytes} B")
            } else {
                format!("{v:.1} {unit}")
            };
        }
        v /= 1024.0;
    }
    format!("{v:.1} GiB")
}

impl SizeReport {
    /// The one-line report, or `None` for a directory with no session file.
    pub fn line(&self) -> Option<String> {
        let ((big, big_bytes), (old, at)) = (self.largest.as_ref()?, self.oldest.as_ref()?);
        let day = super::assemble::civil_from_days((*at / 86400) as i64);
        Some(format!(
            "store: {} session file(s), {} total; largest {big} ({}); oldest {old} (last written {day})",
            self.files,
            human(self.bytes),
            human(*big_bytes)
        ))
    }
}

/// The report line of `dir` (what `tole sessions` and `tole doctor` print).
pub fn line(dir: &Path) -> Option<String> {
    scan(dir)?.line()
}
