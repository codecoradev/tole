//! #1307: shell spellings that ran the tole binary past the guard because the
//! guard's word cut and the shell's grammar disagreed. Every `ROUTES` entry runs
//! tole in bash or dash (the differential test runs them against a fake `tole`);
//! each must be refused as `sh -c` text, as a split command line, and nested in
//! `sh -c` at depths 1 to 3. `ALLOWED` is what must stay allowed next to them
//! (as `sh -c` text: nested in quotes the guard cuts naively and refuses more).
//! Not here (open, listed in `docs/threat-model.md`): `eval`, a pipe or a
//! here-string into a shell, `x=tole; $x`, `xargs`, `find -exec`, `rg --pre`.

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
    // A brace group, a function: a lone `{` or `}` is not the program.
    "{ X=a}b tole x; }",
    "{ X=a${IFS}b tole x; }",
    "{ env X=a}b tole x; }",
    "{ env X{=1 tole x; }",
    "function f { X=a}b tole x; }; f",
    "{ { X=a}b tole x; }; }",
    "{ $(echo tole) x; }",
    // `env` anywhere in a chain.
    "env env -S 'tole x'",
    "env X=1 env -S 'tole x'",
    "nohup env env -S 'tole x'",
    "env sudo env -S 'tole x'",
    "env nohup env -S 'tole x'",
    "env -iS 'tole x'",
    // `env -S` behind an option that takes an argument.
    "env --unset FOO -S 'tole x'",
    "env --chdir /tmp -S 'tole x'",
    "env -iu FOO -S 'tole x'",
    "env -C dir -S 'tole x'",
    "env -a x -S 'tole x'",
    "env -u A -u B -S 'tole x'",
    "env -uFOO -S 'tole x'",
    // Closing a descriptor is one word; `>|` is a redirection.
    ">&- tole x",
    "3>&- tole x",
    "2>&- tole x",
    "<&- tole x",
    "3<&- tole x",
    ">|f tole x",
    ">| f tole x",
    // The program word glued to a redirection or a variable, or a variable
    // that expands to nothing in front of it.
    "tole>f x",
    "tole</dev/null x",
    "tole>&2",
    "tole<<<x",
    "tole$E x",
    "tole$IFS x",
    "tole\"$E\" x",
    "tole$@ x",
    "$E tole x",
    "$@ tole x",
    // A glob or an ANSI-C string in the program word.
    "./tol? x",
    "./to[l]e x",
    "./tol* x",
    "$PWD/tol? x",
    // A variable word or a substitution in front of the program.
    "$EMPTY$EMPTY tole x",
    "\"$@\" tole x",
    "$E>f tole x",
    "env $(echo tole) x",
    "time $(echo tole) x",
    "if $(echo tole) x; then :; fi",
    "! $(echo tole) x",
    "timeout 5 $(echo tole) x",
    // A redirection that names a descriptor.
    "{fd}>f tole x",
    "{fd}<f tole x",
    "{fd}>>f tole x",
    // A redirection glued to a wrapper or a shell.
    "time< f tole x",
    "nohup>f tole x",
    "env>f tole x",
    "sh>f -c 'tole x'",
    "bash<f -c 'tole x'",
    // A wrapper in front of a shell's `-c`, with options and assignments between.
    "env sh -c 'tole x'",
    "env -i sh -c 'tole x'",
    "env -C . sh -c 'tole x'",
    "env FOO=1 sh -c 'tole x'",
    "sudo sh -c 'tole x'",
    "sudo -u root env sh -ic 'tole x'",
    "nohup sh -c 'tole x'",
    "timeout 5 sh -c 'tole x'",
    "exec -a name sh -c 'tole x'",
    "time bash -lc 'tole x'",
    "nice -n 5 dash -xc 'tole x'",
    "stdbuf -o0 sh -c 'echo a; tole x'",
    "env env -i bash -c 'cd . && tole x'",
    "env -i sh -c '/usr/bin/tole x'",
    // Other shells and runners.
    "rbash -c 'tole x'",
    "ksh93 -c 'tole x'",
    "watch tole x",
    "strace tole x",
    "flock f tole x",
    "ionice -c3 tole x",
    "taskset 1 tole x",
    "chrt 1 tole x",
    "setpriv tole x",
    "script -qc tole /dev/null",
    // A nested shell whose payload is unquoted, with escaped blanks.
    "sh -c echo\\ a\\;\\ tole\\ x",
    "sh -c \\(\\ tole\\ x\\ \\)",
    "sh -c true\\ \\&\\&\\ tole\\ x",
    "sh -c true\\ \\&\\&\\ X=a\\\\\\ b\\ tole\\ x",
];

