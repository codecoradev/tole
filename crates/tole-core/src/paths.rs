//! The one place that decides where the CodeCora home lives (#344).
//!
//! Skills, the update-check cache and the project-config trust store all
//! live under `<root>/tole/`. The root is attacker-relevant: a root that
//! resolves against the current directory would land inside a possibly
//! cloned, untrusted project. So the rules are strict and shared:
//!
//! * root = `$CODECORA_HOME`, else `$HOME` (or `$USERPROFILE`) joined with
//!   `.codecora`;
//! * an EMPTY variable counts as unset;
//! * a RELATIVE root is refused (unresolvable), never joined to the cwd;
//! * there is NO cwd fallback — unresolvable is `None` and callers skip the
//!   feature (or error) instead of guessing.
//!
//! Only reads the environment; no new dependency, no I/O.

use std::path::PathBuf;

/// The CodeCora root directory (`$CODECORA_HOME`, else
/// `$HOME|$USERPROFILE/.codecora`), or `None` when it cannot be resolved
/// to an absolute path. Never falls back to the current directory.
pub fn codecora_root() -> Option<PathBuf> {
    resolve_root(
        std::env::var("CODECORA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("USERPROFILE").ok().as_deref(),
    )
}

/// The tole data directory: `<root>/tole`. `None` when the root is
/// unresolvable (see [`codecora_root`]).
pub fn tole_data_dir() -> Option<PathBuf> {
    codecora_root().map(|r| r.join("tole"))
}

/// `p` made absolute and symlink-free as far as it exists: the longest
/// existing ancestor is canonicalized and the rest appended.
fn canon_lenient(p: &std::path::Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = cur.canonicalize() {
            return tail.iter().rev().fold(c, |acc, n| acc.join(n));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// True when a file-tool jail at `workspace` and `data_dir` overlap: the
/// workspace holds the data dir (tole started in `~`), or is inside it.
/// File tools in such a jail could write owner-only records (#684).
pub fn jail_overlaps(workspace: &std::path::Path, data_dir: &std::path::Path) -> bool {
    let (w, d) = (canon_lenient(workspace), canon_lenient(data_dir));
    d.starts_with(&w) || w.starts_with(&d)
}

fn resolve_root(
    codecora_home: Option<&str>,
    home: Option<&str>,
    userprofile: Option<&str>,
) -> Option<PathBuf> {
    fn set(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.is_empty())
    }
    let root = match set(codecora_home) {
        Some(c) => PathBuf::from(c),
        None => PathBuf::from(set(home).or_else(|| set(userprofile))?).join(".codecora"),
    };
    root.is_absolute().then_some(root)
}

/// Serializes tests that touch the process environment (CODECORA_HOME /
/// HOME / USERPROFILE) and restores the previous values afterwards.
#[cfg(test)]
pub(crate) fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved: Vec<(String, Option<std::ffi::OsString>)> = vars
        .iter()
        .map(|(k, _)| (k.to_string(), std::env::var_os(k)))
        .collect();
    for (k, v) in vars {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    // Restore even if `f` panics, so a failed assertion cannot leak env.
    struct Restore(Vec<(String, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
    let _r = Restore(saved);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    #[cfg(unix)]
    #[test]
    fn resolver_matrix() {
        // CODECORA_HOME set absolute wins over HOME / USERPROFILE.
        assert_eq!(
            resolve_root(Some("/cc"), Some("/home/u"), Some("/up")),
            p("/cc")
        );
        // Empty CODECORA_HOME counts as unset -> HOME.
        assert_eq!(
            resolve_root(Some(""), Some("/home/u"), None),
            p("/home/u/.codecora")
        );
        // Relative CODECORA_HOME is refused (no fall-through to HOME).
        assert_eq!(resolve_root(Some("rel/dir"), Some("/home/u"), None), None);
        // HOME only / USERPROFILE only.
        assert_eq!(
            resolve_root(None, Some("/home/u"), None),
            p("/home/u/.codecora")
        );
        assert_eq!(resolve_root(None, None, Some("/up")), p("/up/.codecora"));
        // HOME wins over USERPROFILE; empty HOME falls to USERPROFILE.
        assert_eq!(
            resolve_root(None, Some("/home/u"), Some("/up")),
            p("/home/u/.codecora")
        );
        assert_eq!(
            resolve_root(None, Some(""), Some("/up")),
            p("/up/.codecora")
        );
        // Relative HOME is refused.
        assert_eq!(resolve_root(None, Some("rel"), None), None);
        // Nothing set / all empty: unresolvable, never ".".
        assert_eq!(resolve_root(None, None, None), None);
        assert_eq!(resolve_root(Some(""), Some(""), Some("")), None);
    }

    #[cfg(unix)]
    #[test]
    fn env_wrappers_read_the_environment() {
        with_env(
            &[
                ("CODECORA_HOME", Some("/cc-env")),
                ("HOME", Some("/home/x")),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(codecora_root(), p("/cc-env"));
                assert_eq!(tole_data_dir(), p("/cc-env/tole"));
            },
        );
        with_env(
            &[
                ("CODECORA_HOME", None),
                ("HOME", None),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(codecora_root(), None);
                assert_eq!(tole_data_dir(), None);
            },
        );
    }

    /// #684: a file-tool jail that holds (or sits inside) the data dir is
    /// refused; a sibling directory is not.
    #[test]
    fn jail_overlap_with_the_data_dir() {
        let base = std::env::temp_dir().join(format!("tole-paths-jail-{}", std::process::id()));
        let data = base.join(".codecora").join("tole");
        std::fs::create_dir_all(&data).unwrap();
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        assert!(jail_overlaps(&base, &data), "home holds the data dir");
        assert!(jail_overlaps(&base.join(".codecora"), &data));
        assert!(jail_overlaps(&data, &data));
        assert!(
            jail_overlaps(&data.join("cron"), &data),
            "inside the data dir"
        );
        assert!(!jail_overlaps(&project, &data));
        // A data dir that does not exist yet still counts.
        let fresh = base.join("fresh-home").join(".codecora").join("tole");
        assert!(jail_overlaps(&base.join("fresh-home"), &fresh));
        // A symlink into the data dir is followed.
        #[cfg(unix)]
        {
            let link = base.join("project-link");
            std::os::unix::fs::symlink(&data, &link).unwrap();
            assert!(jail_overlaps(&link, &data));
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
