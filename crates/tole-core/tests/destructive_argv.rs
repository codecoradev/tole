//! Unit tests of `check_destructive_argv`, moved out of `src/` (#1249): they
//! only use the public API of the crate.
#![cfg(feature = "shell-tools")]

use tole_core::subprocess::check_destructive_argv;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn refused_cases() {
    for argv in [
        s(&["rm", "-rf", "/"]),
        s(&["rm", "--recursive", "/"]),
        s(&["rm", "-rfv", "/etc"]), // flag cluster with extra letter
        s(&["rm", "-r", "../outside"]),
        s(&["rm", "-rf", "/home/*"]),
        s(&["rm", "-rf", "./.."]),
        s(&["rm", "-rf", "~"]),
        s(&["/bin/rm", "-rf", "/"]), // path-qualified program
        s(&["dd", "if=/dev/zero", "of=/dev/sda"]),
        s(&["mkfs.ext4", "/dev/sda1"]),
        s(&["shutdown", "-h", "now"]),
        s(&["env", "dd", "if=/dev/zero", "of=/dev/sda"]), // wrapper bypass
        s(&["busybox", "mkfs.ext4", "/dev/sda"]),         // wrapper bypass
        s(&["nohup", "shutdown", "-h", "now"]),           // wrapper bypass
        s(&["sh", "-c", "rm -rf /etc"]),                  // quoted payload
        s(&["sh", "-c", "-rm -rf /etc"]),                 // dash-prefixed payload (scan-3)
        s(&["bash", "-c", "/bin/rm -rf /"]),              // dash-prefixed + path-qualified (scan-3)
        s(&["bash", "-lc", "dd if=/dev/zero of=/dev/sda"]), // quoted payload
        s(&["xargs", "-n", "1", "rm", "-rf", "/"]),       // option value derails naive scan
        s(&["env", "-C", ".", "rm", "-rf", "/"]),         // env -C dir bypass
        s(&["nohup", "sh", "-c", "dd if=/dev/zero of=/dev/sda"]), // nested sh
        s(&["env", "bash", "-c", "rm -rf /etc"]),         // nested bash
        s(&["sudo", "rm", "-rf", "/"]),                   // sudo wrapper
        s(&["doas", "rm", "-rf", "/"]),                   // doas wrapper
    ] {
        let err = check_destructive_argv(&argv)
            .err()
            .unwrap_or_else(|| panic!("must refuse: {argv:?}"));
        assert!(
            err.contains("destructive beyond the Write risk tier"),
            "{argv:?}"
        );
    }
}

#[test]
fn benign_cases_pass() {
    for argv in [
        s(&["cat", "middleware.ts"]),
        s(&["grep", "shutdown", "runbook.md"]),
        s(&["git", "add", "file"]),
        s(&["cargo", "add", "serde"]),
        s(&["echo", "useradd"]),
        s(&["which", "rm"]), // rm as ARGUMENT TEXT
        s(&["man", "rm"]),
        s(&["echo", "rm -rf is dangerous"]),   // rm inside text
        s(&["rm", "-rf", "./build"]),          // scoped, inside ws
        s(&["rm", "-rf", "./a/../build"]),     // normalizes inside
        s(&["rm", "notes.txt"]),               // non-recursive anywhere
        s(&["sh", "-c", "echo hello"]),        // benign payload
        s(&["env", "FOO=1", "echo", "hi"]),    // env with assignment
        s(&["sh", "-c", "cat middleware.ts"]), // benign payload w/ dd-like text
    ] {
        assert!(
            check_destructive_argv(&argv).is_ok(),
            "must NOT refuse: {argv:?}"
        );
    }
}
