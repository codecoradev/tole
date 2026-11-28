//! The deterministic line generator of `tests/session_guard_differential.rs`
//! (#1307): a fixed-seed xorshift picks one value per axis, so a run is the
//! same on every machine and no clock or `rand` is involved.
//!
//! A generated line is: leading assignments, redirections, a wrapper, the program word and
//! one argument form one simple command; a context puts it behind or between
//! other commands (separators, braces, a subshell, `if`); up to two layers of
//! `sh -c '...'` wrap it, each quoted with single quotes, double quotes or
//! backslashes. A line that starts with a shell is also emitted as a command
//! line (split like `run_command`, run without an outer shell). `@FAKE@` is
//! the directory of the fake `tole`.

use crate::guard_corpus::Input;
use std::collections::HashSet;

const ASSIGNS: &[&str] = &[
    "",
    "X=1 ",
    "X= ",
    "X='a b' ",
    "X=\"a b\" ",
    "X=a\\ b ",
    "X=$'a b' ",
    "X='a;c' ",
    "X=$(echo a) ",
    "X=1 Y=2 ",
    "X=a}b ",
    "X=a${IFS}b ",
    "X+=1 ",
    ">/dev/null ",
    "2>&1 ",
    ">&- ",
    "3>&- ",
    ">|/dev/null ",
    "$NOPE ",
    "! ",
];
const WRAPPERS: &[&str] = &[
    "",
    "env ",
    "env -i ",
    "env X=1 ",
    "env X='a b' ",
    "nohup ",
    "exec ",
    "sudo ",
    "time ",
    "command ",
    "nice ",
    "timeout 5 ",
    "env env ",
    "env X=1 env ",
    "nohup env ",
    "coproc ",
    "builtin command ",
    "ionice -c3 ",
    "time</dev/null ",
    "env>/dev/null ",
];
const PROGRAMS: &[&str] = &[
    "tole",
    "@FAKE@/tole",
    "./tole",
    "\"tole\"",
    "'tole'",
    "t\\ole",
    "tole-cli",
    "tole>/dev/null",
    "tole$NOPE",
    "tole\"$NOPE\"",
    "tol\\\ne",
    "$'\\x74ole'",
    "./tol?",
    "./to[l]e",
    "$PWD/tol*",
    "$NOPE tole",
];
/// `{}` is the simple command.
const CONTEXTS: &[&str] = &[
    "{}",
    "echo a; {}",
    "{}; echo a",
    "true && {}",
    "false || {}",
    "echo a | {}",
    "{} | cat",
    "echo a & {}",
    "{} &",
    "echo a\n{}",
    "{ {}; }",
    "( {} )",
    "if true; then {}; fi",
    "grep a f; {}",
    "grep \"a|b\" f | {}",
    "echo a;{}",
    "cd . && {}",
    "echo \"{}\"",
    "f() { {}; }; f",
    "function f { {}; }; f",
    "true && { {}; }",
];
/// Not `env sh -c` and the like: a wrapper in front of a shell is a documented
/// route listed in `KNOWN_RESIDUAL`, and mixing it in only repeats that gap.
const SHELLS: &[&str] = &["sh", "bash"];

#[derive(Clone, Copy)]
enum Quote {
    Single,
    Double,
    Backslash,
}

fn quote(text: &str, how: Quote) -> String {
    match how {
        Quote::Single => format!("'{}'", text.replace('\'', "'\\''")),
        Quote::Double => {
            let mut out = String::from("\"");
            for c in text.chars() {
                if matches!(c, '\\' | '"' | '$' | '`') {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
            out
        }
        Quote::Backslash => text
            .chars()
            .map(|c| match c {
                // A backslash before a newline is a continuation: quote it.
                '\n' => "'\n'".to_string(),
                c if c.is_ascii_alphanumeric() || "_-./=,:@".contains(c) => c.to_string(),
                c => format!("\\{c}"),
            })
            .collect(),
    }
}

struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
        &of[(self.next() % of.len() as u64) as usize]
    }
}

/// `count` distinct `sh -c` texts (fewer if the draws repeat too often) and the
/// command-line form of those that start with a shell, the same on every run.
pub fn generate(count: usize) -> Vec<Input> {
    let mut rng = Xorshift(0x9E37_79B9_7F4A_7C15);
    let (mut seen, mut out) = (HashSet::new(), Vec::new());
    let mut draws = 0;
    while seen.len() < count && draws < count * 20 {
        draws += 1;
        let cmd = format!(
            "{}{}{} x",
            rng.pick(ASSIGNS),
            rng.pick(WRAPPERS),
            rng.pick(PROGRAMS)
        );
        let mut line = rng.pick(CONTEXTS).replace("{}", &cmd);
        // Depth 0 for 3 in 10 lines, 1 for 4, 2 for 3.
        let depth = match rng.next() % 10 {
            0..=2 => 0,
            3..=6 => 1,
            _ => 2,
        };
        for _ in 0..depth {
            let how = *rng.pick(&[Quote::Single, Quote::Double, Quote::Backslash]);
            line = format!("{} -c {}", rng.pick(SHELLS), quote(&line, how));
        }
        if seen.insert(line.clone()) {
            if depth > 0 {
                out.push(Input::line(&line));
            }
            out.push(Input::sh(&line));
        }
    }
    out
}
