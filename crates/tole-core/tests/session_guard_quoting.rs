//! #1287: the session guard (#684) must not refuse a quoted argument that
//! only mentions `tole` (`grep -E "a|tole"`), and must keep refusing every way
//! of running the binary, quoted or not. Both directions are tested: the
//! allowed table is the false positive, the refused tables are the attempts to
//! hide a real invocation behind quoting.

use std::path::Path;
use tole_core::session_guard::check_session_argv;

/// A command line, split into an argv the way `run_command` does.
fn line(l: &str) -> bool {
    let argv = tole_core::run_command::RunCommandTool::split_argv(l).unwrap();
    check_session_argv(&argv, Path::new("/tmp")).is_err()
}

/// Shell text handed to `sh -c`, as one argument (nothing is unquoted for it).
fn shell(text: &str) -> bool {
    check_session_argv(&["sh".into(), "-c".into(), text.into()], Path::new("/tmp")).is_err()
}

#[test]
fn a_quoted_pattern_that_mentions_tole_passes() {
    // The refused call of the issue, verbatim.
    assert!(!line(
        "bash -c 'ls \"$HOME/.bun/install/global/node_modules\" | grep -i -E \"codecora|cora|tole|opencode|gemini\"'"
    ));
    for line_ in [
        "grep -E 'a|tole' file",
        "rg \"tole\" src",
        "sh -c \"grep -E 'a|tole' file\"",
        "bash -c 'cat x | grep -E \"a;tole|b&tole\"'",
        "bash -lc 'grep -rn \"(tole)\" src && echo done'",
        "sh -c 'FOO=1 grep -E \"a|tole\" f'",
    ] {
        assert!(!line(line_), "must allow: {line_}");
    }
    for text in [
        "grep -E \"a|tole\" f",
        "rg \"a|tole|b\" src",
        "grep 'a|tole' f && echo done",
        "ls \"$HOME/x\" | grep -i -E \"cora|tole|opencode\"",
        "cat \"a|tole\"",
        "head -n1 \"a;tole\" | wc -l",
        "tail 'x&tole' ; ls",
        "grep \"x{tole}\" f",
        "grep \"(tole)\" f",
        "grep \"a\ntole x\" f",
        "grep 'a\\bc|tole' f",
        "ls 'a'\"b|tole\"'c;d'",
        "echo \"x\" ; cat \"a|tole\"; grep \"b&tole\" f",
    ] {
        assert!(!shell(text), "must allow: {text:?}");
    }
}

