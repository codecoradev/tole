//! #1307: shell spellings that ran the tole binary past the guard because the
//! guard's word cut and the shell's grammar disagreed. Every `ROUTES` entry runs
//! tole in bash or dash (the differential test runs them against a fake `tole`);
//! each must be refused as `sh -c` text, as a split command line, and nested in
//! `sh -c` at depths 1 to 3. `ALLOWED` is what must stay allowed next to them
//! (as `sh -c` text: nested in quotes the guard cuts naively and refuses more).
//! Not here (documented, not part of #1307): a wrapper in front of a shell's
//! `-c`, `eval`, a pipe into a shell, `x=tole; $x`, `xargs`, `find -exec`.

use std::path::Path;
use tole_core::run_command::RunCommandTool;
use tole_core::session_guard::check_session_argv;

const ROUTES: &[&str] = &[
    // An assignment with an operator, an array assignment.
    "X+=1 tole x",
    "X+=1 Y=2 tole x",
    "X[0]=1 tole x",
    // A leading redirection (the `&` of `2>&1` is no separator).
    ">/dev/null tole x",
    "> /dev/null tole x",
    "2>&1 tole x",
    "2>/dev/null tole x",
    "</dev/null tole x",
    "&>/dev/null tole x",
    ">>f tole x",
    // Reserved words and builtins before the program.
    "! tole x",
    "coproc tole x",
    "builtin command tole x",
    "builtin exec tole x",
    "if false; then :; elif tole x; then :; fi",
    "until tole x; do :; done",
    // A command substitution as the program word.
    "$(echo tole) x",
    "`echo tole` x",
    "X=1 $(echo tole) x",
    // A quoted or escaped KEY behind a wrapper, a separator inside the quotes.
    "env \"X=a;c\" tole x",
    "env 'X'='a;c' tole x",
    "env \\X='a;c' tole x",
    // A word cut at a brace.
    "X=a${IFS}b tole x",
    "env X=a}b tole x",
    "env X{=1 tole x",
    // `env -S` splits one argument into a command.
    "env -S 'tole x'",
    "env -i -S 'tole x'",
    "sudo env --split-string='tole x'",
    "env --s='tole x'",
    // A nested shell whose payload is unquoted, with escaped blanks.
    "sh -c echo\\ a\\;\\ tole\\ x",
    "sh -c \\(\\ tole\\ x\\ \\)",
    "sh -c true\\ \\&\\&\\ tole\\ x",
    "sh -c true\\ \\&\\&\\ X=a\\\\\\ b\\ tole\\ x",
];

/// Realistic commands next to the routes (`&>`, `2>&1`, `--show-output`, `!`,
/// `until`, a quoted redirection target, a substitution as an argument).
const ALLOWED: &[&str] = &[
    "cargo test &>\"$LOG\"",
    "make 2>&1 | tee out.log",
    "cargo build >&2",
    "cmd &>/dev/null",
    "echo $(date) > \"$out\"",
    "echo $(date) >out.txt",
    "X=$(date) cmd",
    "env RUST_LOG=debug cargo test -- --show-output",
    "env X=1 rg -S foo src",
    "timeout 30 grep -rn \"a=b\" src",
    "env FOO=1 BAR=2 printenv BAR",
    "! git diff --quiet",
    "if ! grep -q foo f; then echo no; fi",
    "until curl -s localhost; do sleep 1; done",
    "for f in $(ls); do echo \"$f\"; done",
    "if [ \"$(id -u)\" = 0 ]; then echo root; fi",
    "grep -E \"a|tole\" f",
    "git commit -m 'fix tole bug'",
    "sh -c 'echo a; echo b'",
    "echo a\\ b",
];

fn quote(t: &str) -> String {
    format!("'{}'", t.replace('\'', "'\\''"))
}

/// `sh -c <text>` written `depth` times (depth 0 is the text itself).
fn nest(text: &str, depth: usize) -> String {
    (0..depth).fold(text.to_string(), |t, _| format!("sh -c {}", quote(&t)))
}

fn refused(argv: &[String]) -> bool {
    check_session_argv(argv, Path::new("/tmp")).is_err()
}

fn shell(text: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), text.into()]
}

fn line(text: &str) -> Vec<String> {
    RunCommandTool::split_argv(text).expect("a command line")
}

fn check(name: &str, bad: Vec<String>) {
    assert!(bad.is_empty(), "{name}: still allowed:\n{}", bad.join("\n"));
}

#[test]
fn every_route_is_refused_as_shell_text_at_depths_0_to_3() {
    let mut bad = Vec::new();
    for r in ROUTES {
        for d in 0..=3 {
            let t = nest(r, d);
            if !refused(&shell(&t)) {
                bad.push(format!("depth {d}: {t:?}"));
            }
        }
    }
    check("sh -c text", bad);
}

#[test]
fn every_route_is_refused_as_a_command_line() {
    let mut bad = Vec::new();
    for r in ROUTES {
        for d in 0..=3 {
            let t = nest(r, d);
            if !refused(&line(&t)) {
                bad.push(format!("depth {d}: {t:?}"));
            }
        }
    }
    check("split argv", bad);
}

#[test]
fn the_realistic_neighbours_stay_allowed() {
    let bad: Vec<_> = ALLOWED
        .iter()
        .filter(|t| refused(&shell(t)))
        .map(|t| format!("{t:?}"))
        .collect();
    check("allowed as sh -c text", bad);
}

#[test]
fn an_unclosed_quote_or_escaped_blank_before_the_program_is_doubt_and_refused() {
    // The cut leaves the program behind a word that holds a blank.
    for t in [
        "X+='a b' tole x",
        "X+=a\\ b tole x",
        ">'a b' tole x",
        "> 'a b' tole x",
        ">a\\ b tole x",
        "env \"X=a b\" tole x",
    ] {
        assert!(refused(&shell(t)), "must refuse: {t:?}");
    }
}
