//! Owner-approved skill content (#591, design `skills-registry-design.md`
//! §4.2): a skill is followed, so the trust anchor is the owner's approval
//! of its exact bytes, not the fact that a file exists or that an index
//! returned its name.
//!
//! - A skill **package** is every file of its directory (SKILL.md plus
//!   `references/`, `scripts/`, `assets/`, ...). Its hash covers each
//!   file's relative path and bytes, so any change to any file needs a new
//!   approval. A symlink in a package makes it unreadable (its target could
//!   change without the hash changing).
//! - **Approvals** live in the user's data dir
//!   (`$CODECORA_HOME/tole/skill-approvals.json`), outside every workspace,
//!   one record per skill name: the approved hash, when, and how many files.
//!   `tole skills approve <name>` writes it; nothing else does.
//! - A [`Gate`] decides what a session may load. [`Gate::Open`] is today's
//!   behavior. [`Gate::Approved`] loads a body only when the package hash on
//!   disk matches the approval, and parses it from the same bytes it hashed;
//!   any other skill is reported as unavailable, with no description and no
//!   body.

use crate::skills::{
    parse_loaded, read_capped, read_file_capped, skill_file, ReadCapError, Skill,
    MAX_SKILL_FILE_BYTES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The approvals file name in the tole data dir.
pub const APPROVALS_FILE: &str = "skill-approvals.json";
/// Most files a skill package may hold.
pub const MAX_PACKAGE_FILES: usize = 256;
/// Most bytes a skill package may hold, all files together.
pub const MAX_PACKAGE_BYTES: u64 = 4 * 1024 * 1024;
/// Most directory entries (files and directories, at any depth) a skill
/// package walk visits.
pub const MAX_PACKAGE_ENTRIES: usize = 1024;
/// Largest approvals file read.
pub const MAX_APPROVALS_BYTES: u64 = 1024 * 1024;

/// The size limits of a package walk ([`MAX_PACKAGE_FILES`],
/// [`MAX_PACKAGE_BYTES`] outside tests).
#[derive(Debug, Clone, Copy)]
struct Limits {
    files: usize,
    bytes: u64,
    /// Directory entries (files and directories) walked.
    entries: usize,
}

const LIMITS: Limits = Limits {
    files: MAX_PACKAGE_FILES,
    bytes: MAX_PACKAGE_BYTES,
    entries: MAX_PACKAGE_ENTRIES,
};

/// The owner's approval of one skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The approved package hash (`sha256:<hex>`).
    pub hash: String,
    /// Seconds since the Unix epoch.
    pub approved_at: u64,
    /// Files in the approved package.
    pub files: usize,
}

/// Every approval of this user, by skill name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approvals {
    #[serde(default)]
    pub skills: BTreeMap<String, Approval>,
}

impl Approvals {
    /// `$CODECORA_HOME/tole/skill-approvals.json`; `None` when the CodeCora
    /// home is unresolvable (then nothing is approved).
    pub fn default_path() -> Option<PathBuf> {
        crate::paths::tole_data_dir().map(|d| d.join(APPROVALS_FILE))
    }

    /// Read `path`. A missing file is no approvals; an unreadable or
    /// malformed one is an error (fail closed, loudly).
    pub fn load(path: &Path) -> Result<Self, String> {
        match read_file_capped(path, MAX_APPROVALS_BYTES) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|e| format!("{}: malformed skill approvals: {e}", path.display())),
            Err(ReadCapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Write `path` atomically (temp file + rename), creating its directory.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".{APPROVALS_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }

    /// Approve `pkg` as it is on disk now, replacing an earlier approval of
    /// the same name (only the latest approved bytes load).
    pub fn approve(&mut self, pkg: &SkillPackage, now: u64) {
        self.skills.insert(
            pkg.name.clone(),
            Approval {
                hash: pkg.hash.clone(),
                approved_at: now,
                files: pkg.files.len(),
            },
        );
    }

    /// Drop the approval of `name`; false when there was none.
    pub fn revoke(&mut self, name: &str) -> bool {
        self.skills.remove(name).is_some()
    }

    /// True when `hash` is the approved hash of `name`.
    pub fn is_approved(&self, name: &str, hash: &str) -> bool {
        self.skills.get(name).is_some_and(|a| a.hash == hash)
    }
}

/// One skill's directory, read in full: what an approval covers.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillPackage {
    pub name: String,
    pub dir: PathBuf,
    /// `(relative path with '/', bytes)`, sorted by path.
    pub files: Vec<(String, Vec<u8>)>,
    /// [`package_hash`] of `files`.
    pub hash: String,
}