/// Routes that need a shell to read them: as a command line there is no brace
/// group, no continuation line, no ANSI-C string and no function to define.
const TEXT_ONLY: &[&str] = &[
    // Positionals and option letters of the wrappers (man pages).
    "timeout .5 ./tol? x",
    "timeout .5 $(echo tole) x",
    "taskset ff $(echo tole) x",
    "taskset ff ./tol? x",
    "flock 1.lock ./tol? x",
    "watch -c $(echo tole) x",
    "watch -d ./tol? x",
    "timeout>f -s KILL 5 ./tol? x",
    "strace -S calls $(echo tole) x",
    "strace -I 1 -a 40 $(echo tole) x",
    // The program behind a wrapper option the guard has to size.
    "timeout 5s $(echo tole) x",
    "timeout 5s ./tol? x",
    "timeout -s KILL 5 ./tol? x",
    "nohup timeout 5s ./tol? x",
    "timeout 5s ${E:-tole} x",
    "stdbuf -o L $(echo tole) x",
    "flock f ./tol? x",
    "strace -o f $'\\x74ole' x",
    "taskset 0x1 $(echo tole) x",
    "sudo -g wheel $(echo tole) x",
    "ionice -c 3 ./tol? x",
    "sudo -u root -g wheel $(echo tole) x",
    // The running binary under another name, and a shell named by a glob or a variable.
    "/proc/$PPID/exe x",
    "/proc/${PPID}/exe x",
    "exec /proc/$PPID/exe x",
    "env -i /proc/$PPID/exe x",
    "/bin/d?sh -c 'tole x'",
    "$0 -c 'tole x'",
    "S=sh; $S -c 'tole x'",
    "${SHELL:-sh} -c 'tole x'",
    "env $S -c 'tole x'",
    // A glob class, brace expansion, a default value: the shell builds the word.
    "./[]t]ole x",
    "./[[:lower:]]ole x",
    "./[!x]ole x",
    "to{l,x}e y",
    "${E:-tole} y",
    "${E-tole} y",
    "{tole,x} y",
    "'to'$E'le' x",
    "\"$EMPTY\"tole x",
    "true && { X=a}b tole x; }",
    "f() { X=a}b tole x; }; f",
    "tol\\\ne x",
    "to\\\nle x",
    "$'\\x74ole' x",
    "$'\\164ole' x",
    "$'t\\157le' x",
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
    "[ -f x ] && echo yes",
    "[[ -f x ]] || echo no",
    "rg \"foo|ba?\" src",
    "git log --grep \"a|b*\"",
    "./run.sh --all",
    "{ echo a; echo b; }",
    "f() { echo a; }; f",
    "echo a >&- ",
    "echo a >| out.txt",
    "$CC -o x y.c",
    // A glob or an ANSI-C string that is an argument, not the program.
    // A redirection that ends a quoted nested payload.
    "sh -c \"nohup sh -c 'make > log 2>&1' &\"",
    "sh -c 'make 2>&1'",
    "sh -c \"echo a >f\"",
    // Wrapper options that take an argument, then a program with arguments.
    "watch -n 5 ls *",
    "watch -c ls *",
    "timeout .5 ls *",
    "flock 1.lock ls *",
    "taskset ff make -j4",
    "strace -S calls ls *",
    "timeout 5 ls $(pwd)",
    "timeout 5s ls $(pwd)",
    "timeout -s KILL 5 ls *",
    "stdbuf -o L cat $(echo f)",
    "flock f ls *",
    "taskset 0x1 make -j4",
    "taskset -c 0-3 ls *",
    "sudo -g wheel ls *",
    "sudo -E ls $(pwd)",
    "strace -o f ls $(pwd)",
    "time -p ls $(pwd)",
    "nice -n 5 ls *",
    "time ls *",
    "timeout 30 grep -rn foo *",
    "env ls t*",
    "time printf $'a\\tb\\n'",
    "command ls *",
    "exec ls *",
    "! ls *",
    "$CC -c foo.c",
    "for d in a b; do ls *; done",
    "if true; then cp * y; fi",
    "env FOO=1 cargo test *",
    "sudo rm -rf ./build/*",
    "time echo $'a\\nb'",
    "${HOME}/bin/tool x",
    "echo ${HOME} ${X:-y}",
    "env X=1 rg -S foo src",
    "env -u FOO rg -S foo src",
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
    for r in ROUTES.iter().chain(TEXT_ONLY) {
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