#[test]
fn a_real_command_next_to_a_quoted_pattern_is_refused() {
    for text in [
        "grep -E \"a|tole\" f | tole x",
        "grep -E \"a|tole\" f; tole x",
        "grep -E \"a|tole\" f && tole cron resume j",
        "grep -E \"a|tole\" f || tole x",
        "grep -E \"a|tole\" f & tole x",
        "grep -E \"a|tole\" f\ntole x",
        "grep \"a\nb\" f\ntole x",
        "tole x | grep \"a|b\"",
        "echo \"a|b\"; tole x",
        "echo 'a|b' | tole x",
        "echo \"a|tole\" ; tole x",
        "echo 'x' \"y;z\" ; tole x",
        "echo \"a\" 'b' ; tole x",
        "{ grep \"a\" f; }; tole x",
        "(grep \"a|b\" f; tole x)",
        "if grep -q \"a|b\" f; then tole x; fi",
        "grep \"a|b\" f | env tole x",
        "grep \"a|b\" f | sudo tole x",
        "grep \"a|b\" f | command tole x",
        "grep \"a|b\" f | exec tole x",
        "grep \"a|b\" f && \"tole\" x",
        "grep \"a|b\" f && 'tole' x",
        "grep \"a|b\" f && t'ol'e x",
        "grep \"a|b\" f && to\"\"le x",
        "grep \"a|b\" f && \\tole x",
        "grep \"a|b\" f && ./tole x",
        "grep \"a|b\" f && /usr/local/bin/tole x",
        "grep \"a|b\" f && tole-cli x",
        "grep \"a|b\" f && env -i tole x",
        "grep \"a|b\" f && nohup tole x",
        "grep \"a|b\" f && FOO=1 tole x",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
}

#[test]
fn a_substitution_around_a_quoted_pattern_is_refused() {
    for text in [
        "x=\"$(tole cron list)\"",
        "echo \"$(tole x)\"",
        "echo \"a|$(tole x)\"",
        "grep \"$(tole x)|a\" f",
        "grep \"a|$(echo b; tole x)\" f",
        "ls \"$(tole x)\"",
        "echo \"`tole x`\"",
        "grep \"a|`tole x`\" f",
        "grep $(tole x) f",
        "grep -f <(tole x) f",
        "grep `tole x` f",
        "grep \"${x:-$(tole x)}\" f",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
}

#[test]
fn quoting_tricks_cannot_hide_a_separator() {
    for text in [
        // An escaped or quoted quote character must not open a string.
        "echo \\\" ; tole x",
        "echo \\' ; tole x",
        "echo '\\' ; tole x",
        "echo \"\\\"\" ; tole x",
        "echo \"a\\\\\" ; tole x",
        "echo $'\\'' ; tole cron resume j",
        "echo $'a\\'b; tole x; echo '''",
        // fish treats a backslash in single quotes as an escape.
        "echo 'a\\'b'; tole x; echo \"'\" \"x\"",
        // A comment or here-document body is not quoting. Each of these
        // starts with an inert program and has balanced apostrophes, so only
        // the scan's own fallback keeps `tole x` from being read as quoted.
        "grep a f # it's\ntole x\necho don't",
        "grep a <<EOF\ngrep it's\nEOF\ntole x\necho don't",
        "grep a <<<x; tole x; echo 'don't'",
        // Refused on purpose, though the text may be only data: a shell
        // without `$'..'` (dash), and fish (a backslash escapes in single
        // quotes), read it differently from bash.
        "grep $'a;tole x' f",
        "grep 'a\\\\|tole' f",
        // A comment or here-document body is not quoting.
        "# it's\ntole x\necho 'x'",
        "grep a f # it's\ntole x\necho 'x'",
        "cat <<EOF\nit's\nEOF\ntole x\necho 'y'",
        "cat <<<\"a\"; tole x; echo 'y'",
        "echo ${#x}; tole x",
        "echo a#b; tole x",
        // Unbalanced quotes: refuse rather than guess.
        "grep \"a|b f; tole x",
        "grep 'a|b f; tole x",
        "grep \"a|b\" f ' ; tole x",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
}

#[test]
fn a_program_that_runs_its_quoted_argument_keeps_the_naive_split() {
    // Only the arguments of an inert program (grep, ls, ...) are data. Any
    // other program may run the text, so its quoted separators still split.
    for text in [
        "eval \"a;tole x\"",
        "eval 'a; tole x'",
        "echo \"a;tole x\" | sh",
        "echo \"a|tole x\" | bash",
        "env sh -c \"a;tole x\"",
        "sudo sh -c \"a;tole x\"",
        "nohup sh -c 'a|tole x'",
        "sh -c \"a|tole x\"",
        "sh -c 'sh -c \"a|tole x\"'",
        "sh -c 'bash -c \"grep a f|tole x\"'",
        "sh -c 'env sh -c \"a;tole x\"'",
        "python3 -c \"import os; os.system('tole x')\"",
        "awk 'BEGIN{system(\"tole x\")}'",
        "xargs -I{} tole {}",
        "echo x | xargs -I{} tole {}",
        "grep \"a\" f | xargs -I{} sh -c \"a;tole {}\"",
    ] {
        assert!(shell(text), "must refuse: {text:?}");
    }
    // Wrappers in front of an inert program are not inert themselves.
    assert!(line("sh -c \"sudo sh -c 'a;tole x'\""));
    assert!(line("bash -c \"bash -c 'tole cron resume j'\""));
}

#[test]
fn the_binary_is_still_refused_as_an_argv() {
    for l in [
        "tole cron resume j",
        "./tole x",
        "/path/tole x",
        "\"tole\" cron",
        "'tole' x",
        "t'ol'e x",
        "to\"\"le x",
        "\\tole x",
        "command tole x",
        "exec tole x",
        "env -i tole x",
        "env FOO=1 tole x",
        "sudo tole x",
        "sudo -u root tole x",
        "time tole x",
        "FOO=1 BAR=2 tole x",
        "$(tole cron list)",
        "bash -c 'x=$(tole x)'",
        "sh -c 'echo \"a\" | tole cron resume j'",
        "sh -c 'echo \"a\" ; \"tole\" x'",
        "bash -c \"grep -E 'a|b' f && tole x\"",
        "sh -c '$0 cron resume j' tole",
    ] {
        assert!(line(l), "must refuse: {l}");
    }
}

#[cfg(unix)]
#[test]
fn a_link_to_the_running_executable_is_still_refused_next_to_quotes() {
    let dir = std::env::temp_dir().join(format!("tole-guard-quote-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let helper = dir.join("helper");
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &helper).unwrap();
    let run = |text: &str| {
        let argv = ["sh".into(), "-c".into(), text.into()];
        check_session_argv(&argv, &dir).is_err()
    };
    let abs = helper.display();
    assert!(run("grep \"a|b\" f | ./helper x"));
    assert!(run("grep -E \"a|tole\" f; ./helper x"));
    assert!(run(&format!("echo \"a|b\" && {abs} x")));
    assert!(run(&format!("grep \"a;b\" f | env {abs} x")));
    assert!(run("sh -c \"a|./helper x\""));
    // Mentioning the link in a quoted pattern is not running it.
    assert!(!run("grep -E \"a|./helper\" f"));
    let _ = std::fs::remove_dir_all(&dir);
}