impl SkillPackage {
    /// The bytes of SKILL.md (always present in a package).
    pub fn skill_md(&self) -> &[u8] {
        self.files
            .iter()
            .find(|(p, _)| p == "SKILL.md")
            .map(|(_, b)| b.as_slice())
            .unwrap_or_default()
    }
}

/// The package of skill `name`, from the directory `load_skill` would read
/// (project first, then user-global).
pub fn read_package(workspace: Option<&Path>, name: &str) -> Result<SkillPackage, String> {
    let dir = skill_file(workspace, name)
        .and_then(|f| f.parent().map(Path::to_path_buf))
        .ok_or_else(|| format!("skill {name:?} not found"))?;
    let files = read_dir_files(&dir, LIMITS)?;
    let hash = package_hash(&files);
    Ok(SkillPackage {
        name: name.to_string(),
        dir,
        files,
        hash,
    })
}

/// Every file under `dir` (relative path, bytes), sorted by path, within
/// `limits`. A file is never read past what is left of the byte budget:
/// its length is checked first (lstat), and the read itself is capped.
fn read_dir_files(dir: &Path, limits: Limits) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut seen = 0usize;
    walk(dir, "", &mut files, &mut total, &mut seen, limits)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn walk(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<(String, Vec<u8>)>,
    total: &mut u64,
    seen: &mut usize,
    limits: Limits,
) -> Result<(), String> {
    let over = || {
        format!(
            "{}: skill package over {} files or {} bytes",
            dir.display(),
            limits.files,
            limits.bytes
        )
    };
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        // Every entry counts, directories too: a package of countless
        // (empty) directories is refused before it costs a long walk.
        *seen += 1;
        if *seen > limits.entries {
            return Err(format!(
                "{}: skill package over {} entries",
                dir.display(),
                limits.entries
            ));
        }
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| format!("{}: non-UTF-8 file name", path.display()))?;
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if kind.is_symlink() {
            return Err(format!("{}: symlink in a skill package", path.display()));
        } else if kind.is_dir() {
            walk(&path, &rel, files, total, seen, limits)?;
        } else if kind.is_file() {
            // Refuse before reading: the file count, then the length the
            // directory entry reports (lstat: a symlink is not followed).
            let remaining = limits.bytes.saturating_sub(*total);
            let len = entry
                .metadata()
                .map_err(|e| format!("{}: {e}", path.display()))?
                .len();
            if files.len() >= limits.files || len > remaining {
                return Err(over());
            }
            let file = open_no_follow(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let bytes = read_capped(file, remaining)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .ok_or_else(over)?;
            *total += bytes.len() as u64;
            files.push((rel, bytes));
        } else {
            return Err(format!("{}: not a regular file", path.display()));
        }
    }
    Ok(())
}

/// Open `path` for reading without following a symlink swapped in after
/// the directory walk saw a regular file.
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// `sha256:<hex>` over each file's relative path, length and bytes, in
/// the given (sorted) order. The length prefix keeps two packages from
/// sharing a byte stream.
pub fn package_hash(files: &[(String, Vec<u8>)]) -> String {
    let mut h = Sha256::new();
    for (path, bytes) in files {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Why a skill could not be loaded through a [`Gate`].
#[derive(Debug, Clone, PartialEq)]
pub enum LoadError {
    /// The skill exists but its bytes are not the approved ones.
    NotApproved { name: String, hash: String },
    /// Missing, unreadable or malformed.
    Other(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApproved { name, hash } => write!(
                f,
                "skill {name:?} is not approved by the owner (content {hash}), so it cannot be \
                 loaded; the owner can review it and run `tole skills approve {name}`"
            ),
            Self::Other(e) => f.write_str(e),
        }
    }
}

/// What a session may load.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Gate {
    /// Every skill on disk (the pre-#591 behavior).
    #[default]
    Open,
    /// Only skills whose package hash matches the owner's approval.
    Approved(Arc<Approvals>),
}

impl Gate {
    /// Load skill `name`. Under [`Gate::Approved`] the body is parsed from
    /// the exact bytes that were hashed, so a file swapped after the check
    /// is never what loads.
    pub fn load(&self, workspace: Option<&Path>, name: &str) -> Result<Skill, LoadError> {
        match self {
            Self::Open => crate::skills::load_skill(workspace, name).map_err(LoadError::Other),
            Self::Approved(approvals) => {
                let pkg = read_package(workspace, name).map_err(LoadError::Other)?;
                if !approvals.is_approved(name, &pkg.hash) {
                    return Err(LoadError::NotApproved {
                        name: name.to_string(),
                        hash: pkg.hash,
                    });
                }
                if pkg.skill_md().len() as u64 > MAX_SKILL_FILE_BYTES {
                    return Err(LoadError::Other(format!(
                        "skill {name:?}: SKILL.md is over {MAX_SKILL_FILE_BYTES} bytes"
                    )));
                }
                let text = std::str::from_utf8(pkg.skill_md()).map_err(|_| {
                    LoadError::Other(format!("skill {name:?}: SKILL.md is not UTF-8"))
                })?;
                parse_loaded(text, name).map_err(LoadError::Other)
            }
        }
    }
}

