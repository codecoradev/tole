use super::*;
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

fn base(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-safedir-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    // The OS temp dir may itself be a link (macOS /var): work from its target.
    d.canonicalize().unwrap()
}

fn create_new() -> Mode {
    Mode {
        write: true,
        create_new: true,
        ..Mode::default()
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_on_the_path_is_an_error_not_a_detour() {
    let b = base("walk");
    std::fs::create_dir_all(b.join("real/sessions")).unwrap();
    std::os::unix::fs::symlink(b.join("real"), b.join("link")).unwrap();
    assert!(SafeDir::walk(&b.join("real/sessions")).is_ok());
    // A link as the last component and in the middle are both refused.
    assert!(SafeDir::walk(&b.join("link")).is_err());
    assert!(SafeDir::walk(&b.join("link/sessions")).is_err());
    // Not canonical: refused rather than interpreted.
    assert!(SafeDir::walk(&b.join("real/../real")).is_err());
    assert!(SafeDir::walk(Path::new("relative/dir")).is_err());
}

#[cfg(unix)]
#[test]
fn swapping_the_directory_for_a_symlink_after_open_redirects_nothing() {
    let b = base("swap");
    let sessions = b.join(".tole").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let elsewhere = b.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let dir = SafeDir::open(&sessions).unwrap();
    // The job swaps `.tole` for a link to another directory.
    std::fs::rename(b.join(".tole"), b.join("moved")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, b.join(".tole")).unwrap();

    let mut f = dir
        .open_file(OsStr::new("s.jsonl.lock"), create_new())
        .unwrap();
    f.write_all(b"1").unwrap();
    assert!(
        b.join("moved/sessions/s.jsonl.lock").is_file(),
        "created where the handle points"
    );
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing was created through the link"
    );
    // A rename goes through the same handle.
    dir.rename(OsStr::new("s.jsonl.lock"), OsStr::new("t.jsonl.lock"))
        .unwrap();
    assert!(b.join("moved/sessions/t.jsonl.lock").is_file());
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    dir.sync().unwrap();
}

#[cfg(unix)]
#[test]
fn a_file_that_is_a_link_a_fifo_or_a_directory_is_refused() {
    let b = base("kinds");
    let dir = SafeDir::open(&b).unwrap();
    std::fs::write(b.join("real"), "x").unwrap();
    std::os::unix::fs::symlink(b.join("real"), b.join("link")).unwrap();
    let ro = Mode {
        read: true,
        ..Mode::default()
    };
    assert!(dir.open_file(OsStr::new("link"), ro).is_err(), "symlink");
    let fifo = std::ffi::CString::new(b.join("fifo").to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(dir.open_file(OsStr::new("fifo"), ro).is_err(), "fifo");
    std::fs::create_dir(b.join("sub")).unwrap();
    assert!(dir.open_file(OsStr::new("sub"), ro).is_err(), "directory");
    // A plain file reads back.
    let mut s = String::new();
    dir.open_file(OsStr::new("real"), ro)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "x");
}

#[test]
fn only_plain_file_names_are_accepted() {
    let b = base("names");
    let dir = SafeDir::open(&b).unwrap();
    for bad in ["", ".", "..", "a/b", "../x", "/abs"] {
        assert!(
            dir.open_file(OsStr::new(bad), create_new()).is_err(),
            "{bad:?}"
        );
        assert!(
            dir.rename(OsStr::new(bad), OsStr::new("ok")).is_err(),
            "{bad:?}"
        );
        assert!(
            dir.rename(OsStr::new("ok"), OsStr::new(bad)).is_err(),
            "{bad:?}"
        );
    }
    assert!(std::fs::read_dir(&b).unwrap().next().is_none());
}

#[test]
fn create_new_does_not_overwrite_and_append_extends() {
    let b = base("modes");
    let dir = SafeDir::open(&b).unwrap();
    let name = OsStr::new("f");
    dir.open_file(name, create_new())
        .unwrap()
        .write_all(b"a")
        .unwrap();
    assert!(dir.open_file(name, create_new()).is_err());
    let app = Mode {
        append: true,
        ..Mode::default()
    };
    dir.open_file(name, app).unwrap().write_all(b"b").unwrap();
    assert_eq!(std::fs::read_to_string(b.join("f")).unwrap(), "ab");
    let (d2, n2) = SafeDir::for_file(&b.join("f")).unwrap();
    assert_eq!(n2, "f");
    assert!(d2
        .open_file(
            &n2,
            Mode {
                read: true,
                ..Mode::default()
            }
        )
        .is_ok());
}
