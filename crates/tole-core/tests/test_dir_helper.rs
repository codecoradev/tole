//! The `TestDir` guard (#781) cleans up after itself.

#[path = "support/test_dir.rs"]
mod test_dir;
use test_dir::{wait_for, wait_until, ChildGuard, TestDir, WorkDir, WAIT};

#[test]
fn removes_the_directory_on_drop() {
    let d = TestDir::new("tole-testdir-drop");
    let p = d.path().to_path_buf();
    std::fs::create_dir_all(p.join("a/b")).unwrap();
    std::fs::write(p.join("a/b/f.txt"), "x").unwrap();
    assert!(p.is_dir());
    drop(d);
    assert!(!p.exists());
}

#[test]
fn names_are_unique() {
    let a = TestDir::new("tole-testdir-uniq");
    let b = TestDir::new("tole-testdir-uniq");
    assert_ne!(a.path(), b.path());
}

#[test]
fn removes_the_directory_when_the_test_panics() {
    let p = std::sync::Mutex::new(None);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let d = TestDir::new("tole-testdir-panic");
        *p.lock().unwrap() = Some(d.path().to_path_buf());
        panic!("boom");
    }));
    assert!(r.is_err());
    let p = p.lock().unwrap().clone().unwrap();
    assert!(!p.exists());
}

#[test]
fn keep_leaves_the_directory() {
    let p = TestDir::new("tole-testdir-keep").keep();
    assert!(p.is_dir());
    std::fs::remove_dir_all(&p).unwrap();
}

#[cfg(unix)]
#[test]
fn removes_read_only_files_and_directories() {
    use std::os::unix::fs::PermissionsExt;
    let d = TestDir::new("tole-testdir-ro");
    let p = d.path().to_path_buf();
    let sub = p.join("locked");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("f"), "x").unwrap();
    std::fs::set_permissions(sub.join("f"), std::fs::Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o500)).unwrap();
    drop(d);
    assert!(!p.exists());
}

#[cfg(unix)]
#[test]
fn child_guard_kills_and_reaps_the_child() {
    let d = TestDir::new("tole-testdir-child");
    let child = std::process::Command::new("sleep")
        .arg("60")
        .current_dir(d.path())
        .spawn()
        .unwrap();
    let pid = child.id();
    let guard = ChildGuard(child);
    assert_eq!(guard.id(), pid);
    drop(guard);
    // Reaped: signal 0 to the pid now fails (no such process).
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive);
}

#[test]
fn work_dir_keeps_its_home_twin_inside_the_guard() {
    let w = WorkDir::new("tole-testdir-work");
    let home = w.with_extension("home");
    std::fs::create_dir_all(home.join("tole")).unwrap();
    assert!(w.is_dir() && home.is_dir());
    let (wp, hp) = (w.path().to_path_buf(), home);
    drop(w);
    assert!(!wp.exists() && !hp.exists());
}

#[test]
fn in_system_temp_is_under_the_system_temp_dir() {
    let d = TestDir::in_system_temp("tole-testdir-sys");
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    assert!(
        d.path().starts_with(&tmp),
        "{:?} not under {tmp:?}",
        d.path()
    );
    let p = d.path().to_path_buf();
    drop(d);
    assert!(!p.exists());
}

// The condition waits (#812).