/// `YYYY-MM-DD HH:MM UTC` for an approval time.
pub fn format_time(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (y, m, d) = crate::cron::civil_from_days(days);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tole-skapprove-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(ws: &Path, rel: &str, body: &str) {
        let p = ws.join("skills").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn skill(ws: &Path, name: &str, body: &str) {
        write(
            ws,
            &format!("{name}/SKILL.md"),
            &format!("---\nname: {name}\ndescription: \"{name} d\"\n---\n{body}"),
        );
    }

    #[test]
    fn package_hash_covers_every_file_path_and_byte() {
        let ws = tmp("hash");
        skill(&ws, "demo", "b");
        write(&ws, "demo/references/a.md", "ref");
        let p1 = read_package(Some(&ws), "demo").unwrap();
        assert_eq!(
            p1.files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["SKILL.md", "references/a.md"]
        );
        assert!(
            p1.hash.starts_with("sha256:") && p1.hash.len() == 71,
            "{}",
            p1.hash
        );
        // A reference changes: new hash.
        write(&ws, "demo/references/a.md", "ref2");
        let p2 = read_package(Some(&ws), "demo").unwrap();
        assert_ne!(p1.hash, p2.hash);
        // A file is renamed with the same bytes: new hash.
        std::fs::remove_file(ws.join("skills/demo/references/a.md")).unwrap();
        write(&ws, "demo/references/b.md", "ref2");
        let p3 = read_package(Some(&ws), "demo").unwrap();
        assert_ne!(p2.hash, p3.hash);
        // A file is added: new hash.
        write(&ws, "demo/scripts/x.sh", "");
        assert_ne!(read_package(Some(&ws), "demo").unwrap().hash, p3.hash);
        // Same bytes: same hash.
        assert_eq!(
            read_package(Some(&ws), "demo").unwrap().hash,
            read_package(Some(&ws), "demo").unwrap().hash
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_a_package_makes_it_unreadable() {
        let ws = tmp("link");
        skill(&ws, "demo", "b");
        std::os::unix::fs::symlink("/etc/hostname", ws.join("skills/demo/ref.md")).unwrap();
        let err = read_package(Some(&ws), "demo").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_approved_gate_loads_only_the_approved_bytes() {
        let ws = tmp("gate");
        skill(&ws, "demo", "approved body");
        skill(&ws, "other", "never approved");
        let mut a = Approvals::default();
        a.approve(&read_package(Some(&ws), "demo").unwrap(), 1);
        let gate = Gate::Approved(Arc::new(a));
        assert_eq!(gate.load(Some(&ws), "demo").unwrap().body, "approved body");
        // Never approved: not loaded, and the error names the hash.
        let other_hash = read_package(Some(&ws), "other").unwrap().hash;
        assert_eq!(
            gate.load(Some(&ws), "other").unwrap_err(),
            LoadError::NotApproved {
                name: "other".into(),
                hash: other_hash
            }
        );
        // Edited after approval (body or a reference): not loaded.
        skill(&ws, "demo", "tampered body");
        assert!(matches!(
            gate.load(Some(&ws), "demo"),
            Err(LoadError::NotApproved { .. })
        ));
        skill(&ws, "demo", "approved body");
        assert!(gate.load(Some(&ws), "demo").is_ok());
        write(&ws, "demo/references/new.md", "x");
        assert!(matches!(
            gate.load(Some(&ws), "demo"),
            Err(LoadError::NotApproved { .. })
        ));
        // An approval of one name never covers another with the same bytes.
        let mut b = Approvals::default();
        b.approve(&read_package(Some(&ws), "other").unwrap(), 1);
        b.skills.insert("demo".into(), b.skills["other"].clone());
        assert!(Gate::Approved(Arc::new(b)).load(Some(&ws), "demo").is_err());
        // Missing skill: an ordinary error. Open gate: today's behavior.
        assert!(matches!(
            gate.load(Some(&ws), "absent"),
            Err(LoadError::Other(_))
        ));
        assert_eq!(
            Gate::Open.load(Some(&ws), "other").unwrap().body,
            "never approved"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A reader that panics when asked for more than `cap` bytes in total:
    /// proof that an over-budget file is never read in full.
    struct Bounded {
        cap: usize,
        served: usize,
    }

    impl std::io::Read for Bounded {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                self.served + buf.len().min(1) <= self.cap,
                "read past the cap ({} bytes)",
                self.cap
            );
            let n = buf.len().min(self.cap - self.served).min(4096);
            buf[..n].fill(0);
            self.served += n;
            Ok(n)
        }
    }

    /// Cora finding on #617: a huge file must be refused without reading
    /// it into memory. The read stops one byte past the remaining budget.
    #[test]
    fn an_over_budget_file_is_refused_without_a_full_read() {
        use std::io::Read as _;
        // An endless source: only the cap keeps the read bounded.
        let endless = Bounded { cap: 11, served: 0 };
        assert_eq!(read_capped(endless, 10).unwrap(), None);
        let exact = Bounded { cap: 11, served: 0 };
        assert_eq!(read_capped(exact.take(10), 10).unwrap(), Some(vec![0; 10]));
        // The walk refuses on the reported length before opening the file,
        // and on the file count, with the over-limit error.
        let ws = tmp("limits");
        skill(&ws, "demo", "b");
        let dir = ws.join("skills/demo");
        let tiny = Limits {
            files: 8,
            bytes: 64,
            entries: 64,
        };
        assert!(read_dir_files(&dir, tiny).is_ok());
        let big = std::fs::File::create(dir.join("big.bin")).unwrap();
        big.set_len(1 << 40).unwrap(); // sparse: 1 TiB reported, nothing stored
        let err = read_dir_files(&dir, tiny).unwrap_err();
        assert!(
            err.ends_with("skill package over 8 files or 64 bytes"),
            "{err}"
        );
        assert!(read_package(Some(&ws), "demo")
            .unwrap_err()
            .ends_with(&format!(
                "skill package over {MAX_PACKAGE_FILES} files or {MAX_PACKAGE_BYTES} bytes"
            )));
        std::fs::remove_file(dir.join("big.bin")).unwrap();
        write(&ws, "demo/a.md", "a");
        let one_file = Limits {
            files: 1,
            bytes: 64,
            entries: 64,
        };
        assert!(read_dir_files(&dir, one_file)
            .unwrap_err()
            .ends_with("over 1 files or 64 bytes"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Review note on #617: directories count against the limits, so a
    /// package of countless empty directories is refused quickly.
    #[test]
    fn empty_directories_count_against_the_entry_limit() {
        let ws = tmp("entries");
        skill(&ws, "demo", "b");
        let dir = ws.join("skills/demo");
        for i in 0..6 {
            std::fs::create_dir_all(dir.join(format!("d{i}/e"))).unwrap();
        }
        let few = Limits {
            files: 8,
            bytes: 1024,
            entries: 8,
        };
        let err = read_dir_files(&dir, few).unwrap_err();
        assert!(err.contains("over 8 entries"), "{err}");
        let enough = Limits {
            files: 8,
            bytes: 1024,
            entries: 13,
        };
        assert!(
            read_dir_files(&dir, enough).is_ok(),
            "1 file + 12 directories"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn approvals_round_trip_replace_revoke_and_fail_closed() {
        let ws = tmp("store");
        skill(&ws, "demo", "v1");
        let path = ws.join("data").join(APPROVALS_FILE);
        assert_eq!(Approvals::load(&path).unwrap(), Approvals::default());
        let mut a = Approvals::default();
        let v1 = read_package(Some(&ws), "demo").unwrap();
        a.approve(&v1, 1_700_000_000);
        a.save(&path).unwrap();
        let back = Approvals::load(&path).unwrap();
        assert_eq!(back, a);
        assert_eq!(back.skills["demo"].files, 1);
        // Approving new bytes replaces the old approval.
        skill(&ws, "demo", "v2");
        let v2 = read_package(Some(&ws), "demo").unwrap();
        a.approve(&v2, 1_700_000_100);
        assert!(a.is_approved("demo", &v2.hash));
        assert!(!a.is_approved("demo", &v1.hash));
        assert!(a.revoke("demo"));
        assert!(!a.revoke("demo"));
        assert!(!a.is_approved("demo", &v2.hash));
        // A malformed file is an error, never "no approvals" or "all".
        std::fs::write(&path, "{not json").unwrap();
        assert!(Approvals::load(&path).unwrap_err().contains("malformed"));
        // An oversized one is refused without reading it in full.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_APPROVALS_BYTES + 1)
            .unwrap();
        let err = Approvals::load(&path).unwrap_err();
        assert!(err.contains("over 1048576 bytes"), "{err}");
        assert_eq!(format_time(1_700_000_000), "2023-11-14 22:13 UTC");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