#[test]
fn wait_until_returns_at_once_for_a_true_condition() {
    let start = std::time::Instant::now();
    wait_until(std::time::Duration::from_secs(30), "nothing", || true);
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn wait_until_waits_for_a_condition_that_becomes_true() {
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setter = {
        let flag = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    wait_until(WAIT, "the flag", || {
        flag.load(std::sync::atomic::Ordering::SeqCst)
    });
    setter.join().unwrap();
}

#[test]
fn wait_for_hands_back_the_value() {
    let mut n = 0;
    let got = wait_for(WAIT, "the third probe", || {
        n += 1;
        (n == 3).then_some(n * 10)
    });
    assert_eq!(got, 30);
}

#[test]
fn wait_until_panics_with_what_it_waited_for_and_the_bound() {
    let r = std::panic::catch_unwind(|| {
        wait_until(
            std::time::Duration::from_millis(80),
            "a file that never comes",
            || false,
        );
    });
    let msg = *r.unwrap_err().downcast::<String>().unwrap();
    assert!(msg.contains("a file that never comes"), "{msg}");
    assert!(msg.contains("80ms"), "{msg}");
}

#[test]
fn wait_until_looks_once_more_at_the_deadline() {
    // A probe that only holds after the bound has passed still wins when
    // the thread was paused past it: the last look at the deadline decides.
    let mut calls = 0;
    wait_until(std::time::Duration::ZERO, "the second look", || {
        calls += 1;
        calls >= 2
    });
}

/// Runs a script `rounds` times while three threads fork in a loop (a
/// `pre_exec` hook forces the fork+exec path instead of `posix_spawn`), and
/// returns how many runs failed with `ETXTBSY`. `write` is the way the
/// script is written.
#[cfg(target_os = "linux")]
fn etxtbsy_failures(rounds: usize, write: impl Fn(&std::path::Path, &str)) -> usize {
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    /// Stops the forking threads when it drops, also on a panic of the
    /// writer: `thread::scope` joins them before the panic propagates, so
    /// they must not wait for a store that never comes.
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let d = TestDir::new("tole-etxtbsy");
    let stop = AtomicBool::new(false);
    let mut busy = 0;
    std::thread::scope(|s| {
        for _ in 0..3 {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let mut c = std::process::Command::new("/bin/true");
                    // SAFETY: the hook does nothing; it only disables
                    // posix_spawn so the child is a real fork.
                    unsafe { c.pre_exec(|| Ok(())) };
                    let _ = c.status();
                }
            });
        }
        let _stop = StopOnDrop(&stop);
        for i in 0..rounds {
            let p = d.join(format!("s{i}"));
            write(&p, "#!/bin/sh\nexit 0\n");
            match std::process::Command::new(&p).status() {
                Ok(st) => assert!(st.success()),
                Err(e) if e.raw_os_error() == Some(26) => busy += 1,
                Err(e) => panic!("exec {}: {e}", p.display()),
            }
        }
    });
    busy
}

#[cfg(target_os = "linux")]
#[test]
fn a_failing_stress_run_stops_the_forking_threads_instead_of_hanging() {
    // The writer panics at once; the forking threads must still end, so the
    // panic reaches the test instead of the binary hanging.
    let r = std::panic::catch_unwind(|| {
        etxtbsy_failures(3, |_, _| panic!("writer failed"));
    });
    assert!(r.is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn write_executable_is_never_text_file_busy_under_concurrent_forks() {
    // The stress of #840: with the helper, no run may see ETXTBSY.
    let busy = etxtbsy_failures(400, |p, c| test_dir::write_executable(p, c));
    assert_eq!(busy, 0, "write_executable left a writer behind");
}

#[cfg(target_os = "linux")]
#[test]
fn a_plain_write_can_be_text_file_busy_under_concurrent_forks() {
    // The reproduction of #840: write + chmod by hand. The race is timing
    // dependent, so this only reports how often it happens (it must not
    // fail a run that was lucky); the test above holds the contract.
    use std::os::unix::fs::PermissionsExt;
    let busy = etxtbsy_failures(400, |p, c| {
        std::fs::write(p, c).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    eprintln!("plain write: {busy} of 400 runs were ETXTBSY");
}

#[cfg(unix)]
#[test]
fn write_executable_writes_an_executable_file_and_replaces_an_old_one() {
    use std::os::unix::fs::PermissionsExt;
    let d = TestDir::new("tole-writeexec");
    let p = d.join("run");
    test_dir::write_executable(&p, "#!/bin/sh\necho one\n");
    test_dir::write_executable(&p, "#!/bin/sh\necho two\n");
    assert_eq!(
        std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let out = std::process::Command::new(&p).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "two\n");
}
